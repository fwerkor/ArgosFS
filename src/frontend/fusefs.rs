mod credentials;
mod handles;
mod writeback;
use credentials::*;
use handles::*;
use writeback::*;

use super::fuse_directory_cache::{DirectoryListingCache, DirectoryRevision};
use super::fuse_queue::FuseRequestExecutor;
use super::metrics;
use crate::acl;
use crate::commit_metrics::WritebackFlushReason;
use crate::error::{ArgosError, Result};
use crate::types::{InodeId, NodeKind};
use crate::util::now_f64;
use crate::volume::{ArgosFs, CapacitySnapshot, NodeAttr, RenamePolicy};
use fuser::{
    AccessFlags, BsdFileFlags, Config, CopyFileRangeFlags, Errno, FileAttr, FileHandle, FileType,
    Filesystem, FopenFlags, Generation, INodeNo, InitFlags, KernelConfig, LockOwner, MountOption,
    OpenFlags, RenameFlags, ReplyAttr, ReplyCreate, ReplyData, ReplyDirectory, ReplyDirectoryPlus,
    ReplyEmpty, ReplyEntry, ReplyLseek, ReplyOpen, ReplyStatfs, ReplyWrite, ReplyXattr, Request,
    SessionACL, TimeOrNow, WriteFlags,
};
use parking_lot::{Mutex, RwLock};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{CString, OsStr};
use std::io::Read;
use std::ops::Deref;
use std::os::fd::{AsRawFd, FromRawFd};
use std::path::Path;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::sync::{Arc, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const TTL: Duration = Duration::from_secs(5);
// The kernel still sends at most 1 MiB per request. Accumulating up to four
// adjacent requests amortizes one partially-covered stripe at each boundary
// across a larger sequential write, without unbounded per-inode buffering.
const FUSE_WRITEBACK_MAX_BYTES: usize = 4 * 1024 * 1024;
// A hot mount with many open files must not accumulate a full per-inode
// allowance for every inode. Once full, new writes take the existing
// synchronous fallback without evicting another inode's acknowledged data.
const FUSE_WRITEBACK_MAX_MOUNT_BYTES: usize = 64 * 1024 * 1024;
const FUSE_WRITEBACK_MAX_INODES: usize = 4096;
const WRITEBACK_LOCK_STRIPES: usize = 128;
// The small FUSE reader pool does not run blocking I/O. The execution pool
// follows available CPU parallelism and both pools are independently tunable.
fn fuse_thread_count(variable: &str, min: usize, max: usize) -> usize {
    std::env::var(variable)
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|count| (min..=128).contains(count))
        .unwrap_or_else(|| {
            thread::available_parallelism()
                .map_or(min, |parallelism| parallelism.get())
                .clamp(min, max)
        })
}

pub struct ArgosFuse {
    shared: Arc<ArgosFuseShared>,
    commit_worker: Option<DeferredCommitWorker>,
    io_tasks: OnceLock<std::result::Result<FuseRequestExecutor, String>>,
}

/// Shared state held by in-flight requests until their asynchronous replies
/// have been sent. The mount's Drop joins workers before syncing this state.
#[doc(hidden)]
pub struct ArgosFuseShared {
    volume: ArgosFs,
    writeback: Mutex<FuseWriteback>,
    writeback_locks: [RwLock<()>; WRITEBACK_LOCK_STRIPES],
    handles: Mutex<FuseHandles>,
    directory_cache: Mutex<DirectoryListingCache>,
    statfs_cache: Mutex<CapacitySnapshot>,
}

impl Deref for ArgosFuse {
    type Target = ArgosFuseShared;

    fn deref(&self) -> &Self::Target {
        &self.shared
    }
}

struct DeferredCommitWorker {
    stop: Option<Sender<()>>,
    join: Option<JoinHandle<()>>,
}

impl DeferredCommitWorker {
    fn start(volume: &ArgosFs) -> Result<Option<Self>> {
        let Some(interval) = volume.deferred_commit_interval() else {
            return Ok(None);
        };
        let (stop, receiver) = mpsc::channel();
        let volume = volume.clone();
        let join = thread::Builder::new()
            .name("argosfs-group-commit".to_string())
            .spawn(move || loop {
                match receiver.recv_timeout(interval) {
                    Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                    Err(RecvTimeoutError::Timeout) => {
                        if let Err(err) = volume.sync_deferred_if_dirty() {
                            eprintln!("argosfs: deferred group commit failed: {err}");
                        }
                    }
                }
            })?;
        Ok(Some(Self {
            stop: Some(stop),
            join: Some(join),
        }))
    }

    fn stop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for DeferredCommitWorker {
    fn drop(&mut self) {
        self.stop();
    }
}

impl ArgosFuse {
    pub fn new(volume: ArgosFs) -> Self {
        Self::try_new(volume).expect("start ArgosFS deferred commit worker")
    }

    fn try_new(volume: ArgosFs) -> Result<Self> {
        // Seed the snapshot before the first FUSE request, so statfs never
        // needs to block on a metadata write lock even during the first commit.
        let unavailable = volume.runtime_unavailable_devices();
        let snapshot = CapacitySnapshot::from_metadata(
            volume.root(),
            &volume.metadata_snapshot(),
            unavailable,
        );
        let commit_worker = DeferredCommitWorker::start(&volume)?;
        Ok(Self {
            shared: Arc::new(ArgosFuseShared {
                volume,
                writeback: Mutex::new(FuseWriteback::default()),
                writeback_locks: std::array::from_fn(|_| RwLock::new(())),
                handles: Mutex::new(FuseHandles::default()),
                directory_cache: Mutex::new(DirectoryListingCache::default()),
                statfs_cache: Mutex::new(snapshot),
            }),
            commit_worker,
            io_tasks: OnceLock::new(),
        })
    }

    fn submit_io(&self, ino: InodeId, job: impl FnOnce(Arc<ArgosFuseShared>) + Send + 'static) {
        self.submit_io_kind(ino, false, job);
    }

    fn submit_io_kind(
        &self,
        ino: InodeId,
        shared_read: bool,
        job: impl FnOnce(Arc<ArgosFuseShared>) + Send + 'static,
    ) {
        let shared = Arc::clone(&self.shared);
        let executor = self.io_tasks.get_or_init(|| {
            FuseRequestExecutor::start(fuse_thread_count("ARGOSFS_FUSE_IO_WORKERS", 2, 16))
                .map_err(|err| err.to_string())
        });
        match executor {
            Ok(executor) => {
                let accepted = if shared_read {
                    executor.submit_read(ino, move || job(shared))
                } else {
                    executor.submit(ino, move || job(shared))
                };
                if !accepted {
                    eprintln!("argosfs: rejecting FUSE request during shutdown");
                }
            }
            Err(err) => {
                eprintln!("argosfs: unable to dispatch FUSE I/O: {err}");
                // Dropping an unfulfilled reply inside job sends EIO to FUSE.
            }
        }
    }
}

impl ArgosFuseShared {
    fn directory_listing(&self, ino: InodeId) -> Result<Arc<Vec<crate::volume::DirEntry>>> {
        let current = self.volume.attr_inode(ino)?;
        if current.kind != NodeKind::Directory {
            return Err(ArgosError::NotDirectory(format!("inode {ino}")));
        }
        if let Some(entries) = self
            .directory_cache
            .lock()
            .get(ino, DirectoryRevision::from(&current))
        {
            return Ok(entries);
        }
        // Materialize outside the cache lock. Each listing is constructed
        // from one metadata read snapshot; '.' carries its exact revision.
        let entries = Arc::new(self.volume.readdir(ino)?);
        if let Some(dot) = entries.first() {
            self.directory_cache.lock().insert(
                ino,
                DirectoryRevision::from(&dot.attr),
                Arc::clone(&entries),
            );
        }
        Ok(entries)
    }

    fn statfs_snapshot(&self) -> CapacitySnapshot {
        let unavailable = self.volume.runtime_unavailable_devices();
        if let Some(meta) = self.volume.try_metadata_snapshot() {
            let snapshot = CapacitySnapshot::from_metadata(self.volume.root(), &meta, unavailable);
            *self.statfs_cache.lock() = snapshot.clone();
            return snapshot;
        }
        // A checkpoint is holding the metadata writer lock. Do not wait.
        // Prefer the snapshot the volume republishes after every durable
        // commit: it advances while a long checkpoint is in flight, unlike a
        // value cached only by earlier statfs calls.
        let mut cached = self
            .volume
            .published_capacity_snapshot()
            .unwrap_or_else(|| self.statfs_cache.lock().clone());
        // Never advertise cached free capacity when additional members have
        // been quarantined since the snapshot was taken.
        if !unavailable.is_subset(&cached.unavailable) {
            cached.free_bytes = 0;
        }
        cached
    }

    fn require_access(&self, req: &impl Credentials, ino: INodeNo, mask: i32) -> Result<()> {
        let groups = request_groups(req);
        self.volume
            .check_access_inode_with_groups(ino.0, req.uid(), &groups, mask)
    }

    fn open_reply_flags(&self, write_open: bool) -> FopenFlags {
        let mut flags = FopenFlags::empty();
        if self.volume.io_policy().direct_io {
            flags |= FopenFlags::FOPEN_DIRECT_IO;
        }
        if write_open {
            flags |= FopenFlags::FOPEN_NOFLUSH;
        } else if !self.volume.io_policy().direct_io {
            flags |= FopenFlags::FOPEN_KEEP_CACHE;
        }
        flags
    }

    fn require_xattr_write_access(
        &self,
        req: &impl Credentials,
        ino: INodeNo,
        name: &str,
    ) -> Result<()> {
        if name.starts_with("user.") {
            return self.require_access(req, ino, libc::W_OK);
        }
        if is_owner_managed_xattr(name) {
            let attr = self.volume.attr_inode(ino.0)?;
            if req.uid() == 0 || req.uid() == attr.uid {
                return Ok(());
            }
            return Err(ArgosError::PermissionDenied(format!(
                "xattr {name} requires file ownership or root"
            )));
        }
        if name.starts_with("trusted.")
            || name.starts_with("security.")
            || name.starts_with("system.argosfs.")
            || name.starts_with("system.")
        {
            if req.uid() == 0 {
                return Ok(());
            }
            return Err(ArgosError::PermissionDenied(format!(
                "xattr namespace requires root: {name}"
            )));
        }
        self.require_access(req, ino, libc::W_OK)
    }

    fn require_regular_file(&self, ino: INodeNo) -> Result<()> {
        let attr = self.volume.attr_inode(ino.0)?;
        if attr.kind != NodeKind::File {
            return Err(ArgosError::IsDirectory(format!("inode {ino}")));
        }
        Ok(())
    }

    fn require_writable_handle(&self, ino: INodeNo, fh: FileHandle) -> Result<()> {
        let file = self
            .handles
            .lock()
            .get(fh)
            .cloned()
            .ok_or_else(|| ArgosError::Invalid("invalid FUSE file handle".to_string()))?;
        if file.ino != ino.0 {
            return Err(ArgosError::Invalid(
                "FUSE file handle inode mismatch".to_string(),
            ));
        }
        if !file.can_write() {
            return Err(ArgosError::PermissionDenied(
                "FUSE file handle is not open for writing".to_string(),
            ));
        }
        Ok(())
    }

    fn lookup_after_access(&self, parent: InodeId, name: &OsStr) -> Result<NodeAttr> {
        let attr = self.volume.lookup(parent, name)?;
        // A contiguous pending append can supply fresh attributes directly,
        // without forcing a partial-stripe rewrite for path lookup. Keep the
        // old best-effort behavior for invalid or failed dirty extents.
        self.visible_inode_attr(attr.ino)
            .or_else(|_| self.volume.lookup(parent, name))
    }

    fn unlink_after_access(&self, parent: InodeId, name: &OsStr, uid: u32) -> Result<()> {
        // Keep the open-handle decision atomic with the namespace mutation. A
        // concurrent open/release must not turn a preserving unlink into a normal
        // unlink (or vice versa) after we have inspected the reference count.
        let handles = self.handles.lock();
        let attr = self.volume.lookup(parent, name)?;
        if handles.refs(attr.ino) > 0 {
            return self.volume.unlink_at_as_preserving_open(parent, name, uid);
        }

        // Only the target inode matters for unlink. Unrelated dirty inodes must not
        // block removal. If this inode cannot be flushed, retain its buffered data
        // until the namespace mutation actually succeeds; otherwise a rejected
        // unlink (for example, sticky-directory permissions) would lose data.
        let target_flush_failed = self
            .flush_inode_writeback_reason(attr.ino, WritebackFlushReason::Metadata)
            .is_err();
        let removes_inode = attr.nlink <= 1;
        let result = self.volume.unlink_at_as(parent, name, uid);
        if result.is_ok() && target_flush_failed && removes_inode {
            self.discard_inode_writeback(attr.ino);
        }
        result
    }
}

impl Drop for ArgosFuse {
    fn drop(&mut self) {
        // Accepted requests may still own dirty writeback and file handles.
        // Drain them before stopping group commit and doing final sync.
        if let Some(Ok(executor)) = self.io_tasks.get_mut() {
            executor.shutdown();
        }
        if let Some(worker) = self.commit_worker.as_mut() {
            worker.stop();
        }
        let _ = self.flush_all_writeback();
        let _ = self.volume.sync();
    }
}

pub fn mount(
    volume_root: impl AsRef<Path>,
    mountpoint: impl AsRef<Path>,
    foreground: bool,
    options: Vec<String>,
) -> Result<()> {
    mount_with_metrics(volume_root, mountpoint, foreground, options, None)
}

pub fn mount_with_metrics(
    volume_root: impl AsRef<Path>,
    mountpoint: impl AsRef<Path>,
    foreground: bool,
    options: Vec<String>,
    metrics_listen: Option<&str>,
) -> Result<()> {
    let volume = ArgosFs::open(volume_root)?;
    mount_volume_with_metrics(volume, mountpoint, foreground, options, metrics_listen)
}

pub fn mount_volume(
    volume: ArgosFs,
    mountpoint: impl AsRef<Path>,
    foreground: bool,
    options: Vec<String>,
) -> Result<()> {
    mount_volume_with_metrics(volume, mountpoint, foreground, options, None)
}

pub fn mount_volume_with_metrics(
    volume: ArgosFs,
    mountpoint: impl AsRef<Path>,
    foreground: bool,
    options: Vec<String>,
    metrics_listen: Option<&str>,
) -> Result<()> {
    let mut mount_options = vec![
        MountOption::FSName("argosfs".to_string()),
        MountOption::Subtype("argosfs".to_string()),
    ];
    for option in options {
        mount_options.push(MountOption::CUSTOM(option));
    }
    if !foreground {
        eprintln!(
            "argosfs mount runs in the foreground; --foreground is accepted for CLI compatibility"
        );
    }
    let config = mount_config(mount_options);
    // This server sees the same atomic counters as FUSE and the commit worker.
    // Keep it scoped to the blocking mount so it cannot keep the volume alive
    // after the FUSE filesystem performs its final sync.
    let _metrics_server = metrics_listen
        .map(|address| metrics::MountedMetricsServer::start(&volume, address))
        .transpose()?;
    fuser::mount2(ArgosFuse::try_new(volume)?, mountpoint, &config).map_err(ArgosError::Io)
}

fn mount_config(options: Vec<MountOption>) -> Config {
    let mut config = Config::default();
    // Linux FUSE can dispatch requests on separate channels. In particular,
    // statfs must be able to reach its lock-free cached snapshot while a
    // different request is blocked on writeback or metadata persistence.
    #[cfg(target_os = "linux")]
    {
        config.n_threads = Some(fuse_thread_count("ARGOSFS_FUSE_REQUEST_THREADS", 2, 8));
        config.clone_fd = true;
    }
    for option in options {
        match option {
            MountOption::CUSTOM(ref value) if value == "allow_other" => {
                config.acl = SessionACL::All;
            }
            MountOption::CUSTOM(ref value) if value == "allow_root" => {
                if config.acl != SessionACL::All {
                    config.acl = SessionACL::RootAndOwner;
                }
            }
            other => config.mount_options.push(normalize_mount_option(other)),
        }
    }
    config
}

fn normalize_mount_option(option: MountOption) -> MountOption {
    match option {
        MountOption::CUSTOM(value) => match value.as_str() {
            "auto_unmount" => MountOption::AutoUnmount,
            "default_permissions" => MountOption::DefaultPermissions,
            "dev" => MountOption::Dev,
            "nodev" => MountOption::NoDev,
            "suid" => MountOption::Suid,
            "nosuid" => MountOption::NoSuid,
            "ro" => MountOption::RO,
            "rw" => MountOption::RW,
            "exec" => MountOption::Exec,
            "noexec" => MountOption::NoExec,
            "atime" => MountOption::Atime,
            "noatime" => MountOption::NoAtime,
            "dirsync" => MountOption::DirSync,
            "sync" => MountOption::Sync,
            "async" => MountOption::Async,
            _ if value.starts_with("fsname=") => MountOption::FSName(value[7..].to_string()),
            _ if value.starts_with("subtype=") => MountOption::Subtype(value[8..].to_string()),
            _ => MountOption::CUSTOM(value),
        },
        other => other,
    }
}

impl Filesystem for ArgosFuse {
    fn init(
        &mut self,
        _req: &Request,
        config: &mut KernelConfig,
    ) -> std::result::Result<(), std::io::Error> {
        let _ = config.set_max_write(1024 * 1024);
        let _ = config.set_max_readahead(1024 * 1024);
        let _ = config.set_max_background(64);
        let _ = config.set_congestion_threshold(48);
        let _ = config
            .add_capabilities(InitFlags::FUSE_PARALLEL_DIROPS | InitFlags::FUSE_READDIRPLUS_AUTO);
        Ok(())
    }

    fn lookup(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEntry) {
        let req = RequestIdentity::from_request(req);
        let name = name.to_owned();
        self.submit_io(parent.0, move |shared| {
            let req = &req;
            let name = name.as_os_str();
            match shared
                .require_access(req, parent, libc::X_OK)
                .and_then(|()| shared.lookup_after_access(parent.0, name))
            {
                Ok(attr) => reply.entry(&TTL, &to_file_attr(&attr), Generation(0)),
                Err(err) => reply.error(errno(&err)),
            }
        });
    }

    fn getattr(&self, _req: &Request, ino: INodeNo, _fh: Option<FileHandle>, reply: ReplyAttr) {
        self.submit_io(ino.0, move |shared| {
            match shared.visible_inode_attr(ino.0) {
                Ok(attr) => reply.attr(&TTL, &to_file_attr(&attr)),
                Err(err) => reply.error(errno(&err)),
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn setattr(
        &self,
        req: &Request,
        ino: INodeNo,
        mode: Option<u32>,
        uid: Option<u32>,
        gid: Option<u32>,
        size: Option<u64>,
        atime: Option<TimeOrNow>,
        mtime: Option<TimeOrNow>,
        _ctime: Option<SystemTime>,
        fh: Option<FileHandle>,
        _crtime: Option<SystemTime>,
        _chgtime: Option<SystemTime>,
        _bkuptime: Option<SystemTime>,
        _flags: Option<BsdFileFlags>,
        reply: ReplyAttr,
    ) {
        let req = RequestIdentity::from_request(req);
        self.submit_io(ino.0, move |shared| {
            let req = &req;
            if let Err(err) =
                shared.flush_inode_writeback_reason(ino.0, WritebackFlushReason::Metadata)
            {
                reply.error(errno(&err));
                return;
            }
            let result = (|| -> Result<NodeAttr> {
                let current = shared.volume.attr_inode(ino.0)?;
                let groups = request_groups(req);
                let kernel_killpriv = mode.is_some_and(|requested_mode| {
                    req.uid() != 0
                        && req.uid() != current.uid
                        && is_setid_clear_only(current.mode, requested_mode)
                });
                if mode.is_some() && req.uid() != 0 && req.uid() != current.uid && !kernel_killpriv
                {
                    return Err(ArgosError::PermissionDenied(
                        "chmod requires file ownership or root".to_string(),
                    ));
                }
                if (uid.is_some() || gid.is_some()) && req.uid() != 0 {
                    if req.uid() != current.uid {
                        return Err(ArgosError::PermissionDenied(
                            "chown requires file ownership or root".to_string(),
                        ));
                    }
                    if uid.is_some_and(|requested_uid| requested_uid != current.uid) {
                        return Err(ArgosError::PermissionDenied(
                            "non-root chown cannot change file owner".to_string(),
                        ));
                    }
                    if gid.is_some_and(|requested_gid| !groups.contains(&requested_gid)) {
                        return Err(ArgosError::PermissionDenied(
                            "non-root chown requires the requested group to be supplementary"
                                .to_string(),
                        ));
                    }
                }
                if size.is_some() {
                    if let Some(fh) = fh {
                        shared.require_writable_handle(ino, fh)?;
                    } else {
                        shared.require_access(req, ino, libc::W_OK)?;
                    }
                }
                if atime.is_some() || mtime.is_some() {
                    let owner_or_root = req.uid() == 0 || req.uid() == current.uid;
                    if !owner_or_root && (is_specific_time(&atime) || is_specific_time(&mtime)) {
                        return Err(ArgosError::PermissionDenied(
                            "setting explicit timestamps requires ownership or root".to_string(),
                        ));
                    }
                    if !owner_or_root {
                        shared.require_access(req, ino, libc::W_OK)?;
                    }
                }
                let ino = ino.0;
                let mut attr = current;
                if let Some(mode) = mode {
                    attr = if kernel_killpriv {
                        shared.volume.chmod_inode(ino, mode)?
                    } else {
                        shared
                            .volume
                            .chmod_inode_as(ino, mode, req.uid(), &groups)?
                    };
                }
                if uid.is_some() || gid.is_some() {
                    attr = shared
                        .volume
                        .chown_inode_as(ino, uid, gid, req.uid(), &groups)?;
                }
                if let Some(size) = size {
                    shared.volume.truncate_inode_as(ino, size)?;
                    attr = shared.volume.attr_inode(ino)?;
                }
                if atime.is_some() || mtime.is_some() {
                    let current = shared.volume.attr_inode(ino)?;
                    let atime = atime.map(time_or_now).unwrap_or(current.atime);
                    let mtime = mtime.map(time_or_now).unwrap_or(current.mtime);
                    attr = shared.volume.utimens_inode(ino, atime, mtime)?;
                }
                Ok(attr)
            })();
            match result {
                Ok(attr) => reply.attr(&TTL, &to_file_attr(&attr)),
                Err(err) => reply.error(errno(&err)),
            }
        });
    }

    fn readlink(&self, _req: &Request, ino: INodeNo, reply: ReplyData) {
        self.submit_io(ino.0, move |shared| {
            if let Err(err) =
                shared.flush_inode_writeback_reason(ino.0, WritebackFlushReason::Metadata)
            {
                reply.error(errno(&err));
                return;
            }
            match shared.volume.readlink_inode_bytes(ino.0) {
                Ok(target) => reply.data(&target),
                Err(err) => reply.error(errno(&err)),
            }
        });
    }

    fn mknod(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        rdev: u32,
        reply: ReplyEntry,
    ) {
        let req = RequestIdentity::from_request(req);
        let name = name.to_owned();
        self.submit_io(parent.0, move |shared| {
            let req = &req;
            let name = name.as_os_str();
            if let Err(err) = shared.flush_all_writeback() {
                reply.error(errno(&err));
                return;
            }
            let file_type = mode & libc::S_IFMT;
            if matches!(file_type, value if value == libc::S_IFCHR || value == libc::S_IFBLK)
                && req.uid() != 0
            {
                reply.error(Errno::EACCES);
                return;
            }
            match shared
                .require_access(req, parent, libc::W_OK | libc::X_OK)
                .and_then(|()| {
                    shared.volume.mknod_at_with_owner(
                        parent.0,
                        name,
                        mode & !umask,
                        rdev as u64,
                        req.uid(),
                        req.gid(),
                    )
                }) {
                Ok(attr) => reply.entry(&TTL, &to_file_attr(&attr), Generation(0)),
                Err(err) => reply.error(errno(&err)),
            }
        });
    }

    fn mkdir(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        reply: ReplyEntry,
    ) {
        let req = RequestIdentity::from_request(req);
        let name = name.to_owned();
        self.submit_io(parent.0, move |shared| {
            let req = &req;
            let name = name.as_os_str();
            if let Err(err) = shared.flush_all_writeback() {
                reply.error(errno(&err));
                return;
            }
            match shared
                .require_access(req, parent, libc::W_OK | libc::X_OK)
                .and_then(|()| {
                    shared.volume.mkdir_at_with_owner(
                        parent.0,
                        name,
                        mode & !umask,
                        req.uid(),
                        req.gid(),
                    )
                }) {
                Ok(attr) => reply.entry(&TTL, &to_file_attr(&attr), Generation(0)),
                Err(err) => reply.error(errno(&err)),
            }
        });
    }

    fn unlink(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let req = RequestIdentity::from_request(req);
        let name = name.to_owned();
        self.submit_io(parent.0, move |shared| {
            let req = &req;
            let name = name.as_os_str();
            let result = shared
                .require_access(req, parent, libc::W_OK | libc::X_OK)
                .and_then(|()| shared.unlink_after_access(parent.0, name, req.uid()));
            match result {
                Ok(()) => reply.ok(),
                Err(err) => reply.error(errno(&err)),
            }
        });
    }

    fn rmdir(&self, req: &Request, parent: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let req = RequestIdentity::from_request(req);
        let name = name.to_owned();
        self.submit_io(parent.0, move |shared| {
            let req = &req;
            let name = name.as_os_str();
            if let Err(err) = shared.flush_all_writeback() {
                reply.error(errno(&err));
                return;
            }
            match shared
                .require_access(req, parent, libc::W_OK | libc::X_OK)
                .and_then(|()| shared.volume.rmdir_at_as(parent.0, name, req.uid()))
            {
                Ok(()) => reply.ok(),
                Err(err) => reply.error(errno(&err)),
            }
        });
    }

    fn symlink(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        link: &Path,
        reply: ReplyEntry,
    ) {
        let req = RequestIdentity::from_request(req);
        let name = name.to_owned();
        let link = link.to_owned();
        self.submit_io(parent.0, move |shared| {
            let req = &req;
            let name = name.as_os_str();
            let link = link.as_path();
            if let Err(err) = shared.flush_all_writeback() {
                reply.error(errno(&err));
                return;
            }
            match shared
                .require_access(req, parent, libc::W_OK | libc::X_OK)
                .and_then(|()| {
                    shared
                        .volume
                        .symlink_at_with_owner(parent.0, name, link, req.uid(), req.gid())
                }) {
                Ok(attr) => reply.entry(&TTL, &to_file_attr(&attr), Generation(0)),
                Err(err) => reply.error(errno(&err)),
            }
        });
    }

    fn rename(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        newparent: INodeNo,
        newname: &OsStr,
        flags: RenameFlags,
        reply: ReplyEmpty,
    ) {
        let req = RequestIdentity::from_request(req);
        let name = name.to_owned();
        let newname = newname.to_owned();
        self.submit_io(parent.0, move |shared| {
            let req = &req;
            let name = name.as_os_str();
            let newname = newname.as_os_str();
            if let Err(err) = shared.flush_all_writeback() {
                reply.error(errno(&err));
                return;
            }
            let supported_flags = RenameFlags::RENAME_NOREPLACE
                | RenameFlags::RENAME_EXCHANGE
                | RenameFlags::RENAME_WHITEOUT;
            if flags.bits() & !supported_flags.bits() != 0
                || flags.contains(RenameFlags::RENAME_WHITEOUT)
                || flags.contains(RenameFlags::RENAME_NOREPLACE)
                    && flags.contains(RenameFlags::RENAME_EXCHANGE)
            {
                reply.error(Errno::EINVAL);
                return;
            }
            let handles = shared.handles.lock();
            let preserve_replaced_inode = if flags.contains(RenameFlags::RENAME_EXCHANGE) {
                false
            } else {
                shared
                    .volume
                    .lookup(newparent.0, newname)
                    .ok()
                    .is_some_and(|attr| handles.refs(attr.ino) > 0)
            };
            let policy = RenamePolicy {
                no_replace: flags.contains(RenameFlags::RENAME_NOREPLACE),
                exchange: flags.contains(RenameFlags::RENAME_EXCHANGE),
                uid: Some(req.uid()),
                preserve_replaced_inode,
            };
            let result = shared
                .require_access(req, parent, libc::W_OK | libc::X_OK)
                .and_then(|()| shared.require_access(req, newparent, libc::W_OK | libc::X_OK))
                .and_then(|()| {
                    shared.volume.rename_at_with_policy(
                        parent.0,
                        name,
                        newparent.0,
                        newname,
                        policy,
                    )
                });
            drop(handles);
            match result {
                Ok(()) => reply.ok(),
                Err(err) => reply.error(errno(&err)),
            }
        });
    }

    fn link(
        &self,
        req: &Request,
        ino: INodeNo,
        newparent: INodeNo,
        newname: &OsStr,
        reply: ReplyEntry,
    ) {
        let req = RequestIdentity::from_request(req);
        let newname = newname.to_owned();
        self.submit_io(ino.0, move |shared| {
            let req = &req;
            let newname = newname.as_os_str();
            if let Err(err) = shared.flush_all_writeback() {
                reply.error(errno(&err));
                return;
            }
            match shared
                .require_access(req, newparent, libc::W_OK | libc::X_OK)
                .and_then(|()| shared.volume.link_at(ino.0, newparent.0, newname))
            {
                Ok(attr) => reply.entry(&TTL, &to_file_attr(&attr), Generation(0)),
                Err(err) => reply.error(errno(&err)),
            }
        });
    }

    fn open(&self, req: &Request, ino: INodeNo, flags: OpenFlags, reply: ReplyOpen) {
        let req = RequestIdentity::from_request(req);
        self.submit_io(ino.0, move |shared| {
            let req = &req;
            if flags.0 & libc::O_TRUNC != 0 {
                if let Err(err) =
                    shared.flush_inode_writeback_reason(ino.0, WritebackFlushReason::Metadata)
                {
                    reply.error(errno(&err));
                    return;
                }
            }
            let mut handles = shared.handles.lock();
            let result = (|| -> Result<FileHandle> {
                shared.require_access(req, ino, open_mask(flags))?;
                let mut attr = shared.volume.attr_inode(ino.0)?;
                if attr.kind != NodeKind::File {
                    return Err(ArgosError::IsDirectory(format!("inode {ino}")));
                }
                if flags.0 & libc::O_TRUNC != 0 {
                    shared.volume.truncate_inode_as(ino.0, 0)?;
                    attr = shared.volume.attr_inode(ino.0)?;
                }
                Ok(handles.open(OpenFileHandle {
                    ino: attr.ino,
                    flags: flags.0,
                }))
            })();
            drop(handles);
            match result {
                Ok(handle) => reply.opened(
                    handle,
                    shared.open_reply_flags(flags.0 & (libc::O_WRONLY | libc::O_RDWR) != 0),
                ),
                Err(err) => reply.error(errno(&err)),
            }
        });
    }

    fn read(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        size: u32,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyData,
    ) {
        self.submit_io_kind(ino.0, true, move |shared| {
            let file = shared.handles.lock().get(fh).cloned();
            let result = file
                .ok_or_else(|| ArgosError::Invalid("invalid FUSE file handle".to_string()))
                .and_then(|file| {
                    if file.ino != ino.0 {
                        return Err(ArgosError::Invalid(
                            "FUSE file handle inode mismatch".to_string(),
                        ));
                    }
                    if !file.can_read() {
                        return Err(ArgosError::PermissionDenied(
                            "FUSE file handle is not open for reading".to_string(),
                        ));
                    }
                    shared.read_visible_inode(ino.0, offset, size as usize)
                });
            match result {
                Ok(data) => reply.data(&data),
                Err(err) => reply.error(errno(&err)),
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn write(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        data: &[u8],
        _write_flags: WriteFlags,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        reply: ReplyWrite,
    ) {
        if data.is_empty() {
            reply.written(0);
            return;
        }
        let data = data.to_vec();
        self.submit_io(ino.0, move |shared| {
            let data = data.as_slice();
            let file = shared.handles.lock().get(fh).cloned();
            let result = file
                .ok_or_else(|| ArgosError::Invalid("invalid FUSE file handle".to_string()))
                .and_then(|file| {
                    if file.ino != ino.0 {
                        return Err(ArgosError::Invalid(
                            "FUSE file handle inode mismatch".to_string(),
                        ));
                    }
                    if !file.can_write() {
                        return Err(ArgosError::PermissionDenied(
                            "FUSE file handle is not open for writing".to_string(),
                        ));
                    }
                    let sync_write = file.flags & (libc::O_SYNC | libc::O_DSYNC) != 0;
                    let direct = shared.volume.io_policy().direct_io
                        || data.len() > FUSE_WRITEBACK_MAX_BYTES
                        || sync_write;
                    let written = if direct {
                        shared
                            .flush_inode_writeback_reason(
                                ino.0,
                                WritebackFlushReason::WritePressure,
                            )
                            .and_then(|()| {
                                shared
                                    .volume
                                    .write_inode_range_from_open_handle(ino.0, offset, data)
                            })?
                    } else {
                        shared.require_regular_file(ino)?;
                        if shared.queue_writeback_staged(ino.0, offset, data)? {
                            data.len()
                        } else {
                            shared.flush_inode_writeback_reason(
                                ino.0,
                                WritebackFlushReason::WritePressure,
                            )?;
                            if shared.queue_writeback_staged(ino.0, offset, data)? {
                                data.len()
                            } else {
                                shared
                                    .volume
                                    .write_inode_range_from_open_handle(ino.0, offset, data)?
                            }
                        }
                    };
                    if sync_write {
                        shared.volume.sync()?;
                    }
                    Ok(written)
                });
            match result {
                Ok(written) => reply.written(written as u32),
                Err(err) => reply.error(errno(&err)),
            }
        });
    }

    fn lseek(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: i64,
        whence: i32,
        reply: ReplyLseek,
    ) {
        self.submit_io(ino.0, move |shared| {
            if offset < 0 {
                reply.error(Errno::EINVAL);
                return;
            }
            let result = shared
                .handles
                .lock()
                .get(fh)
                .cloned()
                .ok_or_else(|| ArgosError::Invalid("invalid FUSE file handle".to_string()))
                .and_then(|file| {
                    if file.ino != ino.0 {
                        return Err(ArgosError::Invalid(
                            "FUSE file handle inode mismatch".to_string(),
                        ));
                    }
                    shared.flush_inode_writeback_reason(ino.0, WritebackFlushReason::Metadata)?;
                    shared
                        .volume
                        .seek_data_or_hole(ino.0, offset as u64, whence)
                });
            match result {
                Ok(next) => match i64::try_from(next) {
                    Ok(next) => reply.offset(next),
                    Err(_) => reply.error(Errno::EOVERFLOW),
                },
                Err(err) => reply.error(errno(&err)),
            }
        });
    }

    fn fallocate(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        offset: u64,
        length: u64,
        mode: i32,
        reply: ReplyEmpty,
    ) {
        self.submit_io(ino.0, move |shared| {
            let result = shared
                .handles
                .lock()
                .get(fh)
                .cloned()
                .ok_or_else(|| ArgosError::Invalid("invalid FUSE file handle".to_string()))
                .and_then(|file| {
                    if file.ino != ino.0 || !file.can_write() {
                        return Err(ArgosError::PermissionDenied(
                            "FUSE file handle is not open for this write".to_string(),
                        ));
                    }
                    shared.flush_inode_writeback_reason(ino.0, WritebackFlushReason::Metadata)?;
                    shared
                        .volume
                        .fallocate_inode_as(ino.0, offset, length, mode)
                });
            match result {
                Ok(()) => reply.ok(),
                Err(err) => reply.error(errno(&err)),
            }
        });
    }

    #[allow(clippy::too_many_arguments)]
    fn copy_file_range(
        &self,
        _req: &Request,
        ino_in: INodeNo,
        fh_in: FileHandle,
        offset_in: u64,
        ino_out: INodeNo,
        fh_out: FileHandle,
        offset_out: u64,
        len: u64,
        flags: CopyFileRangeFlags,
        reply: ReplyWrite,
    ) {
        // Serialize with other writes to the destination inode. The source
        // remains protected by the volume's existing read/data locks.
        self.submit_io(ino_out.0, move |shared| {
            if !flags.is_empty() {
                reply.error(Errno::EINVAL);
                return;
            }
            let result = (|| -> Result<usize> {
                let handles = shared.handles.lock();
                let input = handles.get(fh_in).ok_or_else(|| {
                    ArgosError::Invalid("invalid input FUSE file handle".to_string())
                })?;
                let output = handles.get(fh_out).ok_or_else(|| {
                    ArgosError::Invalid("invalid output FUSE file handle".to_string())
                })?;
                if input.ino != ino_in.0 || !input.can_read() {
                    return Err(ArgosError::PermissionDenied(
                        "input FUSE file handle is not open for reading".to_string(),
                    ));
                }
                if output.ino != ino_out.0 || !output.can_write() {
                    return Err(ArgosError::PermissionDenied(
                        "output FUSE file handle is not open for writing".to_string(),
                    ));
                }
                drop(handles);
                shared.flush_inode_writeback_reason(ino_in.0, WritebackFlushReason::Metadata)?;
                if ino_out != ino_in {
                    shared
                        .flush_inode_writeback_reason(ino_out.0, WritebackFlushReason::Metadata)?;
                }
                shared
                    .volume
                    .copy_inode_range_as(ino_in.0, offset_in, ino_out.0, offset_out, len)
            })();
            match result {
                Ok(copied) => reply.written(copied as u32),
                Err(err) => reply.error(errno(&err)),
            }
        });
    }

    fn statfs(&self, _req: &Request, _ino: INodeNo, reply: ReplyStatfs) {
        // statfs is an observation, not a durability boundary. Flushing every
        // dirty inode here could itself start a multi-minute checkpoint.
        let snapshot = self.statfs_snapshot();
        let block = snapshot.block_size;
        reply.statfs(
            snapshot.usable_bytes / block,
            snapshot.free_bytes / block,
            snapshot.free_bytes / block,
            snapshot.inode_count + 1_000_000,
            1_000_000,
            block as u32,
            255,
            block as u32,
        );
    }

    fn release(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _flags: OpenFlags,
        _lock_owner: Option<LockOwner>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        self.submit_io(ino.0, move |shared| {
            let flush_result =
                shared.flush_inode_writeback_reason(ino.0, WritebackFlushReason::Release);
            let mut handles = shared.handles.lock();
            let Some((file, last)) = handles.close(fh) else {
                reply.error(Errno::EBADF);
                return;
            };
            let reap_result = if last {
                if flush_result.is_ok() {
                    shared.volume.reap_unlinked_inode(file.ino)
                } else {
                    let mut writeback = shared.writeback.lock();
                    if writeback.dirty.contains_key(&file.ino) {
                        writeback.reap_after_flush.insert(file.ino);
                        Ok(())
                    } else {
                        drop(writeback);
                        shared.volume.reap_unlinked_inode(file.ino)
                    }
                }
            } else {
                Ok(())
            };
            drop(handles);
            match flush_result.and(reap_result) {
                Ok(()) => reply.ok(),
                Err(err) => reply.error(errno(&err)),
            }
        });
    }

    fn fsync(
        &self,
        _req: &Request,
        ino: INodeNo,
        fh: FileHandle,
        _datasync: bool,
        reply: ReplyEmpty,
    ) {
        self.submit_io(ino.0, move |shared| {
            let result = shared
                .handles
                .lock()
                .get(fh)
                .cloned()
                .ok_or_else(|| ArgosError::Invalid("invalid FUSE file handle".to_string()))
                .and_then(|file| {
                    if file.ino != ino.0 {
                        return Err(ArgosError::Invalid(
                            "FUSE file handle inode mismatch".to_string(),
                        ));
                    }
                    shared.flush_inode_writeback_reason(ino.0, WritebackFlushReason::Sync)?;
                    shared.volume.sync()
                });
            match result {
                Ok(()) => reply.ok(),
                Err(err) => reply.error(errno(&err)),
            }
        });
    }

    fn setxattr(
        &self,
        req: &Request,
        ino: INodeNo,
        name: &OsStr,
        value: &[u8],
        flags: i32,
        position: u32,
        reply: ReplyEmpty,
    ) {
        let req = RequestIdentity::from_request(req);
        let name = name.to_owned();
        let value = value.to_owned();
        self.submit_io(ino.0, move |shared| {
            let req = &req;
            let name = name.as_os_str();
            let value = value.as_slice();
            if let Err(err) =
                shared.flush_inode_writeback_reason(ino.0, WritebackFlushReason::Metadata)
            {
                reply.error(errno(&err));
                return;
            }
            let result = (|| -> Result<()> {
                if position != 0 {
                    return Err(ArgosError::Invalid(format!(
                        "unsupported setxattr position {position}"
                    )));
                }

                let supported = libc::XATTR_CREATE | libc::XATTR_REPLACE;
                if flags & !supported != 0
                    || flags & libc::XATTR_CREATE != 0 && flags & libc::XATTR_REPLACE != 0
                {
                    return Err(ArgosError::Invalid(format!(
                        "unsupported setxattr flags {flags:#x}"
                    )));
                }

                let name = xattr_name(name)?;
                shared.require_xattr_write_access(req, ino, name)?;
                let exists = xattr_exists(shared.volume.getxattr_inode(ino.0, name))?;

                if flags & libc::XATTR_CREATE != 0 && exists {
                    return Err(ArgosError::AlreadyExists(format!("xattr {name}")));
                }
                if flags & libc::XATTR_REPLACE != 0 && !exists {
                    return Err(ArgosError::NotFound(format!("xattr {name}")));
                }

                shared.volume.setxattr_inode(ino.0, name, value)
            })();

            match result {
                Ok(()) => reply.ok(),
                Err(err) => reply.error(xattr_errno(&err)),
            }
        });
    }

    fn getxattr(&self, req: &Request, ino: INodeNo, name: &OsStr, size: u32, reply: ReplyXattr) {
        let req = RequestIdentity::from_request(req);
        let name = name.to_owned();
        self.submit_io(ino.0, move |shared| {
            let req = &req;
            let name = name.as_os_str();
            // Xattrs are independent of buffered file data. In particular,
            // Linux probes security.capability before writes; forcing a
            // data flush here defeats write coalescing on each request.
            let result = shared
                .require_access(req, ino, libc::R_OK)
                .and_then(|()| shared.volume.getxattr_inode(ino.0, xattr_name(name)?));
            match result {
                Ok(value) if size == 0 => reply.size(value.len() as u32),
                Ok(value) if value.len() <= size as usize => reply.data(&value),
                Ok(_) => reply.error(Errno::ERANGE),
                Err(err) => reply.error(xattr_errno(&err)),
            }
        });
    }

    fn listxattr(&self, req: &Request, ino: INodeNo, size: u32, reply: ReplyXattr) {
        let req = RequestIdentity::from_request(req);
        self.submit_io(ino.0, move |shared| {
            let req = &req;
            // Xattrs are independent of buffered file data. In particular,
            // Linux probes security.capability before writes; forcing a
            // data flush here defeats write coalescing on each request.
            match shared
                .require_access(req, ino, libc::R_OK)
                .and_then(|()| shared.volume.listxattr_inode(ino.0))
            {
                Ok(names) => {
                    let data = names
                        .into_iter()
                        .flat_map(|name| {
                            let mut bytes = name.into_bytes();
                            bytes.push(0);
                            bytes
                        })
                        .collect::<Vec<_>>();
                    if size == 0 {
                        reply.size(data.len() as u32);
                    } else if data.len() <= size as usize {
                        reply.data(&data);
                    } else {
                        reply.error(Errno::ERANGE);
                    }
                }
                Err(err) => reply.error(errno(&err)),
            }
        });
    }

    fn removexattr(&self, req: &Request, ino: INodeNo, name: &OsStr, reply: ReplyEmpty) {
        let req = RequestIdentity::from_request(req);
        let name = name.to_owned();
        self.submit_io(ino.0, move |shared| {
            let req = &req;
            let name = name.as_os_str();
            if let Err(err) =
                shared.flush_inode_writeback_reason(ino.0, WritebackFlushReason::Metadata)
            {
                reply.error(errno(&err));
                return;
            }
            let result = (|| {
                let name = xattr_name(name)?;
                shared.require_xattr_write_access(req, ino, name)?;
                shared.volume.removexattr_inode(ino.0, name)
            })();
            match result {
                Ok(()) => reply.ok(),
                Err(err) => reply.error(xattr_errno(&err)),
            }
        });
    }

    fn access(&self, req: &Request, ino: INodeNo, mask: AccessFlags, reply: ReplyEmpty) {
        let req = RequestIdentity::from_request(req);
        self.submit_io(ino.0, move |shared| {
            let req = &req;
            match shared.require_access(req, ino, mask.bits()) {
                Ok(()) => reply.ok(),
                Err(err) => reply.error(errno(&err)),
            }
        });
    }

    fn create(
        &self,
        req: &Request,
        parent: INodeNo,
        name: &OsStr,
        mode: u32,
        umask: u32,
        flags: i32,
        reply: ReplyCreate,
    ) {
        let req = RequestIdentity::from_request(req);
        let name = name.to_owned();
        self.submit_io(parent.0, move |shared| {
            let req = &req;
            let name = name.as_os_str();
            if let Err(err) = shared.flush_all_writeback() {
                reply.error(errno(&err));
                return;
            }
            let mut handles = shared.handles.lock();
            let result = shared
                .require_access(req, parent, libc::W_OK | libc::X_OK)
                .and_then(|()| {
                    shared.volume.create_file_at_with_owner(
                        parent.0,
                        name,
                        mode & !umask,
                        req.uid(),
                        req.gid(),
                    )
                })
                .map(|attr| {
                    let handle = handles.open(OpenFileHandle {
                        ino: attr.ino,
                        flags,
                    });
                    (attr, handle)
                });
            drop(handles);
            match result {
                Ok((attr, handle)) => reply.created(
                    &TTL,
                    &to_file_attr(&attr),
                    Generation(0),
                    handle,
                    shared.open_reply_flags(true),
                ),
                Err(err) => reply.error(errno(&err)),
            }
        });
    }

    fn readdir(
        &self,
        req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectory,
    ) {
        let req = RequestIdentity::from_request(req);
        self.submit_io(ino.0, move |shared| {
            let req = &req;
            shared.flush_all_writeback_best_effort();
            match shared
                .require_access(req, ino, libc::R_OK)
                .and_then(|()| shared.directory_listing(ino.0))
            {
                Ok(entries) => {
                    for (idx, entry) in entries
                        .iter()
                        .enumerate()
                        .skip(usize::try_from(offset).unwrap_or(usize::MAX))
                    {
                        let full = reply.add(
                            INodeNo(entry.attr.ino),
                            (idx + 1) as u64,
                            file_type_from_attr(&entry.attr),
                            entry.os_name(),
                        );
                        if full {
                            break;
                        }
                    }
                    reply.ok();
                }
                Err(err) => reply.error(errno(&err)),
            }
        });
    }

    fn readdirplus(
        &self,
        req: &Request,
        ino: INodeNo,
        _fh: FileHandle,
        offset: u64,
        mut reply: ReplyDirectoryPlus,
    ) {
        let req = RequestIdentity::from_request(req);
        self.submit_io(ino.0, move |shared| {
            let req = &req;
            shared.flush_all_writeback_best_effort();
            match shared
                .require_access(req, ino, libc::R_OK | libc::X_OK)
                .and_then(|()| shared.directory_listing(ino.0))
            {
                Ok(entries) => {
                    for (idx, entry) in entries
                        .iter()
                        .enumerate()
                        .skip(usize::try_from(offset).unwrap_or(usize::MAX))
                    {
                        // Child sizes and timestamps can change without a
                        // parent-directory mutation. Never cache a stale
                        // readdirplus attribute with the directory listing.
                        let current = match shared.visible_inode_attr(entry.attr.ino) {
                            Ok(attr) => attr,
                            Err(ArgosError::NotFound(_)) => continue,
                            Err(err) => {
                                reply.error(errno(&err));
                                return;
                            }
                        };
                        let attr = to_file_attr(&current);
                        let full = reply.add(
                            INodeNo(entry.attr.ino),
                            (idx + 1) as u64,
                            entry.os_name(),
                            &TTL,
                            &attr,
                            Generation(0),
                        );
                        if full {
                            break;
                        }
                    }
                    reply.ok();
                }
                Err(err) => reply.error(errno(&err)),
            }
        });
    }
}

fn is_owner_managed_xattr(name: &str) -> bool {
    matches!(
        name,
        acl::POSIX_ACL_ACCESS_XATTR
            | acl::POSIX_ACL_DEFAULT_XATTR
            | acl::ARGOS_POSIX_ACL_ACCESS_XATTR
            | acl::ARGOS_POSIX_ACL_DEFAULT_XATTR
    )
}

fn to_file_attr(attr: &NodeAttr) -> FileAttr {
    FileAttr {
        ino: INodeNo(attr.ino),
        size: attr.size,
        blocks: attr.blocks,
        atime: f64_to_system_time(attr.atime),
        mtime: f64_to_system_time(attr.mtime),
        ctime: f64_to_system_time(attr.ctime),
        crtime: f64_to_system_time(attr.ctime),
        kind: file_type_from_attr(attr),
        perm: (attr.mode & 0o7777) as u16,
        nlink: attr.nlink,
        uid: attr.uid,
        gid: attr.gid,
        rdev: attr.rdev as u32,
        blksize: attr.blksize,
        flags: 0,
    }
}

fn errno(err: &ArgosError) -> Errno {
    Errno::from_i32(err.errno())
}

fn xattr_exists(result: Result<Vec<u8>>) -> Result<bool> {
    match result {
        Ok(_) => Ok(true),
        Err(err) if is_missing_xattr(&err) => Ok(false),
        Err(err) => Err(err),
    }
}

fn xattr_errno(err: &ArgosError) -> Errno {
    if is_missing_xattr(err) {
        Errno::NO_XATTR
    } else {
        errno(err)
    }
}

fn is_missing_xattr(err: &ArgosError) -> bool {
    matches!(err, ArgosError::NotFound(resource) if resource.starts_with("xattr "))
}

fn file_type_from_attr(attr: &NodeAttr) -> FileType {
    match attr.kind {
        NodeKind::Directory => FileType::Directory,
        NodeKind::File => FileType::RegularFile,
        NodeKind::Symlink => FileType::Symlink,
        NodeKind::Special => match attr.mode & libc::S_IFMT {
            value if value == libc::S_IFCHR => FileType::CharDevice,
            value if value == libc::S_IFBLK => FileType::BlockDevice,
            value if value == libc::S_IFIFO => FileType::NamedPipe,
            value if value == libc::S_IFSOCK => FileType::Socket,
            _ => FileType::RegularFile,
        },
    }
}

fn f64_to_system_time(value: f64) -> SystemTime {
    if value <= 0.0 {
        return UNIX_EPOCH;
    }
    let mut seconds = value.floor() as u64;
    let mut micros = ((value - seconds as f64) * 1_000_000.0).round() as u64;
    if micros >= 1_000_000 {
        seconds = seconds.saturating_add(1);
        micros -= 1_000_000;
    }
    UNIX_EPOCH + Duration::new(seconds, (micros * 1_000) as u32)
}

fn system_time_to_f64(time: SystemTime) -> f64 {
    let duration = time.duration_since(UNIX_EPOCH).unwrap_or_default();
    duration.as_secs() as f64 + f64::from(duration.subsec_micros()) / 1_000_000.0
}

fn time_or_now(value: TimeOrNow) -> f64 {
    match value {
        TimeOrNow::SpecificTime(time) => system_time_to_f64(time),
        TimeOrNow::Now => system_time_to_f64(SystemTime::now()),
    }
}

fn is_specific_time(value: &Option<TimeOrNow>) -> bool {
    matches!(value, Some(TimeOrNow::SpecificTime(_)))
}

fn is_setid_clear_only(current_mode: u32, requested_mode: u32) -> bool {
    let current = current_mode & 0o7777;
    let requested = requested_mode & 0o7777;
    let cleared = current & !(libc::S_ISUID | libc::S_ISGID);
    requested == cleared && requested != current
}

fn open_mask(flags: OpenFlags) -> i32 {
    let mut mask = match flags.0 & libc::O_ACCMODE {
        libc::O_RDONLY => libc::R_OK,
        libc::O_WRONLY => libc::W_OK,
        libc::O_RDWR => libc::R_OK | libc::W_OK,
        _ => libc::R_OK,
    };
    if flags.0 & libc::O_TRUNC != 0 {
        mask |= libc::W_OK;
    }
    mask
}

fn xattr_name(name: &OsStr) -> Result<&str> {
    name.to_str()
        .ok_or_else(|| ArgosError::Invalid("non-UTF-8 xattr names are unsupported".to_string()))
}

#[cfg(test)]
#[path = "fusefs_full_stripe_tests.rs"]
mod fusefs_full_stripe_tests;
#[cfg(test)]
#[path = "fusefs_non_eof_tests.rs"]
mod fusefs_non_eof_tests;
#[cfg(test)]
#[path = "fusefs_read_overlay_tests.rs"]
mod fusefs_read_overlay_tests;
#[cfg(test)]
#[path = "fusefs_writeback_overlay_tests.rs"]
mod fusefs_writeback_overlay_tests;
#[cfg(test)]
#[path = "fusefs_tests.rs"]
mod tests;
