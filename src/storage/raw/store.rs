use super::wire::{self, put_u32, put_u64};
mod checkpoint;
use checkpoint::*;
mod journal_io;
use journal_io::*;
mod replay;
use replay::*;
mod device;
use device::*;

pub use checkpoint::write_metadata_copies;
pub(crate) use checkpoint::write_metadata_copies_quorum;
pub(crate) use checkpoint::write_metadata_copies_quorum_measured;
pub use device::inspect_device;
pub use device::scan_paths;
pub use device::superblock_for_device;
pub use journal_io::reset_journals_after_checkpoint;

use crate::backend::{FileBlockBackend, StorageBackend};
use crate::commit_metrics::{CheckpointReason, CommitMetrics, CommitPhase, RawWriteKind};
use crate::error::{ArgosError, Result};
use crate::journal;
use crate::raw_format::{
    align_down, align_up, RawDeviceLabel, RawSuperblock, BACKUP_REGION_SIZE, DEVICE_LABEL_OFFSET,
    DEVICE_LABEL_SIZE, PRIMARY_SUPERBLOCK_OFFSET, PROTECTIVE_HEADER_OFFSET,
    REQUIRED_FEATURE_ARRAY_SPLICE, REQUIRED_FEATURE_DELTA_INTEGRITY, SUPERBLOCK_SIZE,
};
use crate::storage::guard::GuardedBackend;
use crate::types::{
    BackendKind, DiskStatus, FaultPoint, Metadata, MetadataCandidateReport, RawJournalMemberReport,
    TransactionReport,
};
use crate::util::{now_f64, sha256_hex};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use uuid::Uuid;

const PROTECTIVE_MAGIC: &[u8; 16] = b"ARGOSFS-RAW-HD\0\0";
const METADATA_MAGIC: &[u8; 16] = b"ARGOSFS-RAW-MD\0\0";
const JOURNAL_MAGIC: &[u8; 16] = b"ARGOSFS-RAW-JN\0\0";
const RAW_STORE_VERSION: u32 = 1;
const RAW_QUORUM_COMMIT_ACTION: &str = "__argosfs_quorum_commit__";
const RAW_HEADER_SIZE: usize = 4096;
const METADATA_FORMAT_LEGACY: u32 = 0;
const METADATA_FORMAT_TREE: u32 = 1;
const METADATA_FORMAT_TREE_ZSTD: u32 = 2;
const METADATA_DECOMPRESSED_MAX: u64 = 4 * 1024 * 1024 * 1024;
const CHECKPOINT_ZSTD_LEVEL: i32 = 3;
const METADATA_PAGE_SIZE: usize = 4096;
// Prepare contiguous writes once for all members and both checkpoint slots.
// 1 MiB bounds transient buffers and keeps raw/USB requests manageable.
const CHECKPOINT_BATCH_BYTES: usize = 1024 * 1024;
// Only preflight a reasonably sized stored checkpoint. If the old payload is
// larger, retain the conservative dual-slot rollover. Streaming verification
// can lift this bound without changing the on-disk format.
const CHECKPOINT_PRESERVE_VALIDATION_MAX_BYTES: u64 = 256 * 1024 * 1024;
const METADATA_INDEX_ENTRY_SIZE: usize = 48;
const HASH_HEX_LEN: usize = 64;
const HASH_BYTES_LEN: usize = 32;
// A USB card can answer a scan with a transient I/O error or a short/zero
// read while it wakes up.  Retry before deciding that the member is gone:
// a false "missing" marks the member Offline in the pool metadata.
const RAW_SCAN_ATTEMPTS: usize = 3;
const RAW_SCAN_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(50);

#[derive(Clone)]
pub struct RawOpen {
    /// Recovery selected a verified state and all requested dirty flags were written.
    pub recovery_verified: bool,
    /// Exact recovered v2 image, before device discovery changes live fields.
    pub(crate) durable_metadata: Option<Metadata>,
    pub backend: Arc<dyn StorageBackend>,
    pub metadata: Metadata,
    pub report: TransactionReport,
    pub superblocks: Vec<RawSuperblock>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct QuorumWriteReport {
    pub failed_devices: BTreeMap<String, String>,
    pub unavailable_devices: BTreeSet<String>,
    disk_full: Option<(String, u64, u64)>,
}

impl QuorumWriteReport {
    fn record_failure(&mut self, disk_id: &str, err: &ArgosError) {
        self.failed_devices
            .insert(disk_id.to_string(), err.to_string());
        if member_failure_makes_data_unavailable(err) {
            self.unavailable_devices.insert(disk_id.to_string());
        }
        if let ArgosError::DiskFull {
            disk_id,
            required,
            available,
        } = err
        {
            self.disk_full
                .get_or_insert_with(|| (disk_id.clone(), *required, *available));
        }
    }
}

fn member_failure_makes_data_unavailable(err: &ArgosError) -> bool {
    err.is_fatal_device_error()
}

#[derive(Clone, Debug, Serialize)]
pub struct ScannedDevice {
    pub path: PathBuf,
    pub valid: bool,
    pub error: Option<String>,
    pub pool_uuid: Option<String>,
    pub device_uuid: Option<String>,
    pub disk_id: Option<String>,
    pub disk_index: Option<u32>,
    pub generation: Option<u64>,
    pub clean: Option<bool>,
    pub label: Option<String>,
    pub capacity: u64,
    pub superblock_source: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct RawJournalRecord {
    version: u32,
    time: f64,
    volume_uuid: String,
    txid: u64,
    generation: u64,
    action: String,
    details: serde_json::Value,
    meta_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    metadata: Option<Metadata>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    metadata_delta: Option<Vec<journal::MetadataDeltaOp>>,
    record_hash: String,
}

pub fn initialize_pool(
    backend: Arc<dyn StorageBackend>,
    superblocks: &[RawSuperblock],
    metadata: &mut Metadata,
    force: bool,
) -> Result<()> {
    preflight_empty(&*backend, superblocks, force)?;
    for sb in superblocks {
        let device_id = sb.disk_id.clone();
        let mut header = [0u8; 4096];
        header[..16].copy_from_slice(PROTECTIVE_MAGIC);
        backend.write_at(&device_id, PROTECTIVE_HEADER_OFFSET, &header)?;
        backend.write_at(&device_id, PRIMARY_SUPERBLOCK_OFFSET, &sb.encode())?;
        backend.write_at(&device_id, DEVICE_LABEL_OFFSET, &sb.device_label().encode())?;
        backend.write_at(&device_id, sb.backup_superblock_offset, &sb.encode())?;
        backend.write_at(
            &device_id,
            sb.backup_superblock_offset + SUPERBLOCK_SIZE as u64,
            &sb.device_label().encode(),
        )?;
        initialize_journal_region(&*backend, &device_id, sb)?;
    }
    write_metadata_copies(&*backend, superblocks, metadata)?;
    append_journal(
        &*backend,
        superblocks,
        metadata,
        None,
        "mkfs",
        serde_json::json!({}),
    )?;
    backend.flush_all()
}

pub fn preflight_devices_empty(
    backend: &dyn StorageBackend,
    superblocks: &[RawSuperblock],
    force: bool,
) -> Result<()> {
    preflight_empty(backend, superblocks, force)
}

pub fn open_pool(kind: BackendKind, paths: &[PathBuf], write: bool) -> Result<RawOpen> {
    let scanned = scan_paths(kind, paths);
    let mut devices = Vec::new();
    let mut superblocks = Vec::new();
    let mut seen_device_uuid = std::collections::BTreeSet::new();
    let mut seen_disk_id = std::collections::BTreeMap::<String, (u64, Uuid)>::new();
    let mut pool_uuid: Option<Uuid> = None;
    let mut scan_errors = Vec::new();

    for item in scanned {
        if !item.valid {
            scan_errors.push(format!(
                "{}: {}",
                item.path.display(),
                item.error
                    .clone()
                    .unwrap_or_else(|| "not an ArgosFS device".to_string())
            ));
            continue;
        }
        let (sb, _) = match superblock_from_path(kind, &item.path) {
            Ok(superblock) => superblock,
            Err(err) => {
                // A card that has gone bad between the scan and this read must
                // not make the whole pool unopenable.  Record it and continue;
                // quorum decides whether the remaining members are enough.
                scan_errors.push(format!("{}: {err}", item.path.display()));
                continue;
            }
        };
        if let Some(pool) = pool_uuid {
            if pool != sb.pool_uuid {
                return Err(ArgosError::IncompatibleFormat(
                    "devices belong to different ArgosFS pools".to_string(),
                ));
            }
        } else {
            pool_uuid = Some(sb.pool_uuid);
        }
        if !seen_device_uuid.insert(sb.device_uuid) {
            return Err(ArgosError::IncompatibleFormat(format!(
                "duplicate ArgosFS device UUID {}",
                sb.device_uuid
            )));
        }
        if let Some((generation, device_uuid)) =
            seen_disk_id.insert(sb.disk_id.clone(), (sb.generation, sb.device_uuid))
        {
            if generation != sb.generation {
                return Err(ArgosError::IncompatibleFormat(format!(
                    "generation conflict for disk id {}: {generation} vs {}",
                    sb.disk_id, sb.generation
                )));
            }
            return Err(ArgosError::IncompatibleFormat(format!(
                "duplicate ArgosFS disk id {} with device UUIDs {device_uuid} and {}",
                sb.disk_id, sb.device_uuid
            )));
        }
        devices.push((sb.disk_id.clone(), item.path));
        superblocks.push(sb);
    }
    if superblocks.is_empty() {
        return Err(ArgosError::MissingDevice(format!(
            "no readable ArgosFS devices found{}",
            if scan_errors.is_empty() {
                String::new()
            } else {
                format!(": {}", scan_errors.join("; "))
            }
        )));
    }

    let present_paths = devices
        .iter()
        .cloned()
        .collect::<std::collections::BTreeMap<_, _>>();
    let backend: Arc<dyn StorageBackend> = Arc::new(GuardedBackend::new(Arc::new(
        FileBlockBackend::open_with_ids(kind, devices, write)?,
    )));
    let (mut metadata, mut report) = load_or_recover(&*backend, &superblocks, write)?;
    if metadata.backend != kind {
        return Err(ArgosError::IncompatibleFormat(format!(
            "metadata backend {:?} does not match requested {:?}",
            metadata.backend, kind
        )));
    }
    // Recovery scans every same-pool device so it can discover the durable
    // state, but only members named by that selected state may participate in
    // subsequent writable fanout or be exposed to the mounted volume.
    let superblocks = metadata_member_superblocks(&superblocks, &metadata);
    let durable_metadata = (write
        && metadata.integrity.hash_scheme == Some(journal::DELTA_INTEGRITY_SCHEME)
        && metadata.integrity.meta_hash.len() == 64
        && superblocks
            .iter()
            .all(|sb| sb.required_feature_flags & REQUIRED_FEATURE_DELTA_INTEGRITY != 0))
    .then(|| metadata.clone());
    let present = superblocks
        .iter()
        .map(|sb| sb.disk_id.clone())
        .collect::<std::collections::BTreeSet<_>>();
    for (disk_id, disk) in &mut metadata.disks {
        if let Some(path) = present_paths.get(disk_id) {
            disk.path = path.clone();
            // Restore only scan-offline members: a caller can explicitly
            // isolate a raw member through ArgosFs::mark_disk().
            if disk.status == DiskStatus::Offline && !disk.manually_offline {
                disk.status = DiskStatus::Online;
            }
            if disk.status == DiskStatus::Online {
                disk.offline_since = None;
            }
        } else if !present.contains(disk_id)
            && !matches!(disk.status, DiskStatus::Removed | DiskStatus::Failed)
        {
            if disk.status != DiskStatus::Offline {
                disk.status = DiskStatus::Offline;
                disk.offline_since = Some(now_f64());
            } else if disk.offline_since.is_none() {
                // Metadata from an older build: start the grace period now
                // instead of treating the member as gone immediately.
                disk.offline_since = Some(now_f64());
            }
        }
    }
    if !scan_errors.is_empty() {
        report.errors.extend(scan_errors);
    }
    let mut recovery_verified = report.errors.is_empty() && report.invalid_entries == 0;
    if !clean_unmount_quorum(&superblocks, &metadata) {
        report
            .errors
            .push("pool was not cleanly unmounted".to_string());
    }
    if write {
        let dirty_report =
            write_superblock_clean_state_quorum(&*backend, &superblocks, &metadata, false)?;
        recovery_verified &= dirty_report.failed_devices.is_empty();
        for (disk_id, error) in dirty_report.failed_devices {
            report.errors.push(format!(
                "failed to mark raw member {disk_id} dirty on writable open: {error}"
            ));
        }
    }
    Ok(RawOpen {
        recovery_verified,
        durable_metadata,
        backend,
        metadata,
        report,
        superblocks,
    })
}

pub fn append_transaction(
    backend: &dyn StorageBackend,
    superblocks: &[RawSuperblock],
    metadata: &Metadata,
    action: &str,
    details: serde_json::Value,
) -> Result<()> {
    append_transaction_with_previous(backend, superblocks, metadata, None, action, details)
}

pub fn append_transaction_with_previous(
    backend: &dyn StorageBackend,
    superblocks: &[RawSuperblock],
    metadata: &Metadata,
    previous_metadata: Option<&Metadata>,
    action: &str,
    details: serde_json::Value,
) -> Result<()> {
    append_journal(
        backend,
        superblocks,
        metadata,
        previous_metadata,
        action,
        details,
    )?;
    journal::inject_crash(FaultPoint::AfterJournalCommitBeforeMetadataCommit.as_str())?;
    Ok(())
}

pub(crate) fn append_transaction_quorum(
    backend: &dyn StorageBackend,
    superblocks: &[RawSuperblock],
    metadata: &Metadata,
    previous_metadata: Option<&Metadata>,
    action: &str,
    details: serde_json::Value,
) -> Result<QuorumWriteReport> {
    let report = append_journal_quorum(
        backend,
        superblocks,
        metadata,
        previous_metadata,
        action,
        details,
        JournalQuorumOptions {
            mode: JournalQuorumMode::Transaction,
            metrics: None,
            prepared_delta: None,
        },
    )?;
    journal::inject_crash(FaultPoint::AfterJournalCommitBeforeMetadataCommit.as_str())?;
    Ok(report)
}

pub(crate) fn append_event_quorum(
    backend: &dyn StorageBackend,
    superblocks: &[RawSuperblock],
    metadata: &Metadata,
    action: &str,
    details: serde_json::Value,
) -> Result<QuorumWriteReport> {
    let report = append_journal_quorum(
        backend,
        superblocks,
        metadata,
        None,
        action,
        details,
        JournalQuorumOptions {
            mode: JournalQuorumMode::Event,
            metrics: None,
            prepared_delta: None,
        },
    )?;
    journal::inject_crash(FaultPoint::AfterJournalCommitBeforeMetadataCommit.as_str())?;
    Ok(report)
}

pub(crate) fn append_transaction_with_trusted_integrity_quorum(
    backend: &dyn StorageBackend,
    superblocks: &[RawSuperblock],
    metadata: &Metadata,
    previous_metadata: Option<&Metadata>,
    action: &str,
    details: serde_json::Value,
) -> Result<QuorumWriteReport> {
    let report = append_journal_quorum(
        backend,
        superblocks,
        metadata,
        previous_metadata,
        action,
        details,
        JournalQuorumOptions {
            mode: JournalQuorumMode::TrustedTransaction,
            metrics: None,
            prepared_delta: None,
        },
    )?;
    journal::inject_crash(FaultPoint::AfterJournalCommitBeforeMetadataCommit.as_str())?;
    Ok(report)
}

/// A trusted v2 group commit may carry the delta authenticated against this
/// exact durable base, avoiding a second metadata traversal at journal encode.
pub(crate) struct PreparedJournalBase<'a> {
    pub(crate) previous: &'a Metadata,
    pub(crate) delta: Option<Vec<journal::MetadataDeltaOp>>,
}

/// Timed group commit; the original entry point is retained for uninstrumented
/// repair, recovery and test callers.
pub(crate) fn append_transaction_with_trusted_integrity_quorum_measured(
    backend: &dyn StorageBackend,
    superblocks: &[RawSuperblock],
    metadata: &Metadata,
    prepared: PreparedJournalBase<'_>,
    metrics: &CommitMetrics,
    action: &str,
    details: serde_json::Value,
) -> Result<QuorumWriteReport> {
    let report = append_journal_quorum(
        backend,
        superblocks,
        metadata,
        Some(prepared.previous),
        action,
        details,
        JournalQuorumOptions {
            mode: JournalQuorumMode::TrustedTransaction,
            metrics: Some(metrics),
            prepared_delta: prepared.delta,
        },
    )?;
    journal::inject_crash(FaultPoint::AfterJournalCommitBeforeMetadataCommit.as_str())?;
    Ok(report)
}

/// Upgrade every active member's primary AND backup superblock before any
/// delta-chain record is emitted. This is an explicit offline maintenance
/// operation: a partially completed upgrade can be retried, and no v2 record
/// is written until *all* active members are confirmed upgraded.
pub(crate) fn upgrade_delta_integrity_members(
    backend: &dyn StorageBackend,
    superblocks: &[RawSuperblock],
    metadata: &Metadata,
) -> Result<Vec<RawSuperblock>> {
    let active = metadata
        .disks
        .iter()
        .filter(|(_, disk)| disk.status != DiskStatus::Removed)
        .count();
    if superblocks.len() != active {
        return Err(ArgosError::QuorumUnavailable {
            operation: "upgrade raw delta integrity: all active members must be present"
                .to_string(),
            need: active,
            have: superblocks.len(),
        });
    }
    let mut upgraded = Vec::with_capacity(superblocks.len());
    for sb in superblocks {
        if !metadata_has_voting_member(metadata, &sb.disk_id) {
            return Err(ArgosError::Invalid(format!(
                "raw integrity upgrade found unknown/removed member {}",
                sb.disk_id
            )));
        }
        let mut primary = [0u8; SUPERBLOCK_SIZE];
        let mut backup = [0u8; SUPERBLOCK_SIZE];
        backend.read_at(&sb.disk_id, PRIMARY_SUPERBLOCK_OFFSET, &mut primary)?;
        backend.read_at(&sb.disk_id, sb.backup_superblock_offset, &mut backup)?;
        let primary = RawSuperblock::decode(&primary)?;
        let backup = RawSuperblock::decode(&backup)?;
        for copy in [&primary, &backup] {
            if copy.pool_uuid != sb.pool_uuid
                || copy.device_uuid != sb.device_uuid
                || copy.disk_id != sb.disk_id
                || copy.generation != primary.generation
            {
                return Err(ArgosError::CorruptedMetadata(format!(
                    "raw integrity upgrade superblock copy mismatch on {}",
                    sb.disk_id
                )));
            }
        }
        let mut next = primary;
        next.required_feature_flags |=
            REQUIRED_FEATURE_DELTA_INTEGRITY | REQUIRED_FEATURE_ARRAY_SPLICE;
        for offset in [sb.backup_superblock_offset, PRIMARY_SUPERBLOCK_OFFSET] {
            backend.write_at(&sb.disk_id, offset, &next.encode())?;
            backend.flush_device(&sb.disk_id)?;
            let mut readback = [0u8; SUPERBLOCK_SIZE];
            backend.read_at(&sb.disk_id, offset, &mut readback)?;
            if RawSuperblock::decode(&readback)? != next {
                return Err(ArgosError::Checksum(format!(
                    "raw integrity feature did not persist on {} at {offset}",
                    sb.disk_id
                )));
            }
        }
        upgraded.push(next);
    }
    Ok(upgraded)
}

/// A journal delta may use the compact array-splice operation only when every
/// member in the write set advertises the required feature. Mixed or legacy
/// pools keep the older per-element array encoding.
pub(crate) fn array_splice_feature_enabled(superblocks: &[RawSuperblock]) -> bool {
    !superblocks.is_empty()
        && superblocks
            .iter()
            .all(|sb| sb.required_feature_flags & REQUIRED_FEATURE_ARRAY_SPLICE != 0)
}

pub fn recover_metadata(
    backend: &dyn StorageBackend,
    superblocks: &[RawSuperblock],
) -> Result<Metadata> {
    load_or_recover(backend, superblocks, false).map(|(metadata, _)| metadata)
}

pub fn audit(
    backend: &dyn StorageBackend,
    superblocks: &[RawSuperblock],
) -> Result<TransactionReport> {
    let (metadata, mut report) = load_or_recover(backend, superblocks, false)?;
    if !clean_unmount_quorum(superblocks, &metadata) {
        report
            .errors
            .push("pool was not cleanly unmounted".to_string());
    }
    Ok(report)
}

pub fn write_superblock_clean_state(
    backend: &dyn StorageBackend,
    superblocks: &[RawSuperblock],
    clean: bool,
) -> Result<()> {
    for sb in superblocks {
        write_superblock_clean_state_member(backend, sb, clean, None)?;
    }
    Ok(())
}

pub(crate) fn write_superblock_clean_state_quorum(
    backend: &dyn StorageBackend,
    superblocks: &[RawSuperblock],
    metadata: &Metadata,
    clean: bool,
) -> Result<QuorumWriteReport> {
    write_superblock_clean_state_quorum_impl(backend, superblocks, metadata, clean, None)
}

pub(crate) fn write_superblock_clean_state_quorum_measured(
    backend: &dyn StorageBackend,
    superblocks: &[RawSuperblock],
    metadata: &Metadata,
    clean: bool,
    metrics: &CommitMetrics,
) -> Result<QuorumWriteReport> {
    write_superblock_clean_state_quorum_impl(backend, superblocks, metadata, clean, Some(metrics))
}

fn write_superblock_clean_state_quorum_impl(
    backend: &dyn StorageBackend,
    superblocks: &[RawSuperblock],
    metadata: &Metadata,
    clean: bool,
    metrics: Option<&CommitMetrics>,
) -> Result<QuorumWriteReport> {
    let need = metadata_quorum_requirement(metadata);
    if superblocks.len() < need {
        return Err(quorum_unavailable(
            "superblock clean-state",
            need,
            superblocks.len(),
        ));
    }
    let mut report = QuorumWriteReport::default();
    let mut durable = 0usize;
    for sb in superblocks {
        match write_superblock_clean_state_member(backend, sb, clean, metrics) {
            Ok(()) => durable += 1,
            Err(err) => report.record_failure(&sb.disk_id, &err),
        }
    }
    if durable < need {
        return Err(quorum_failure_before_write(
            &report,
            "superblock clean-state",
            need,
            durable,
        ));
    }
    Ok(report)
}

fn write_superblock_clean_state_member(
    backend: &dyn StorageBackend,
    sb: &RawSuperblock,
    clean: bool,
    metrics: Option<&CommitMetrics>,
) -> Result<()> {
    let mut encoded_current = vec![0u8; SUPERBLOCK_SIZE];
    let mut copy = if backend
        .read_at(&sb.disk_id, PRIMARY_SUPERBLOCK_OFFSET, &mut encoded_current)
        .is_ok()
    {
        RawSuperblock::decode(&encoded_current)
            .ok()
            .filter(|current| {
                current.pool_uuid == sb.pool_uuid && current.device_uuid == sb.device_uuid
            })
            .unwrap_or_else(|| sb.clone())
    } else {
        sb.clone()
    };
    copy.clean = clean;
    copy.generation = copy.generation.saturating_add(1);
    let now = now_f64().max(0.0) as u64;
    if clean {
        copy.last_clean_unmount_time = now;
    } else {
        copy.last_mount_time = now;
    }
    let encoded = copy.encode();
    let label = copy.device_label().encode();
    backend.write_at(&copy.disk_id, PRIMARY_SUPERBLOCK_OFFSET, &encoded)?;
    if let Some(metrics) = metrics {
        metrics.add_raw_write_bytes(RawWriteKind::CleanState, encoded.len());
    }
    backend.write_at(&copy.disk_id, DEVICE_LABEL_OFFSET, &label)?;
    if let Some(metrics) = metrics {
        metrics.add_raw_write_bytes(RawWriteKind::CleanState, label.len());
    }
    backend.write_at(&copy.disk_id, copy.backup_superblock_offset, &encoded)?;
    if let Some(metrics) = metrics {
        metrics.add_raw_write_bytes(RawWriteKind::CleanState, encoded.len());
    }
    backend.flush_device(&copy.disk_id)
}

type MemberCheckpointCandidate = (String, Option<Arc<Metadata>>, MetadataCandidateReport);

/// Bound concurrent checkpoint payload buffers by the actual stored lengths,
/// not by the reserved slot capacity. Oversized payloads run alone.
fn read_all_checkpoint_candidates(
    backend: &dyn StorageBackend,
    superblocks: &[RawSuperblock],
) -> Result<Vec<MemberCheckpointCandidate>> {
    const MAX_WORKERS: usize = 4;
    const PAYLOAD_BUDGET: u64 = 256 * 1024 * 1024;
    let decoded = parking_lot::Mutex::new(BTreeMap::new());
    let mut candidates = Vec::new();
    let mut plans = superblocks
        .iter()
        .map(|sb| {
            let headers = read_checkpoint_headers(backend, sb);
            let bytes = checkpoint_payload_budget(sb, &headers);
            (sb, headers, bytes)
        })
        .peekable();
    while let Some(first) = plans.next() {
        let mut bytes = first.2;
        let mut batch = vec![first];
        while batch.len() < MAX_WORKERS
            && plans
                .peek()
                .is_some_and(|next| bytes.saturating_add(next.2) <= PAYLOAD_BUDGET)
        {
            let next = plans.next().expect("peeked checkpoint");
            bytes += next.2;
            batch.push(next);
        }
        let results = std::thread::scope(|scope| {
            let jobs = batch
                .into_iter()
                .map(|(sb, headers, _)| {
                    let decoded = &decoded;
                    scope.spawn(move || {
                        (
                            sb,
                            read_metadata_candidates_from_headers(backend, sb, decoded, headers),
                        )
                    })
                })
                .collect::<Vec<_>>();
            jobs.into_iter()
                .map(|job| {
                    job.join()
                        .map_err(|_| ArgosError::Invalid("checkpoint read worker panicked".into()))
                })
                .collect::<Result<Vec<_>>>()
        })?;
        for (sb, result) in results {
            candidates.extend(result?.into_iter().map(|(m, r)| (sb.disk_id.clone(), m, r)));
        }
    }
    Ok(candidates)
}

fn load_or_recover(
    backend: &dyn StorageBackend,
    superblocks: &[RawSuperblock],
    checkpoint_replay: bool,
) -> Result<(Metadata, TransactionReport)> {
    let candidates = read_all_checkpoint_candidates(backend, superblocks)?;
    let mut report = TransactionReport::default();
    report.metadata_candidates = candidates
        .iter()
        .map(|(_, _, report)| report.clone())
        .collect();
    let mut best = select_quorum_metadata_candidate(&candidates)?;
    let journal_best = read_latest_journal_metadata(
        backend,
        superblocks,
        &mut report,
        best.as_deref(),
        &candidates,
    )?;
    if let Some(journal_meta) = journal_best {
        if best
            .as_ref()
            .map(|metadata| journal_meta.txid > metadata.txid)
            .unwrap_or(true)
        {
            report.replayed = true;
            best = Some(journal_meta);
        }
    }
    let Some(metadata) = best else {
        return Err(ArgosError::CorruptedMetadata(
            "no valid raw metadata checkpoint or journal record found".to_string(),
        ));
    };
    // The selected snapshot is the only state needed from this point. Drop
    // physical slot votes before any recovery checkpoint serialization.
    drop(candidates);
    let metadata = Arc::unwrap_or_clone(metadata);
    if !matches!(
        metadata.integrity.hash_scheme,
        None | Some(journal::DELTA_INTEGRITY_SCHEME)
    ) {
        return Err(ArgosError::IncompatibleFormat(
            "unknown raw metadata integrity hash scheme".to_string(),
        ));
    }
    // An integrity-v2 state can only be selected when a quorum of physical
    // members has the required downgrade-prevention bit. Otherwise refuse to
    // mount: older binaries would still accept the remaining v1 members.
    if metadata.integrity.hash_scheme == Some(journal::DELTA_INTEGRITY_SCHEME) {
        let upgraded = superblocks
            .iter()
            .filter(|sb| {
                sb.required_feature_flags & REQUIRED_FEATURE_DELTA_INTEGRITY != 0
                    && metadata_has_voting_member(&metadata, &sb.disk_id)
            })
            .count();
        let need = metadata_quorum_requirement(&metadata);
        if upgraded < need {
            return Err(ArgosError::IncompatibleFormat(format!(
                "raw delta integrity requires {need} upgraded members, found {upgraded}"
            )));
        }
    }
    report.selected_metadata_source = "raw-metadata-or-journal".to_string();
    if checkpoint_replay && report.replayed {
        journal::inject_crash(FaultPoint::DuringReplay.as_str())?;
        let member_superblocks = metadata_member_superblocks(superblocks, &metadata);
        if can_retain_replayed_journal(&metadata, &member_superblocks, &report) {
            return Ok((metadata, report));
        }
        let checkpoint_report =
            write_metadata_copies_quorum(backend, &member_superblocks, &metadata)?;
        let failed_checkpoint_members = checkpoint_report
            .failed_devices
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>();

        // Once the replayed checkpoint itself has a durable quorum, journal
        // compaction is cleanup rather than a mount precondition. Reset only
        // members that definitely received the checkpoint; a stale journal on
        // another member is harmless because replay ignores records at/below the
        // selected checkpoint txid.
        for sb in &member_superblocks {
            if failed_checkpoint_members.contains(&sb.disk_id) {
                continue;
            }
            if let Err(err) = initialize_journal_region(backend, &sb.disk_id, sb)
                .and_then(|()| backend.flush_device(&sb.disk_id))
            {
                report.errors.push(format!(
                    "raw journal reset after replay failed on {}: {err}",
                    sb.disk_id
                ));
            }
        }
    }
    Ok((metadata, report))
}

/// Keeping the existing replay base is safe only when every active member
/// has the same validated journal tip at the selected transaction. Divergent,
/// missing, legacy and damaged members still take the conservative reanchor.
fn can_retain_replayed_journal(
    metadata: &Metadata,
    members: &[RawSuperblock],
    report: &TransactionReport,
) -> bool {
    if metadata.integrity.hash_scheme != Some(journal::DELTA_INTEGRITY_SCHEME)
        || !report.errors.is_empty()
        || report.invalid_entries != 0
        || members.len()
            != metadata
                .disks
                .values()
                .filter(|d| d.status != DiskStatus::Removed)
                .count()
        || !metadata
            .disks
            .values()
            .all(|d| matches!(d.status, DiskStatus::Online | DiskStatus::Removed))
        || members
            .iter()
            .any(|sb| sb.required_feature_flags & REQUIRED_FEATURE_DELTA_INTEGRITY == 0)
    {
        return false;
    }
    let mut tip = None;
    for sb in members {
        let Some(member) = report
            .raw_journal_members
            .iter()
            .find(|m| m.disk_id == sb.disk_id)
        else {
            return false;
        };
        if !member.readable
            || member.error.is_some()
            || member.invalid_entries != 0
            || member.last_valid_txid != metadata.txid
            || member.last_valid_generation != metadata.integrity.generation
            || member.last_valid_record_hash.is_empty()
            || tip.is_some_and(|hash: &str| hash != member.last_valid_record_hash)
        {
            return false;
        }
        tip = Some(member.last_valid_record_hash.as_str());
    }
    tip.is_some()
}

fn metadata_has_voting_member(metadata: &Metadata, disk_id: &str) -> bool {
    metadata
        .disks
        .get(disk_id)
        .is_some_and(|disk| disk.status != DiskStatus::Removed)
}

fn metadata_member_superblocks(
    superblocks: &[RawSuperblock],
    metadata: &Metadata,
) -> Vec<RawSuperblock> {
    superblocks
        .iter()
        .filter(|sb| metadata_has_voting_member(metadata, &sb.disk_id))
        .cloned()
        .collect()
}

fn select_quorum_metadata_candidate(
    candidates: &[(String, Option<Arc<Metadata>>, MetadataCandidateReport)],
) -> Result<Option<Arc<Metadata>>> {
    let mut supported =
        BTreeMap::<(u64, u64, String), (Arc<Metadata>, std::collections::BTreeSet<String>)>::new();
    for (disk_id, metadata, _) in candidates {
        let Some(metadata) = metadata else {
            continue;
        };
        if !metadata_has_voting_member(metadata, disk_id) {
            continue;
        }
        let hash = metadata_hash_for_replay(metadata)?;
        supported
            .entry((metadata.txid, metadata.integrity.generation, hash))
            .or_insert_with(|| (metadata.clone(), std::collections::BTreeSet::new()))
            .1
            .insert(disk_id.clone());
    }
    Ok(supported
        .into_iter()
        .filter(|(_, (metadata, members))| members.len() >= metadata_quorum_requirement(metadata))
        .max_by_key(|((txid, generation, _), _)| (*txid, *generation))
        .map(|(_, (metadata, _))| metadata))
}

pub(crate) fn metadata_quorum_requirement(metadata: &Metadata) -> usize {
    metadata
        .disks
        .values()
        .filter(|disk| disk.status != DiskStatus::Removed)
        .count()
        .max(1)
        / 2
        + 1
}

fn clean_unmount_quorum(superblocks: &[RawSuperblock], metadata: &Metadata) -> bool {
    // A clean unmount marks the members clean only after a metadata
    // checkpoint has reached its quorum.  If a card failed during the
    // unmount, or missed a later cleanup, its superblock stays dirty while
    // every other member is clean.  Requiring every member to be clean makes
    // that one stale card block every writable mount, even though the clean
    // members are a full quorum.  The remaining clean members are enough.
    let clean = superblocks.iter().filter(|sb| sb.clean).count();
    clean >= metadata_quorum_requirement(metadata)
}

fn quorum_unavailable(operation: &str, need: usize, have: usize) -> ArgosError {
    ArgosError::QuorumUnavailable {
        operation: operation.to_string(),
        need,
        have,
    }
}

fn retryable_quorum_unavailable(operation: &str, need: usize, have: usize) -> ArgosError {
    ArgosError::RetryableQuorumUnavailable {
        operation: operation.to_string(),
        need,
        have,
    }
}

fn quorum_failure_before_write(
    report: &QuorumWriteReport,
    operation: &str,
    need: usize,
    have: usize,
) -> ArgosError {
    if !report.unavailable_devices.is_empty() {
        return quorum_unavailable(operation, need, have);
    }
    // A checkpoint that does not fit is not a transient quorum failure.
    // Reporting EAGAIN here makes callers retry forever (for example, rsync
    // retrying setattr) instead of reporting the real out-of-space condition.
    if let Some((disk_id, required, available)) = &report.disk_full {
        return ArgosError::DiskFull {
            disk_id: disk_id.clone(),
            required: *required,
            available: *available,
        };
    }
    retryable_quorum_unavailable(operation, need, have)
}

fn indeterminate_commit(operation: &str, need: usize, durable: usize) -> ArgosError {
    ArgosError::IndeterminateCommit(format!(
        "{operation}: need {need} durable members, confirmed {durable}; additional members may have exposed the new state before their write/flush error"
    ))
}

fn raw_record_hash(record: &RawJournalRecord) -> Result<String> {
    Ok(sha256_hex(&serde_json::to_vec(&serde_json::json!({
        "version": record.version,
        "volume_uuid": record.volume_uuid,
        "txid": record.txid,
        "generation": record.generation,
        "action": record.action,
        "meta_hash": record.meta_hash,
    }))?))
}

fn get_u64(bytes: &[u8], offset: usize) -> Result<u64> {
    wire::read_u64(bytes, offset, "short raw-store u64")
}

fn get_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    wire::read_u32(bytes, offset, "short raw-store u32")
}

fn checked_usize(value: u64, name: &str) -> Result<usize> {
    usize::try_from(value)
        .map_err(|_| ArgosError::CorruptedMetadata(format!("{name} does not fit usize")))
}

fn put_fixed_str(out: &mut [u8], offset: usize, len: usize, value: &str) {
    wire::put_string(out, offset, len, value)
}

fn get_fixed_hex(bytes: &[u8], offset: usize, len: usize) -> Result<String> {
    wire::read_string(bytes, offset, len, "short raw-store string")
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;
