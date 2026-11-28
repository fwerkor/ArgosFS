use crate::acl;
use crate::advanced_io;
use crate::allocator;
use crate::autopilot::{plan_background_io, AutopilotMode, AutopilotPolicy, BackgroundIoPolicy};
use crate::backend::{FileBlockBackend, HostFsBackend, StorageBackend};
use crate::cache::BlockCache;
use crate::commit_metrics::{CheckpointReason, CommitMetrics, CommitPhase, DataPlaneByteKind};
use crate::compression::{compress, decompress};
use crate::crypto;
use crate::erasure::RsCodec;
use crate::error::{ArgosError, Result};
use crate::health::{
    classify_inode, probe_disk_path, recent_io_error_spike, refresh_smart, risk_report,
    update_smart_evidence_for_observations, SmartCounterObservations,
};
pub use crate::inode_ops::{DirEntry, NodeAttr, RenamePolicy};
use crate::journal;
use crate::raw_format::{self, RawSuperblock};
use crate::raw_store;
use crate::storage::guard::GuardedBackend;
use crate::types::*;
use verify_after_write::VerifyAfterWrite;

mod verify_after_write;
use crate::util::{
    append_json_line, atomic_write, clean_path, configured_offline_grace_ms,
    configured_read_retries, content_hash_hex, content_hash_matches, ensure_dir,
    ensure_private_dir, now_f64, parent_name, relative_or_absolute, retry_verified, split_path,
    stable_u01,
};
use parking_lot::{Mutex, RwLock};
use serde_json::json;
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use uuid::Uuid;

/// Physical verification must never accept or populate a reconstructed block cache entry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BlockCachePolicy {
    Normal,
    Bypass,
}

/// The time of an acknowledged FUSE buffered write and the inode ctime as
/// observed when that write entered the buffer. A subsequent unlink or other
/// metadata mutation must retain its newer ctime even if the data is flushed
/// later. The snapshot is per acknowledged write, not per disk flush.
#[derive(Clone, Copy, Debug)]
pub(crate) struct BufferedWriteTime {
    pub(crate) at: f64,
    pub(crate) seen_ctime: Option<f64>,
}

impl BufferedWriteTime {
    fn persisted_ctime(self, current_ctime: f64) -> f64 {
        if self.seen_ctime.is_some_and(|seen| seen != current_ctime) {
            current_ctime
        } else {
            self.at
        }
    }
}

thread_local! {
    static THREAD_BULK_IMPORT_MODE: Cell<Option<bool>> = const { Cell::new(None) };
}

pub struct BulkImportModeGuard {
    previous: Option<bool>,
}

impl Drop for BulkImportModeGuard {
    fn drop(&mut self) {
        THREAD_BULK_IMPORT_MODE.with(|value| value.set(self.previous));
    }
}

pub fn bulk_import_scope(enabled: bool) -> BulkImportModeGuard {
    let previous = THREAD_BULK_IMPORT_MODE.with(|value| value.replace(Some(enabled)));
    BulkImportModeGuard { previous }
}

fn bulk_import_enabled() -> bool {
    THREAD_BULK_IMPORT_MODE
        .with(|value| value.get())
        .unwrap_or_else(|| std::env::var_os("ARGOSFS_BULK_IMPORT_COMMIT").is_some())
}

const ROOT_INO: InodeId = 1;
const NON_UTF8_NAME_PREFIX: &str = ".argosfs-name-nonutf8-v3:";
const ESCAPED_UTF8_NAME_PREFIX: &str = ".argosfs-name-utf8-v3:";
const LEGACY_NON_UTF8_NAME_PREFIX: &str = "\0argosfs-name-hex:";
const NON_UTF8_SYMLINK_TARGET_PREFIX: &str = "\0argosfs-symlink-target-hex:";
const BOOT_CRITICAL_XATTR: &str = "system.argosfs.boot_critical";
const DEFAULT_LAYOUT_ID: &str = "layout-0000";
const SHARD_CHECKSUM_BLOCK_SIZE: usize = 256 * 1024;
const INLINE_DATA_MAX: usize = 512;
pub(crate) const MAX_IN_MEMORY_IO_BYTES: usize = 256 * 1024 * 1024;

mod autopilot;
pub(crate) mod capacity;
mod data_plane;
mod decode_flights;
mod lifecycle;
mod metadata_lock;
mod namespace_mutation;
mod quota;
mod raw_parallel_write;
mod transactions;
use metadata_lock::MetadataLock;
#[cfg(test)]
mod deferred_flush_tests;
mod helpers;
mod maintenance;
mod maintenance_key;
mod member_balance;
mod metadata_budget;
mod namespace;
mod read_advisory;
mod shard_read_batch;
mod shard_repair;
#[cfg(test)]
mod shard_repair_tests;
use shard_repair::RepairCounters;
#[cfg(test)]
mod paged_metadata_tests;
#[cfg(test)]
mod read_advisory_tests;
#[cfg(test)]
mod read_cache_range_tests;
#[cfg(test)]
mod read_checkpoint_tests;
#[cfg(test)]
mod read_fast_tests;
#[cfg(test)]
mod tests;

#[cfg(test)]
#[path = "data_fanout_tests.rs"]
mod stripe_fanout_tests;

pub use autopilot::{AutopilotConfig, ReshapeReport};
pub(crate) use capacity::CapacitySnapshot;
use helpers::*;
pub use member_balance::{MemberBalanceOptions, MemberBalanceReport};
use namespace::*;

#[derive(Clone, Debug)]
struct DeferredCommitState {
    durable_metadata: Option<Arc<Metadata>>,
    durability_cache: Option<DurabilityValidationCache>,
    raw_uncommitted_metadata_dirty: bool,
    // Read/access/latency counters are advisory and may be lost at a crash.
    // Their live metadata changes must not independently request a full
    // checkpoint; they still must be included when constructing the trusted
    // previous hash for any later *real* journal transaction.
    raw_advisory_metadata_dirty: bool,
    // Recovery normalization requires a full one-time checkpoint before
    // establishing a trusted v2 delta baseline.
    raw_checkpoint_required: bool,
    // A clean, complete v2 mount may keep its already durable journal tip
    // until a real mutation requires a canonical checkpoint. This is not
    // used for recovery after an error, degraded discovery or formatting.
    raw_mount_reanchor_pending: bool,
    dirty_transactions: u64,
    dirty_since: Option<Instant>,
    pending_reclaims: Vec<FileBlock>,
    last_error: Option<String>,
}

/// A checked data-durability image. Its copy-on-write inode baseline lets
/// subsequent group commits inspect only modified inodes, provided the disk
/// availability and layout definitions are unchanged. A changed condition
/// forces a full validation, never a speculative fast path.
#[derive(Clone, Debug)]
struct DurabilityValidationCache {
    inodes: crate::model::inodes::TrackedInodes,
    disk_statuses: BTreeMap<String, DiskStatus>,
    layouts: Vec<u8>,
    unavailable: BTreeSet<String>,
    #[cfg(test)]
    last_scanned_blocks: usize,
}

impl DeferredCommitState {
    fn new(meta: &Metadata) -> Self {
        Self::new_with_raw_dirty(meta, false)
    }

    fn has_pending_sync(&self) -> bool {
        self.dirty_transactions > 0
            || !self.pending_reclaims.is_empty()
            || (!self.raw_mount_reanchor_pending
                && (self.raw_uncommitted_metadata_dirty || self.raw_checkpoint_required))
    }

    fn new_with_raw_dirty(meta: &Metadata, raw_uncommitted_metadata_dirty: bool) -> Self {
        Self {
            durable_metadata: (meta.backend != BackendKind::Host).then(|| Arc::new(meta.clone())),
            durability_cache: None,
            raw_uncommitted_metadata_dirty,
            raw_advisory_metadata_dirty: false,
            raw_checkpoint_required: raw_uncommitted_metadata_dirty,
            raw_mount_reanchor_pending: false,
            dirty_transactions: 0,
            dirty_since: None,
            pending_reclaims: Vec::new(),
            last_error: None,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct QuotaRuntime {
    active: imbl::OrdSet<InodeId>,
    usage_bytes: imbl::OrdMap<InodeId, u128>,
    // Number of reachable directory entries for each (inode, quota).
    // A file with multiple hard links is charged only on the first edge.
    memberships: imbl::OrdMap<InodeId, imbl::OrdMap<InodeId, u64>>,
}

impl QuotaRuntime {
    fn from_metadata(meta: &Metadata) -> Result<Self> {
        let mut runtime = Self::default();
        for (ino, inode) in &meta.inodes {
            if inode.quota_bytes.is_some() {
                runtime.active.insert(*ino);
                runtime.usage_bytes.insert(*ino, 0);
                runtime.change_edge(meta, *ino, *ino, true)?;
            }
        }
        Ok(runtime)
    }
}

type DirectoryParentIndex = BTreeMap<InodeId, (InodeId, String)>;

/// Mount-local state shared by every handle, independent of backend configuration.
struct MountRuntime {
    read_advisory: read_advisory::ReadAdvisory,
    commit_metrics: CommitMetrics,
    dirty_host_shards: Mutex<BTreeSet<PathBuf>>,
    inode_locks: Mutex<BTreeMap<InodeId, Arc<Mutex<()>>>>,
    decode_flights: decode_flights::DecodeFlights,
    read_retries_seen: AtomicU64,
    read_recovered: AtomicU64,
    repair_counters: RepairCounters,
    mount_owners: AtomicUsize,
    read_retries: usize,
    /// Directory child -> (parent, encoded entry name). Validate by exact
    /// lookup rather than scanning a potentially very wide parent.
    parent_index: RwLock<Option<DirectoryParentIndex>>,
}

impl MountRuntime {
    fn new() -> Self {
        Self {
            read_advisory: read_advisory::ReadAdvisory::default(),
            commit_metrics: CommitMetrics::default(),
            dirty_host_shards: Mutex::new(BTreeSet::new()),
            inode_locks: Mutex::new(BTreeMap::new()),
            decode_flights: decode_flights::DecodeFlights::default(),
            read_retries_seen: AtomicU64::new(0),
            read_recovered: AtomicU64::new(0),
            repair_counters: RepairCounters::default(),
            mount_owners: AtomicUsize::new(1),
            read_retries: configured_read_retries(),
            parent_index: RwLock::new(None),
        }
    }
}

pub struct ArgosFs {
    root: Arc<PathBuf>,
    backend: Arc<dyn StorageBackend>,
    backend_writable: bool,
    raw_superblocks: Arc<Vec<RawSuperblock>>,
    paged_metadata: Option<Arc<Mutex<crate::storage::raw::paged_metadata::ImageStore>>>,
    meta: Arc<MetadataLock>,
    /// Advisory statfs view republished after durable metadata commits.  It is
    /// intentionally independent of `meta` so statfs cannot block on a slow
    /// checkpoint that holds the metadata writer lock.
    capacity_snapshot: Arc<RwLock<Option<CapacitySnapshot>>>,
    quota_runtime: Arc<RwLock<QuotaRuntime>>,
    deferred_commit: Arc<Mutex<DeferredCommitState>>,
    quarantined_devices: Arc<Mutex<BTreeSet<String>>>,
    quarantine_since: Arc<Mutex<BTreeMap<String, f64>>>,
    write_fence: Arc<Mutex<Option<String>>>,
    cache: Arc<BlockCache>,
    verify_written: Option<Arc<VerifyAfterWrite>>,

    runtime: Arc<MountRuntime>,
}

impl Clone for ArgosFs {
    fn clone(&self) -> Self {
        self.runtime.mount_owners.fetch_add(1, Ordering::Relaxed);
        Self {
            root: self.root.clone(),
            backend: self.backend.clone(),
            backend_writable: self.backend_writable,
            raw_superblocks: self.raw_superblocks.clone(),
            paged_metadata: self.paged_metadata.clone(),
            meta: self.meta.clone(),
            capacity_snapshot: self.capacity_snapshot.clone(),
            quota_runtime: self.quota_runtime.clone(),
            deferred_commit: self.deferred_commit.clone(),
            quarantined_devices: self.quarantined_devices.clone(),
            quarantine_since: self.quarantine_since.clone(),
            write_fence: self.write_fence.clone(),
            cache: self.cache.clone(),
            verify_written: self.verify_written.clone(),
            runtime: self.runtime.clone(),
        }
    }
}

impl Drop for ArgosFs {
    fn drop(&mut self) {
        // A clone can outlive another mount handle (for example a metrics
        // request). Only the final handle may stop the shared verifier.
        if self.runtime.mount_owners.fetch_sub(1, Ordering::AcqRel) != 1 {
            return;
        }
        // The worker owns a metadata Arc, so stop it before clean-unmount.
        if let Some(verify) = self.verify_written.take() {
            verify.stop();
        }
        if !self.backend_writable {
            return;
        }
        if self.meta.read().backend != BackendKind::Host {
            let _ = self.mark_clean_unmount();
        }
    }
}

#[derive(Clone, Debug)]
struct PlacementRequest<'a> {
    key: &'a str,
    count: usize,
    storage_class: StorageTier,
    boot_critical: bool,
    exclude_disks: &'a BTreeSet<String>,
    required_bytes: u64,
}

#[derive(Clone)]
struct ShardIntegrity {
    sha256: String,
    checksum_block_size: usize,
    subblock_sha256: Vec<String>,
}

impl ArgosFs {
    pub fn verify_written_failures(&self) -> u64 {
        self.verify_written
            .as_ref()
            .map(|verify| verify.failures())
            .unwrap_or(0)
    }

    pub fn verify_written_skipped(&self) -> u64 {
        self.verify_written
            .as_ref()
            .map(|verify| verify.skipped())
            .unwrap_or(0)
    }

    /// Finish the delayed verifier before producing an offline maintenance
    /// report. Call only after all repairs have completed and sync succeeded.
    pub fn finish_maintenance_verification(&self, report: &mut FsckReport) {
        if let Some(verify) = &self.verify_written {
            verify.drain();
            report.write_verify_checks = verify.checks();
            report.write_verify_retries = verify.retries();
            report.write_verify_failures = verify.failures();
            report.write_verify_skipped = verify.skipped();
            if report.write_verify_failures != 0 {
                report.errors.push(format!(
                    "read-after-write verification failed for {} shard(s)",
                    report.write_verify_failures
                ));
            }
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Base directory for raw-device L2 caches. Root uses
    /// `/var/cache/argosfs/blocks`; unprivileged pools use the user's cache
    /// directory.
    pub fn default_cache_base() -> PathBuf {
        helpers::default_raw_cache_base()
    }

    /// Base directory for loop-image L2 caches, colocated with the images.
    pub fn loop_cache_base(paths: &[PathBuf]) -> PathBuf {
        helpers::loop_cache_base(paths)
    }

    /// The pool's L2 cache directory.
    pub fn cache_dir(&self) -> &Path {
        self.cache.root()
    }

    /// On-disk L2 footprint. This walks the cache directory and is meant for
    /// on-demand inspection, not the metrics hot path.
    pub fn cache_disk_footprint(&self) -> Result<(u64, u64)> {
        self.cache.disk_footprint()
    }

    /// Lock-free with respect to metadata commits. Safe to expose alongside
    /// /commit-metrics while profiling warm/cold FUSE reads.
    pub(crate) fn read_cache_stats(&self) -> BTreeMap<String, serde_json::Value> {
        self.cache.stats()
    }

    pub(crate) fn commit_metrics(&self) -> &CommitMetrics {
        &self.runtime.commit_metrics
    }

    pub fn metadata_snapshot(&self) -> Metadata {
        let guard = self.meta.read();
        let mut snapshot = guard.clone();
        self.runtime.read_advisory.project(&mut snapshot);
        snapshot
    }

    /// Nonblocking metadata observation for advisory queries such as statfs.
    /// A checkpoint may hold the writer lock for slow device I/O.
    pub(crate) fn try_metadata_snapshot(&self) -> Option<Metadata> {
        self.meta.try_read().map(|meta| {
            let mut snapshot = meta.clone();
            self.runtime.read_advisory.project(&mut snapshot);
            snapshot
        })
    }

    /// Last capacity published by a durable metadata commit.  statfs falls
    /// back to this without taking the metadata lock, so a long checkpoint
    /// cannot keep `df` frozen at a mount-time value.
    pub(crate) fn published_capacity_snapshot(&self) -> Option<CapacitySnapshot> {
        self.capacity_snapshot.read().clone()
    }

    /// Republish advisory capacity from `meta`, which the caller must hold for
    /// writing.  Host volumes keep their existing statfs path: their fallback
    /// accounting may scan every inode, which must not run on each mutation.
    fn publish_capacity_snapshot_locked(&self, meta: &Metadata) {
        if meta.backend == BackendKind::Host {
            return;
        }
        let unavailable = self.runtime_unavailable_devices();
        let snapshot = CapacitySnapshot::from_metadata(self.root.as_path(), meta, unavailable);
        *self.capacity_snapshot.write() = Some(snapshot);
    }

    #[cfg(test)]
    pub(crate) fn test_with_metadata_write_lock(&self, action: impl FnOnce()) {
        let _guard = self.meta.write();
        action();
    }

    /// Explicit, offline-only, one-way upgrade of every active raw/loop
    /// member. Call only after unmounting all other processes using this pool;
    /// this API cannot fence an unrelated legacy daemon already mounted.
    pub fn upgrade_delta_integrity(&mut self) -> Result<()> {
        if self.paged_metadata.is_some() {
            return Ok(());
        }

        if self.runtime.mount_owners.load(Ordering::Acquire) != 1 {
            return Err(ArgosError::Conflict(
                "raw integrity upgrade requires a unique volume handle".to_string(),
            ));
        }
        self.sync()?;
        let meta = self.meta.read();
        self.ensure_block_backend_writable_locked(&meta)?;
        if meta.backend == BackendKind::Host {
            return Err(ArgosError::Unsupported(
                "delta integrity upgrade requires raw/loop backend".to_string(),
            ));
        }
        let superblocks = self.metadata_superblocks_locked(&meta)?;
        let upgraded = if self.open_backend_covers_superblocks(&superblocks) {
            raw_store::upgrade_delta_integrity_members(&*self.backend, &superblocks, &meta)?
        } else {
            let backend = self.metadata_block_backend_locked(&meta, &superblocks, true)?;
            raw_store::upgrade_delta_integrity_members(&*backend, &superblocks, &meta)?
        };
        // Only activate new group commits when both copies on ALL active
        // members have been durably upgraded. A partially failed migration
        // leaves the old canonical integrity scheme in use and is retryable.
        self.raw_superblocks = Arc::new(upgraded);
        Ok(())
    }

    pub fn transaction_report(&self) -> Result<TransactionReport> {
        let meta = self.meta.read();
        if meta.backend != BackendKind::Host {
            let superblocks = self.metadata_superblocks_locked(&meta)?;
            if self.open_backend_covers_superblocks(&superblocks) {
                return raw_store::audit(&*self.backend, &superblocks);
            }
            let backend = self.metadata_block_backend_locked(&meta, &superblocks, false)?;
            return raw_store::audit(&*backend, &superblocks);
        }
        journal::scan(&self.root)
    }

    /// Reset journal regions that only contain a stale invalid tail.
    ///
    /// Recovery selects the newest durable metadata checkpoint and replays a
    /// newer journal snapshot when one exists.  If no replay was needed, every
    /// valid journal record is at or below that checkpoint; an invalid tail
    /// then cannot describe committed state.  Clearing it lets `fsck --repair`
    /// or `replay-journal` leave a raw pool mountable writable instead of
    /// tripping the preflight "invalid journal entries" check forever.
    ///
    /// Returns the number of journal regions reset.  A pool with no invalid
    /// entries, a host backend, or a journal that still needs replay is left
    /// untouched.  The caller must have written its metadata checkpoint first
    /// (`ArgosFs::sync`), so the journal reset cannot roll back a repair.
    pub fn reset_stale_journals(&self) -> Result<u64> {
        let report = self.transaction_report()?;
        if report.invalid_entries == 0 || report.replayed {
            return Ok(0);
        }
        if !self.backend_writable {
            return Err(ArgosError::PermissionDenied(
                "volume is read-only; cannot reset stale journals".to_string(),
            ));
        }
        let meta = self.meta.read();
        if meta.backend == BackendKind::Host {
            return Ok(0);
        }
        let superblocks = self.metadata_superblocks_locked(&meta)?;
        let mut stale = Vec::new();
        for member in &report.raw_journal_members {
            if member.invalid_entries == 0 {
                continue;
            }
            if member.last_valid_txid > meta.txid
                || member.last_valid_generation > meta.integrity.generation
            {
                return Err(ArgosError::CorruptedMetadata(format!(
                    "journal on {} contains records newer than the durable checkpoint",
                    member.disk_id
                )));
            }
            if let Some(sb) = superblocks.iter().find(|sb| sb.disk_id == member.disk_id) {
                stale.push(sb.clone());
            }
        }
        if stale.is_empty() {
            return Ok(0);
        }
        if self.open_backend_covers_superblocks(&stale) {
            raw_store::reset_journals_after_checkpoint(&*self.backend, &stale, &meta)
        } else {
            let backend = self.metadata_block_backend_locked(&meta, &superblocks, true)?;
            raw_store::reset_journals_after_checkpoint(&*backend, &stale, &meta)
        }
    }

    fn is_device_quarantined(&self, disk_id: &str) -> bool {
        self.quarantined_devices.lock().contains(disk_id)
    }

    fn quarantine_device(&self, disk_id: &str) {
        self.quarantine_since
            .lock()
            .entry(disk_id.to_string())
            .or_insert_with(now_f64);
        self.quarantined_devices.lock().insert(disk_id.to_string());
    }

    fn quarantined_member_due_for_repair(&self, disk_id: &str) -> bool {
        let grace_ms = configured_offline_grace_ms();
        if grace_ms == 0 {
            return true;
        }
        self.quarantine_since
            .lock()
            .get(disk_id)
            .is_some_and(|since| now_f64() - since >= grace_ms as f64 / 1000.0)
    }

    fn apply_quorum_write_report(
        &self,
        meta: &Metadata,
        report: raw_store::QuorumWriteReport,
    ) -> Result<()> {
        if report.unavailable_devices.is_empty() {
            return Ok(());
        }
        for disk_id in &report.unavailable_devices {
            self.quarantine_device(disk_id);
        }
        let unavailable = self.quarantined_devices.lock().clone();
        match self.validate_data_durability_locked(meta, &unavailable) {
            Ok(()) => Ok(()),
            Err(err) => {
                self.fence_on_quorum_loss(&err);
                Err(ArgosError::CommittedDurabilityLoss(err.to_string()))
            }
        }
    }

    pub(crate) fn runtime_unavailable_devices(&self) -> BTreeSet<String> {
        self.quarantined_devices.lock().clone()
    }

    fn fence_on_quorum_loss(&self, err: &ArgosError) {
        if matches!(
            err,
            ArgosError::QuorumUnavailable { .. }
                | ArgosError::IndeterminateCommit(_)
                | ArgosError::CommittedDurabilityLoss(_)
        ) {
            let mut fence = self.write_fence.lock();
            if fence.is_none() {
                *fence = Some(err.to_string());
            }
        }
    }

    fn device_error_should_quarantine(err: &ArgosError) -> bool {
        err.is_fatal_device_error()
    }

    fn inode_lock(&self, ino: InodeId) -> Arc<Mutex<()>> {
        self.runtime
            .inode_locks
            .lock()
            .entry(ino)
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    pub fn audit_transactions(root: impl AsRef<Path>) -> Result<TransactionReport> {
        journal::load_or_recover(root.as_ref()).map(|recovered| recovered.report)
    }

    pub fn snapshot(&self, name: &str) -> Result<PathBuf> {
        let mut meta = self.meta.write();
        let trimmed = name.trim();
        if trimmed.is_empty() {
            return Err(ArgosError::Invalid(
                "snapshot name must not be empty".to_string(),
            ));
        }
        let safe: String = trimmed
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                    ch
                } else {
                    '_'
                }
            })
            .collect();
        if safe == "." || safe == ".." {
            return Err(ArgosError::Invalid(format!(
                "invalid snapshot name: {trimmed:?}"
            )));
        }
        let path = self
            .root
            .join(".argosfs/snapshots")
            .join(format!("{safe}.json"));
        if path.exists() {
            return Err(ArgosError::AlreadyExists(format!(
                "snapshot {}",
                path.display()
            )));
        }
        atomic_write(&path, serde_json::to_vec_pretty(&*meta)?.as_slice())?;
        self.journal_locked(
            &mut meta,
            "snapshot",
            json!({"name": trimmed, "path": path}),
        )?;
        Ok(path)
    }

    pub fn iter_paths(&self) -> Vec<(String, InodeId)> {
        self.iter_path_bytes()
            .into_iter()
            .map(|(path, ino)| (String::from_utf8_lossy(&path).to_string(), ino))
            .collect()
    }

    pub fn iter_path_bytes(&self) -> Vec<(Vec<u8>, InodeId)> {
        let meta = self.meta.read();
        let mut out = vec![(b"/".to_vec(), ROOT_INO)];

        fn walk(meta: &Metadata, out: &mut Vec<(Vec<u8>, InodeId)>, prefix: &[u8], ino: InodeId) {
            if let Some(inode) = meta.inodes.get(&ino) {
                if inode.kind != NodeKind::Directory {
                    return;
                }
                for (name, child) in inode.entries.iter() {
                    let name_bytes = decode_entry_name_bytes(name);
                    let mut path = Vec::new();
                    if prefix == b"/" {
                        path.push(b'/');
                        path.extend_from_slice(&name_bytes);
                    } else {
                        path.extend_from_slice(prefix);
                        path.push(b'/');
                        path.extend_from_slice(&name_bytes);
                    }
                    out.push((path.clone(), *child));
                    walk(meta, out, &path, *child);
                }
            }
        }

        walk(&meta, &mut out, b"/", ROOT_INO);
        out
    }

    fn file_window(&self, cursor: Option<InodeId>, max_files: usize) -> Vec<(InodeId, Inode)> {
        if max_files == 0 {
            return Vec::new();
        }
        let meta = self.meta.read();
        // HashMap iteration is deliberately unordered. Scrub/rebalance cursors
        // must advance in numeric inode order, and cloning every file's shard
        // records merely to select 128 entries can exceed the memory budget.
        let mut ids = meta
            .inodes
            .iter()
            .filter_map(|(ino, inode)| (inode.kind == NodeKind::File).then_some(*ino))
            .collect::<Vec<_>>();
        if ids.is_empty() {
            return Vec::new();
        }
        ids.sort_unstable();
        let start = cursor
            .map(|value| ids.partition_point(|ino| *ino <= value))
            .filter(|start| *start < ids.len())
            .unwrap_or(0);
        ids.iter()
            .cycle()
            .skip(start)
            .take(max_files.min(ids.len()))
            .filter_map(|ino| meta.inodes.get(ino).map(|inode| (*ino, inode.clone())))
            .collect()
    }

    fn shard_path_locked(&self, meta: &Metadata, disk_id: &str, relpath: &Path) -> PathBuf {
        let disk = meta.disks.get(disk_id).expect("disk metadata exists");
        relative_or_absolute(&self.root, &disk.path).join(relpath)
    }

    fn shard_path_if_disk_exists_locked(
        &self,
        meta: &Metadata,
        disk_id: &str,
        relpath: &Path,
    ) -> Option<PathBuf> {
        let disk = meta.disks.get(disk_id)?;
        Some(relative_or_absolute(&self.root, &disk.path).join(relpath))
    }

    fn encryption_key_locked(&self, meta: &Metadata) -> Result<[u8; 32]> {
        let passphrase = crypto::passphrase_from_env()?.ok_or_else(|| {
            ArgosError::PermissionDenied(
                "set ARGOSFS_KEY or ARGOSFS_KEY_FILE to access encrypted ArgosFS data".to_string(),
            )
        })?;
        crypto::derive_key_for_config(&meta.encryption, &passphrase, meta.uuid.as_bytes())
    }

    fn open_backend_covers_superblocks(&self, superblocks: &[RawSuperblock]) -> bool {
        let Ok(devices) = self.backend.list_devices() else {
            return false;
        };
        let opened = devices
            .into_iter()
            .map(|device| device.device_id)
            .collect::<BTreeSet<_>>();
        superblocks.iter().all(|sb| opened.contains(&sb.disk_id))
    }

    fn metadata_superblocks_locked(&self, meta: &Metadata) -> Result<Vec<RawSuperblock>> {
        if meta.backend == BackendKind::Host {
            return Ok(Vec::new());
        }
        let quarantined = self.quarantined_devices.lock().clone();
        let mut superblocks = self
            .raw_superblocks
            .iter()
            .filter(|sb| {
                !quarantined.contains(&sb.disk_id)
                    && meta
                        .disks
                        .get(&sb.disk_id)
                        .is_some_and(|disk| disk.status != DiskStatus::Removed)
            })
            .cloned()
            .collect::<Vec<_>>();
        let mut seen = superblocks
            .iter()
            .map(|sb| sb.disk_id.clone())
            .collect::<BTreeSet<_>>();
        let mut seen_device_uuid = superblocks
            .iter()
            .map(|sb| sb.device_uuid)
            .collect::<BTreeSet<_>>();
        let expected_pool_uuid = Uuid::parse_str(&meta.uuid)
            .map_err(|err| ArgosError::Invalid(format!("invalid pool UUID: {err}")))?;
        for (disk_id, disk) in &meta.disks {
            if seen.contains(disk_id)
                || quarantined.contains(disk_id)
                || disk.status == DiskStatus::Removed
            {
                continue;
            }
            // Metadata membership is independent of data-placement status. An
            // administratively Offline/Failed member that is still physically
            // reachable may still carry the membership-changing transaction.
            // If a dynamically added member cannot be inspected, quarantine it
            // rather than silently omitting it: deferred data may already reference
            // shards placed there, and durability validation must count it missing.
            match raw_store::inspect_device(meta.backend, disk.path.clone()) {
                Ok((superblock, _))
                    if superblock.pool_uuid == expected_pool_uuid
                        && superblock.disk_id == *disk_id
                        && !seen.contains(&superblock.disk_id)
                        && seen_device_uuid.insert(superblock.device_uuid) =>
                {
                    seen.insert(disk_id.clone());
                    superblocks.push(superblock);
                }
                Ok(_) | Err(_) => self.quarantine_device(disk_id),
            }
        }
        Ok(superblocks)
    }

    fn metadata_block_backend_locked(
        &self,
        meta: &Metadata,
        superblocks: &[RawSuperblock],
        write: bool,
    ) -> Result<Arc<dyn StorageBackend>> {
        if meta.backend == BackendKind::Host {
            return Err(ArgosError::Unsupported(
                "host backend has no block device set".to_string(),
            ));
        }
        let devices = superblocks
            .iter()
            .map(|sb| {
                let disk = meta
                    .disks
                    .get(&sb.disk_id)
                    .ok_or_else(|| ArgosError::MissingDevice(sb.disk_id.clone()))?;
                Ok((sb.disk_id.clone(), disk.path.clone()))
            })
            .collect::<Result<Vec<_>>>()?;
        let inner: Arc<dyn StorageBackend> = Arc::new(FileBlockBackend::open_with_ids(
            meta.backend,
            devices,
            write,
        )?);
        Ok(Arc::new(GuardedBackend::new(inner)))
    }

    fn attr_from_inode(inode: &Inode, chunk_size: usize) -> NodeAttr {
        NodeAttr {
            ino: inode.id,
            kind: inode.kind.clone(),
            mode: inode.mode,
            uid: inode.uid,
            gid: inode.gid,
            nlink: inode.nlink,
            size: inode.size,
            rdev: inode.rdev,
            atime: inode.atime,
            mtime: inode.mtime,
            ctime: inode.ctime,
            blocks: if inode.kind == NodeKind::File {
                inode.size.div_ceil(512)
            } else {
                0
            },
            blksize: u32::try_from(chunk_size).unwrap_or(u32::MAX),
        }
    }
}
