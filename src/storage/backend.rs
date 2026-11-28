use crate::error::{ArgosError, Result};
use crate::types::{BackendKind, DiskStatus};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{Seek, SeekFrom};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, MetadataExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub type DeviceId = String;

#[derive(Clone, Debug)]
pub struct BackendCapabilities {
    pub direct_io: bool,
    pub fallocate: bool,
    pub discard: bool,
}

#[derive(Clone, Debug)]
pub struct BackendDeviceInfo {
    pub device_id: DeviceId,
    pub path: PathBuf,
    pub capacity: u64,
    pub status: DiskStatus,
}

pub trait StorageBackend: Send + Sync {
    fn backend_kind(&self) -> BackendKind;
    fn list_devices(&self) -> Result<Vec<BackendDeviceInfo>>;
    fn read_at(&self, device_id: &DeviceId, offset: u64, buf: &mut [u8]) -> Result<()>;
    /// Read scattered shard payloads without speculative readahead where supported.
    fn read_shard_at(&self, device_id: &DeviceId, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.read_at(device_id, offset, buf)
    }
    /// Transfer ownership of a read buffer so deadline workers can return it
    /// directly instead of copying completed payloads into a second buffer.
    fn read_shard_owned(
        &self,
        device_id: &DeviceId,
        offset: u64,
        mut buf: Vec<u8>,
    ) -> Result<Vec<u8>> {
        self.read_shard_at(device_id, offset, &mut buf)?;
        Ok(buf)
    }
    /// Read an explicitly sized checkpoint range without speculative readahead.
    /// Checkpoint readers already batch contiguous pages themselves.
    fn read_recovery_at(&self, device_id: &DeviceId, offset: u64, buf: &mut [u8]) -> Result<()> {
        self.read_shard_at(device_id, offset, buf)
    }
    fn write_at(&self, device_id: &DeviceId, offset: u64, data: &[u8]) -> Result<()>;
    /// Write ordered, independently positioned ranges. A successful return
    /// covers every range; on error a prefix may have been written. The caller
    /// owns allocation rollback and the durability barrier.
    fn write_batch(&self, device_id: &DeviceId, writes: &[(u64, &[u8])]) -> Result<()> {
        for (offset, data) in writes {
            self.write_at(device_id, *offset, data)?;
        }
        Ok(())
    }
    fn flush_device(&self, device_id: &DeviceId) -> Result<()>;
    fn flush_all(&self) -> Result<()>;
    /// Drop cached pages for a range before a verification read.  The default
    /// is a no-op; block backends override it so a read-after-write check does
    /// not compare against the operating system's page cache.
    fn drop_cache(&self, _device_id: &DeviceId, _offset: u64, _length: u64) -> Result<()> {
        Err(ArgosError::Unsupported(
            "cache eviction is unavailable".to_string(),
        ))
    }
    fn capacity(&self, device_id: &DeviceId) -> Result<u64>;
    fn device_status(&self, device_id: &DeviceId) -> Result<DiskStatus>;
    fn capabilities(&self) -> BackendCapabilities;
}

#[derive(Clone)]
pub struct HostFsBackend {
    root: PathBuf,
}

impl HostFsBackend {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
        }
    }
}

impl StorageBackend for HostFsBackend {
    fn backend_kind(&self) -> BackendKind {
        BackendKind::Host
    }

    fn list_devices(&self) -> Result<Vec<BackendDeviceInfo>> {
        Ok(Vec::new())
    }

    fn read_at(&self, _device_id: &DeviceId, _offset: u64, _buf: &mut [u8]) -> Result<()> {
        Err(ArgosError::Unsupported(format!(
            "host backend at {} does not expose raw read_at",
            self.root.display()
        )))
    }

    fn write_at(&self, _device_id: &DeviceId, _offset: u64, _data: &[u8]) -> Result<()> {
        Err(ArgosError::Unsupported(format!(
            "host backend at {} does not expose raw write_at",
            self.root.display()
        )))
    }

    fn flush_device(&self, _device_id: &DeviceId) -> Result<()> {
        Ok(())
    }

    fn flush_all(&self) -> Result<()> {
        Ok(())
    }

    fn capacity(&self, _device_id: &DeviceId) -> Result<u64> {
        Err(ArgosError::Unsupported(
            "host backend capacity is tracked in metadata".to_string(),
        ))
    }

    fn device_status(&self, _device_id: &DeviceId) -> Result<DiskStatus> {
        Ok(DiskStatus::Online)
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            direct_io: false,
            fallocate: true,
            discard: false,
        }
    }
}

#[derive(Clone)]
pub struct FileBlockBackend {
    kind: BackendKind,
    devices: Arc<BTreeMap<DeviceId, BlockDevice>>,
}

#[derive(Debug)]
struct BlockDevice {
    path: PathBuf,
    file: File,
    /// Independent read-only handle for scattered shard reads.
    shard_file: Option<File>,
    capacity: u64,
}

impl FileBlockBackend {
    pub fn open_loop(paths: &[PathBuf], write: bool) -> Result<Self> {
        Self::open(paths, BackendKind::LoopBlock, write)
    }

    pub fn open_raw(paths: &[PathBuf], write: bool) -> Result<Self> {
        Self::open(paths, BackendKind::RawBlock, write)
    }

    pub fn open_with_ids(
        kind: BackendKind,
        devices: Vec<(DeviceId, PathBuf)>,
        write: bool,
    ) -> Result<Self> {
        let random_data_reads = kind == BackendKind::RawBlock;
        let mut map = BTreeMap::new();
        for (device_id, path) in devices {
            let device = open_block_device(&path, write, random_data_reads)?;
            map.insert(device_id, device);
        }
        Ok(Self {
            kind,
            devices: Arc::new(map),
        })
    }

    fn open(paths: &[PathBuf], kind: BackendKind, write: bool) -> Result<Self> {
        let random_data_reads = kind == BackendKind::RawBlock;
        let mut map = BTreeMap::new();
        for (index, path) in paths.iter().enumerate() {
            let id = format!("disk-{index:04}");
            let device = open_block_device(path, write, random_data_reads)?;
            map.insert(id, device);
        }
        Ok(Self {
            kind,
            devices: Arc::new(map),
        })
    }

    pub fn paths(&self) -> Vec<PathBuf> {
        self.devices
            .values()
            .map(|device| device.path.clone())
            .collect()
    }

    fn device(&self, device_id: &DeviceId) -> Result<&BlockDevice> {
        self.devices
            .get(device_id)
            .ok_or_else(|| ArgosError::MissingDevice(device_id.clone()))
    }
}

impl StorageBackend for FileBlockBackend {
    fn backend_kind(&self) -> BackendKind {
        self.kind
    }

    fn list_devices(&self) -> Result<Vec<BackendDeviceInfo>> {
        Ok(self
            .devices
            .iter()
            .map(|(device_id, device)| BackendDeviceInfo {
                device_id: device_id.clone(),
                path: device.path.clone(),
                capacity: device.capacity,
                status: DiskStatus::Online,
            })
            .collect())
    }

    fn read_at(&self, device_id: &DeviceId, offset: u64, buf: &mut [u8]) -> Result<()> {
        read_exact_at(&self.device(device_id)?.file, device_id, offset, buf)
    }

    fn read_shard_at(&self, device_id: &DeviceId, offset: u64, buf: &mut [u8]) -> Result<()> {
        let device = self.device(device_id)?;
        read_exact_at(
            device.shard_file.as_ref().unwrap_or(&device.file),
            device_id,
            offset,
            buf,
        )
    }

    fn write_at(&self, device_id: &DeviceId, offset: u64, mut data: &[u8]) -> Result<()> {
        let device = self.device(device_id)?;
        let end = offset
            .checked_add(data.len() as u64)
            .ok_or_else(|| ArgosError::Invalid("write offset overflow".to_string()))?;
        if end > device.capacity {
            return Err(ArgosError::DiskFull {
                disk_id: device_id.clone(),
                required: data.len() as u64,
                available: device.capacity.saturating_sub(offset),
            });
        }
        let mut cursor = offset;
        while !data.is_empty() {
            let written = device.file.write_at(data, cursor)?;
            if written == 0 {
                return Err(ArgosError::Io(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    format!("short write on {device_id} at offset {cursor}"),
                )));
            }
            cursor = cursor.saturating_add(written as u64);
            data = &data[written..];
        }
        Ok(())
    }

    fn write_batch(&self, device_id: &DeviceId, writes: &[(u64, &[u8])]) -> Result<()> {
        let device = self.device(device_id)?;
        // Validate the complete batch before modifying any range.
        for (offset, data) in writes {
            let end = offset
                .checked_add(data.len() as u64)
                .ok_or_else(|| ArgosError::Invalid("write offset overflow".into()))?;
            if end > device.capacity {
                return Err(ArgosError::DiskFull {
                    disk_id: device_id.clone(),
                    required: data.len() as u64,
                    available: device.capacity.saturating_sub(*offset),
                });
            }
        }
        #[cfg(target_os = "linux")]
        {
            let mut index = 0;
            while index < writes.len() {
                let (offset, data) = writes[index];
                if data.is_empty() {
                    index += 1;
                    continue;
                }
                let mut end = offset + data.len() as u64;
                let mut next = index + 1;
                // Preserve submission order, including overlaps. Only
                // adjacent ranges can share one positioned vectored syscall.
                while next < writes.len() && next - index < 64 {
                    let (start, data) = writes[next];
                    if start != end || data.is_empty() {
                        break;
                    }
                    end += data.len() as u64;
                    next += 1;
                }
                if next == index + 1 {
                    self.write_at(device_id, offset, data)?;
                } else {
                    write_contiguous_ranges(&device.file, offset, &writes[index..next])?;
                }
                index = next;
            }
        }
        #[cfg(not(target_os = "linux"))]
        for (offset, data) in writes {
            self.write_at(device_id, *offset, data)?;
        }
        Ok(())
    }

    fn flush_device(&self, device_id: &DeviceId) -> Result<()> {
        self.device(device_id)?.file.sync_all()?;
        Ok(())
    }

    fn flush_all(&self) -> Result<()> {
        for device_id in self.devices.keys() {
            self.flush_device(device_id)?;
        }
        Ok(())
    }

    fn capacity(&self, device_id: &DeviceId) -> Result<u64> {
        Ok(self.device(device_id)?.capacity)
    }

    fn device_status(&self, device_id: &DeviceId) -> Result<DiskStatus> {
        let _ = self.device(device_id)?;
        Ok(DiskStatus::Online)
    }

    fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            direct_io: false,
            fallocate: true,
            discard: self.kind == BackendKind::RawBlock,
        }
    }

    #[cfg(target_os = "linux")]
    fn drop_cache(&self, device_id: &DeviceId, offset: u64, length: u64) -> Result<()> {
        use std::os::unix::io::AsRawFd;
        let device = self.device(device_id)?;
        // DONTNEED only discards clean, full pages; shard payloads may have
        // a short final block. The verifier explicitly flushes before this.
        let length = length.saturating_add(4095) & !4095;
        let status = unsafe {
            libc::posix_fadvise(
                device.file.as_raw_fd(),
                offset as libc::off_t,
                length as libc::off_t,
                libc::POSIX_FADV_DONTNEED,
            )
        };
        if status != 0 {
            return Err(ArgosError::Unsupported(format!(
                "page-cache eviction failed on {device_id}: {}",
                std::io::Error::from_raw_os_error(status)
            )));
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn write_contiguous_ranges(file: &File, mut offset: u64, writes: &[(u64, &[u8])]) -> Result<()> {
    let mut index = 0;
    let mut consumed = 0;
    while index < writes.len() {
        let mut vectors = Vec::with_capacity(writes.len() - index);
        for (i, (_, data)) in writes[index..].iter().enumerate() {
            let data = if i == 0 { &data[consumed..] } else { data };
            vectors.push(libc::iovec {
                iov_base: data.as_ptr().cast_mut().cast(),
                iov_len: data.len(),
            });
        }
        let position = libc::off_t::try_from(offset)
            .map_err(|_| ArgosError::Invalid("write offset exceeds off_t".into()))?;
        // SAFETY: the slice payloads and iovec array live through this
        // synchronous syscall; pwritev only reads the buffers.
        let result = unsafe {
            libc::pwritev(
                file.as_raw_fd(),
                vectors.as_ptr(),
                vectors.len() as i32,
                position,
            )
        };
        if result < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err.into());
        }
        if result == 0 {
            return Err(
                std::io::Error::new(std::io::ErrorKind::WriteZero, "short vectored write").into(),
            );
        }
        let mut written = result as usize;
        offset += written as u64;
        while index < writes.len() {
            let remaining = writes[index].1.len() - consumed;
            if written < remaining {
                consumed += written;
                break;
            }
            written -= remaining;
            index += 1;
            consumed = 0;
        }
    }
    Ok(())
}

fn read_exact_at(file: &File, device_id: &DeviceId, offset: u64, mut buf: &mut [u8]) -> Result<()> {
    let mut cursor = offset;
    while !buf.is_empty() {
        let read = file.read_at(buf, cursor)?;
        if read == 0 {
            return Err(ArgosError::Io(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                format!("short read on {device_id} at offset {cursor}"),
            )));
        }
        cursor = cursor.saturating_add(read as u64);
        let (_, rest) = buf.split_at_mut(read);
        buf = rest;
    }
    Ok(())
}

fn same_open_device(first: &File, second: &File) -> Result<bool> {
    let a = first.metadata()?;
    let b = second.metadata()?;
    Ok(a.dev() == b.dev() && a.ino() == b.ino() && a.rdev() == b.rdev())
}

fn open_block_device(path: &Path, write: bool, random_data_reads: bool) -> Result<BlockDevice> {
    let mut file = OpenOptions::new().read(true).write(write).open(path)?;
    // Independent open descriptions preserve sequential metadata readahead.
    // If the path changes or a second open fails, keep the original handle
    // and its default policy rather than reading from a different device.
    let shard_file = if random_data_reads {
        match File::open(path) {
            Ok(shard) if same_open_device(&file, &shard).unwrap_or(false) => {
                advise_random_access(&shard);
                Some(shard)
            }
            _ => None,
        }
    } else {
        None
    };
    let capacity = detect_capacity(&mut file)?;
    Ok(BlockDevice {
        path: path.to_path_buf(),
        file,
        shard_file,
        capacity,
    })
}

#[cfg(target_os = "linux")]
fn advise_random_access(file: &File) {
    // Best effort: a kernel or filesystem without advice support just keeps
    // its default readahead.
    let _ = unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_RANDOM) };
}

#[cfg(not(target_os = "linux"))]
fn advise_random_access(_file: &File) {}

fn detect_capacity(file: &mut File) -> Result<u64> {
    let len = file.metadata()?.len();
    if len != 0 {
        return Ok(len);
    }

    if let Some(bytes) = block_device_capacity(file)? {
        return Ok(bytes);
    }

    Ok(file.seek(SeekFrom::End(0))?)
}

#[cfg(target_os = "linux")]
fn block_device_capacity(file: &File) -> Result<Option<u64>> {
    const BLKGETSIZE64: u64 = 0x8008_1272;

    let mut bytes = 0u64;
    let result = unsafe { libc::ioctl(file.as_raw_fd(), BLKGETSIZE64 as _, &mut bytes) };
    if result == 0 && bytes != 0 {
        Ok(Some(bytes))
    } else {
        Ok(None)
    }
}

#[cfg(not(target_os = "linux"))]
fn block_device_capacity(_file: &File) -> Result<Option<u64>> {
    Ok(None)
}

#[cfg(test)]
#[path = "backend_tests.rs"]
mod tests;
