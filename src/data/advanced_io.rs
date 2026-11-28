use crate::error::{ArgosError, Result};
use crate::types::IoMode;
use crate::util::{ensure_private_dir, read_to_vec};
use io_uring::{opcode, squeue, types, IoUring, Probe};
use memmap2::MmapOptions;
use parking_lot::Mutex;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

const ALIGN: usize = 4096;
const MAX_SHARD_IO_BYTES: usize = 256 * 1024 * 1024;
const MAX_IDLE_RINGS: usize = 16;
const AUTO_URING_MIN_BYTES: usize = 128 * 1024;

fn uring_disabled_by_operator() -> bool {
    static DISABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DISABLED.get_or_init(|| std::env::var_os("ARGOSFS_DISABLE_IO_URING").is_some_and(|v| v == "1"))
}

fn prefer_uring(mode: IoMode, bytes: usize) -> bool {
    !uring_disabled_by_operator()
        && (mode == IoMode::IoUring || (mode == IoMode::Buffered && bytes >= AUTO_URING_MIN_BYTES))
}

fn probe_ring(ring: &IoUring) -> std::io::Result<()> {
    let mut probe = Probe::new();
    ring.submitter().register_probe(&mut probe)?;
    if !probe.is_supported(opcode::Read::CODE) || !probe.is_supported(opcode::Write::CODE) {
        return Err(std::io::Error::from_raw_os_error(libc::EOPNOTSUPP));
    }
    Ok(())
}

// Shard reads can run on short-lived scoped threads. A shared idle pool,
// rather than thread-local rings, also amortizes setup across those threads.
// Each checkout has exactly one owner and returns only a fully drained ring.
struct RingPool {
    idle: Mutex<Vec<IoUring>>,
    disabled: AtomicBool,
}

impl RingPool {
    const fn new() -> Self {
        Self {
            idle: Mutex::new(Vec::new()),
            disabled: AtomicBool::new(false),
        }
    }

    fn acquire(&self) -> Result<RingLease<'_>> {
        if uring_disabled_by_operator() || self.disabled.load(Ordering::Acquire) {
            return Err(ArgosError::Unsupported("io_uring unavailable".into()));
        }
        let idle = self.idle.lock().pop();
        let ring = match idle {
            Some(ring) => ring,
            None => {
                let ring = IoUring::new(8).map_err(|err| {
                    if matches!(
                        err.raw_os_error(),
                        Some(
                            libc::ENOSYS
                                | libc::EPERM
                                | libc::EACCES
                                | libc::EOPNOTSUPP
                                | libc::EINVAL
                        )
                    ) {
                        self.disable();
                    }
                    ArgosError::Unsupported(format!("io_uring setup: {err}"))
                })?;
                if let Err(err) = probe_ring(&ring) {
                    // Old kernels can create a ring without supporting the
                    // non-vectored READ/WRITE commands. Never submit on it.
                    self.disable();
                    return Err(ArgosError::Unsupported(format!(
                        "io_uring opcode probe: {err}"
                    )));
                }
                ring
            }
        };
        Ok(RingLease {
            pool: self,
            ring: Some(ring),
            reusable: true,
        })
    }

    fn disable(&self) {
        self.disabled.store(true, Ordering::Release);
        self.idle.lock().clear();
    }
}

static RINGS: RingPool = RingPool::new();

struct RingLease<'a> {
    pool: &'a RingPool,
    ring: Option<IoUring>,
    reusable: bool,
}

impl Drop for RingLease<'_> {
    fn drop(&mut self) {
        if !self.reusable {
            return;
        }
        let mut idle = self.pool.idle.lock();
        if !self.pool.disabled.load(Ordering::Acquire) && idle.len() < MAX_IDLE_RINGS {
            if let Some(ring) = self.ring.take() {
                idle.push(ring);
            }
        }
    }
}

// An SQE contains a raw userspace pointer. Never release or reuse its
// allocation before observing the completion, even if io_uring_enter fails.
// A fatal enter error disables the acceleration path. Try synchronous
// cancellation before dropping the ring; if the kernel cannot confirm that
// the request has stopped, retain the affected allocation for process lifetime.
struct PendingBuffer<'a> {
    buffer: &'a mut Vec<u8>,
    pending: bool,
}

impl Drop for PendingBuffer<'_> {
    fn drop(&mut self) {
        if self.pending {
            std::mem::forget(std::mem::take(self.buffer));
        }
    }
}

impl RingLease<'_> {
    fn complete(&mut self, entry: squeue::Entry, buffer: &mut Vec<u8>) -> Result<usize> {
        let ring = self.ring.as_mut().expect("checked out ring");
        let mut guard = PendingBuffer {
            buffer,
            pending: false,
        };
        // SAFETY: the caller builds the SQE from this owned allocation. The
        // guard preserves it until CQE observation (including unwind/error).
        unsafe {
            ring.submission()
                .push(&entry)
                .map_err(|_| ArgosError::Invalid("io_uring submission queue is full".into()))?;
        }
        self.reusable = false;
        guard.pending = true;
        loop {
            if let Some(cqe) = ring.completion().next() {
                guard.pending = false;
                self.reusable = true;
                let result = cqe.result();
                return if result < 0 {
                    Err(ArgosError::Io(std::io::Error::from_raw_os_error(-result)))
                } else {
                    Ok(result as usize)
                };
            }
            match ring.submit_and_wait(1) {
                Ok(_) => {}
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => {}
                Err(err) => {
                    // The kernel may still own the pointer. Do not replay
                    // this write via the buffered path or recycle this ring.
                    self.pool.disable();
                    let cancelled = ring.submitter().register_sync_cancel(
                        Some(types::Timespec::new().sec(1)),
                        types::CancelBuilder::any(),
                    );
                    // A non-SQPOLL ring will never submit its unconsumed SQEs
                    // again after this branch. Successful synchronous cancel
                    // (or no in-flight request) makes the buffer safe to free.
                    if cancelled.is_ok()
                        || cancelled.is_err_and(|error| error.raw_os_error() == Some(libc::ENOENT))
                    {
                        guard.pending = false;
                    }
                    return Err(ArgosError::Io(err));
                }
            }
        }
    }
}

pub fn write_all(path: &Path, data: &[u8], mode: IoMode) -> Result<()> {
    if let Some(parent) = path.parent() {
        ensure_private_dir(parent)?;
    }
    if prefer_uring(mode, data.len()) {
        return match write_iouring(path, data) {
            Err(ArgosError::Unsupported(_)) => write_buffered(path, data),
            result => result,
        };
    }
    match mode {
        IoMode::Direct => write_direct(path, data).or_else(|_| write_buffered(path, data)),
        IoMode::Buffered | IoMode::IoUring => write_buffered(path, data),
    }
}

pub fn read_all(
    path: &Path,
    expected_size: usize,
    mode: IoMode,
    zero_copy: bool,
) -> Result<Vec<u8>> {
    if prefer_uring(mode, expected_size) {
        return match read_iouring(path, expected_size) {
            Err(ArgosError::Unsupported(_)) => read_mmap_or_buffered(path, zero_copy),
            result => result,
        };
    }
    match mode {
        IoMode::Direct => {
            read_direct(path, expected_size).or_else(|_| read_mmap_or_buffered(path, zero_copy))
        }
        IoMode::Buffered | IoMode::IoUring => read_mmap_or_buffered(path, zero_copy),
    }
}

pub fn io_uring_available() -> bool {
    RINGS.acquire().is_ok()
}

pub fn current_numa_node() -> Option<i32> {
    let cpu = unsafe { libc::sched_getcpu() };
    if cpu < 0 {
        return None;
    }
    for entry in fs::read_dir("/sys/devices/system/node").ok()? {
        let entry = entry.ok()?;
        let name = entry.file_name().to_string_lossy().to_string();
        if !name.starts_with("node") {
            continue;
        }
        let cpulist = fs::read_to_string(entry.path().join("cpulist")).ok()?;
        if cpu_list_contains(&cpulist, cpu as u32) {
            return name.strip_prefix("node")?.parse::<i32>().ok();
        }
    }
    None
}

fn write_buffered(path: &Path, data: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(data)?;
    file.sync_all()?;
    Ok(())
}

fn read_mmap_or_buffered(path: &Path, zero_copy: bool) -> Result<Vec<u8>> {
    let file_size = usize::try_from(fs::metadata(path)?.len())
        .map_err(|_| ArgosError::FileTooLarge(path.display().to_string()))?;
    if file_size > MAX_SHARD_IO_BYTES {
        return Err(ArgosError::FileTooLarge(format!(
            "shard {} is {file_size} bytes, limit is {MAX_SHARD_IO_BYTES}",
            path.display()
        )));
    }
    if zero_copy {
        let file = File::open(path)?;
        let len = file.metadata()?.len();
        if len > 0 {
            let mmap = unsafe { MmapOptions::new().map(&file)? };
            return Ok(mmap.as_ref().to_vec());
        }
    }
    read_to_vec(path)
}

fn write_iouring(path: &Path, data: &[u8]) -> Result<()> {
    let mut ring = RINGS.acquire()?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    let fd = file.as_raw_fd();
    // Own the submitted memory so an indeterminate kernel error cannot
    // leave the kernel referencing a caller-owned slice.
    let mut owned = Vec::new();
    owned
        .try_reserve_exact(data.len())
        .map_err(|_| ArgosError::Io(std::io::Error::from_raw_os_error(libc::ENOMEM)))?;
    owned.extend_from_slice(data);
    let mut data = owned;
    let mut written = 0usize;
    while written < data.len() {
        let entry = opcode::Write::new(
            types::Fd(fd),
            data[written..].as_ptr(),
            (data.len() - written).min(i32::MAX as usize) as u32,
        )
        .offset(written as u64)
        .build()
        .user_data(1);
        let count = ring.complete(entry, &mut data)?;
        if count == 0 {
            return Err(ArgosError::Io(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "short io_uring write",
            )));
        }
        written += count;
    }
    file.sync_all()?;
    Ok(())
}

fn read_iouring(path: &Path, expected_size: usize) -> Result<Vec<u8>> {
    let file = File::open(path)?;
    let file_size = usize::try_from(file.metadata()?.len())
        .map_err(|_| ArgosError::Invalid("file is too large to read".to_string()))?;
    let size = expected_size.max(file_size);
    if size > MAX_SHARD_IO_BYTES {
        return Err(ArgosError::FileTooLarge(format!(
            "io_uring shard read requires {size} bytes, limit is {MAX_SHARD_IO_BYTES}"
        )));
    }
    if size == 0 {
        return Ok(Vec::new());
    }
    let mut ring = RINGS.acquire()?;
    let mut data = Vec::new();
    data.try_reserve_exact(size)
        .map_err(|_| ArgosError::Io(std::io::Error::from_raw_os_error(libc::ENOMEM)))?;
    data.resize(size, 0);
    let fd = file.as_raw_fd();
    let mut read_total = 0usize;
    while read_total < size {
        let entry = opcode::Read::new(
            types::Fd(fd),
            data[read_total..].as_mut_ptr(),
            (size - read_total) as u32,
        )
        .offset(read_total as u64)
        .build()
        .user_data(1);
        let count = ring.complete(entry, &mut data)?;
        if count == 0 {
            break;
        }
        read_total += count;
    }
    data.truncate(read_total);
    Ok(data)
}

fn write_direct(path: &Path, data: &[u8]) -> Result<()> {
    if data.is_empty() || !data.len().is_multiple_of(ALIGN) {
        return Err(ArgosError::Unsupported(
            "O_DIRECT requires aligned length".to_string(),
        ));
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .custom_flags(libc::O_DIRECT)
        .mode(0o600)
        .open(path)?;
    let mut aligned = AlignedBuf::new(data.len())?;
    aligned.as_mut_slice().copy_from_slice(data);
    let mut written_total = 0usize;
    while written_total < data.len() {
        let written = unsafe {
            libc::pwrite(
                file.as_raw_fd(),
                aligned.ptr.add(written_total).cast(),
                data.len() - written_total,
                written_total as libc::off_t,
            )
        };
        if written < 0 {
            return Err(ArgosError::Io(std::io::Error::last_os_error()));
        }
        if written == 0 {
            return Err(ArgosError::Io(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "short O_DIRECT write",
            )));
        }
        written_total = written_total.saturating_add(written as usize);
    }
    file.sync_all()?;
    Ok(())
}

fn read_direct(path: &Path, expected_size: usize) -> Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECT)
        .open(path)?;
    let file_size = usize::try_from(file.metadata()?.len())
        .map_err(|_| ArgosError::Invalid("file is too large to read".to_string()))?;
    let read_size = expected_size.max(file_size);
    if read_size > MAX_SHARD_IO_BYTES {
        return Err(ArgosError::FileTooLarge(format!(
            "direct shard read requires {read_size} bytes, limit is {MAX_SHARD_IO_BYTES}"
        )));
    }
    if read_size == 0 || !read_size.is_multiple_of(ALIGN) {
        return Err(ArgosError::Unsupported(
            "O_DIRECT requires aligned length".to_string(),
        ));
    }
    let aligned = AlignedBuf::new(read_size)?;
    let mut read_total = 0usize;
    while read_total < read_size {
        let read = unsafe {
            libc::pread(
                file.as_raw_fd(),
                aligned.ptr.add(read_total).cast(),
                read_size - read_total,
                read_total as libc::off_t,
            )
        };
        if read < 0 {
            return Err(ArgosError::Io(std::io::Error::last_os_error()));
        }
        if read == 0 {
            break;
        }
        read_total = read_total.saturating_add(read as usize);
        if read_total < read_size && !(read_total.is_multiple_of(ALIGN)) {
            break;
        }
    }
    Ok(aligned.as_slice()[..read_total].to_vec())
}

struct AlignedBuf {
    ptr: *mut u8,
    len: usize,
}

impl AlignedBuf {
    fn new(len: usize) -> Result<Self> {
        let mut ptr = std::ptr::null_mut();
        let rc = unsafe { libc::posix_memalign(&mut ptr, ALIGN, len) };
        if rc != 0 {
            return Err(ArgosError::Io(std::io::Error::from_raw_os_error(rc)));
        }
        Ok(Self {
            ptr: ptr.cast(),
            len,
        })
    }

    fn as_slice(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
    }

    fn as_mut_slice(&mut self) -> &mut [u8] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
    }
}

impl Drop for AlignedBuf {
    fn drop(&mut self) {
        unsafe { libc::free(self.ptr.cast()) }
    }
}

fn cpu_list_contains(spec: &str, cpu: u32) -> bool {
    spec.trim().split(',').any(|part| {
        let part = part.trim();
        if let Some((start, end)) = part.split_once('-') {
            let Ok(start) = start.parse::<u32>() else {
                return false;
            };
            let Ok(end) = end.parse::<u32>() else {
                return false;
            };
            cpu >= start && cpu <= end
        } else {
            part.parse::<u32>().is_ok_and(|value| value == cpu)
        }
    })
}

#[cfg(test)]
#[path = "advanced_io_tests.rs"]
mod tests;
