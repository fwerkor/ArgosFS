//! Read-only FUSE/Borg workloads must never generate full metadata checkpoints.
//! Raw volume read counters and latency EWMAs are advisory: they remain live in
//! memory and are picked up by the next *real* durable transaction only.

use super::*;
use std::path::PathBuf;

fn raw_fixture() -> (tempfile::TempDir, Vec<PathBuf>, ArgosFs, InodeId) {
    let dir = tempfile::tempdir().unwrap();
    let images = (0..3)
        .map(|i| dir.path().join(format!("advisory-read-{i}.img")))
        .collect::<Vec<_>>();
    let fs = ArgosFs::create_loop(
        &images,
        VolumeConfig {
            k: 2,
            m: 1,
            compression: Compression::None,
            chunk_size: 4096,
            defer_journal_flush: true,
            defer_metadata_commit: true,
            defer_data_flush: true,
            deferred_commit_interval_ms: 10,
            deferred_commit_max_transactions: 128,
            ..VolumeConfig::default()
        },
        32 * 1024 * 1024,
        "advisory-read-only",
        false,
    )
    .unwrap();
    let ino = fs.create_file_path("/value", 0o600).unwrap();
    fs.write_inode_range(
        ino,
        0,
        b"read-only telemetry must not trigger a full checkpoint",
    )
    .unwrap();
    fs.sync().unwrap();
    assert!(!fs.deferred_commit.lock().raw_checkpoint_required);
    assert!(!fs.deferred_commit.lock().raw_uncommitted_metadata_dirty);
    (dir, images, fs, ino)
}

fn phase(fs: &ArgosFs, phase: CommitPhase) -> u64 {
    fs.commit_metrics()
        .snapshots()
        .find(|(name, _)| *name == phase.name())
        .unwrap()
        .1
        .count
}

#[test]
fn repeated_raw_reads_do_not_write_any_checkpoint_or_journal_even_after_sync() {
    let (_dir, images, fs, ino) = raw_fixture();
    let initial = fs.metadata_snapshot();
    let start_checkpoint = phase(&fs, CommitPhase::Checkpoint);
    let start_journal = phase(&fs, CommitPhase::Journal);
    let start_group = phase(&fs, CommitPhase::GroupCommit);
    let expected = b"read-only telemetry must not trigger a full checkpoint";
    for _ in 0..80 {
        assert_eq!(
            fs.read_inode(ino, 0, expected.len(), true).unwrap(),
            expected
        );
        assert!(!fs.sync_deferred_if_dirty().unwrap());
        fs.sync().unwrap();
    }
    let live = fs.metadata_snapshot();
    assert_eq!(live.txid, initial.txid);
    assert_eq!(live.integrity.meta_hash, initial.integrity.meta_hash);
    assert_eq!(
        live.inodes[&ino].read_bytes,
        initial.inodes[&ino].read_bytes + (80 * expected.len()) as u64
    );
    assert_eq!(
        live.inodes[&ino].access_count,
        initial.inodes[&ino].access_count + 80
    );
    assert_eq!(phase(&fs, CommitPhase::Checkpoint), start_checkpoint);
    assert_eq!(phase(&fs, CommitPhase::Journal), start_journal);
    assert_eq!(phase(&fs, CommitPhase::GroupCommit), start_group);
    {
        let state = fs.deferred_commit.lock();
        assert!(state.raw_advisory_metadata_dirty);
        assert!(!state.raw_uncommitted_metadata_dirty);
        assert!(!state.raw_checkpoint_required);
        assert_eq!(state.dirty_transactions, 0);
    }
    // Advisory counters may roll back on restart. The durable hash/bytes
    // must nevertheless verify; no metadata checkpoint was written for reads.
    drop(fs);
    let recovered = ArgosFs::open_loop(&images, false).unwrap();
    assert_eq!(recovered.metadata_snapshot().txid, initial.txid);
    assert_eq!(
        recovered.read_inode(ino, 0, expected.len(), false).unwrap(),
        expected
    );
    assert_eq!(recovered.transaction_report().unwrap().invalid_entries, 0);
}

#[test]
fn a_real_write_after_many_reads_commits_delta_and_recovers_without_hash_divergence() {
    let (_dir, images, fs, ino) = raw_fixture();
    let initial = fs.metadata_snapshot();
    let before_checkpoint = phase(&fs, CommitPhase::Checkpoint);
    let before_journal = phase(&fs, CommitPhase::Journal);
    for _ in 0..40 {
        assert_eq!(fs.read_inode(ino, 0, 9, false).unwrap(), b"read-only");
    }
    assert!(!fs.sync_deferred_if_dirty().unwrap());
    assert!(fs.runtime.read_advisory.pending_entries().0 > 0);
    fs.write_file("/other", b"new actual durable data", 0o600)
        .unwrap();
    assert!(fs.sync_deferred_if_dirty().unwrap());
    assert!(!fs.deferred_commit.lock().raw_advisory_metadata_dirty);
    assert_eq!(phase(&fs, CommitPhase::Checkpoint), before_checkpoint);
    assert!(phase(&fs, CommitPhase::Journal) > before_journal);
    // write_file creates an inode and then stores its data: both are real
    // namespace mutations, so the transaction id advances twice.
    assert_eq!(fs.metadata_snapshot().txid, initial.txid + 2);
    assert_eq!(fs.transaction_report().unwrap().invalid_entries, 0);
    drop(fs);
    let recovered = ArgosFs::open_loop(&images, false).unwrap();
    assert_eq!(
        recovered.read_file("/other", false).unwrap(),
        b"new actual durable data"
    );
    assert_eq!(
        recovered.read_inode(ino, 0, 9, false).unwrap(),
        b"read-only"
    );
    assert_eq!(recovered.transaction_report().unwrap().invalid_entries, 0);
}

#[test]
fn device_read_latency_updates_are_advisory_but_explicit_checkpoint_recovery_is_not() {
    let (_dir, _images, fs, _ino) = raw_fixture();
    let before = phase(&fs, CommitPhase::Checkpoint);
    {
        let mut meta = fs.meta.write();
        let disk = meta.disks.keys().next().unwrap().clone();
        fs.update_read_latency_locked(&mut meta, &disk, 4096, 0.01);
    }
    assert!(fs.deferred_commit.lock().raw_advisory_metadata_dirty);
    assert!(!fs.sync_deferred_if_dirty().unwrap());
    assert_eq!(phase(&fs, CommitPhase::Checkpoint), before);

    // A genuine recovery-required checkpoint must not be suppressed just
    // because advisory telemetry was also modified since the last commit.
    {
        let mut state = fs.deferred_commit.lock();
        state.raw_uncommitted_metadata_dirty = true;
        state.raw_checkpoint_required = true;
    }
    assert!(fs.sync_deferred_if_dirty().unwrap());
    assert_eq!(phase(&fs, CommitPhase::Checkpoint), before + 1);
    let state = fs.deferred_commit.lock();
    assert!(!state.raw_uncommitted_metadata_dirty);
    assert!(!state.raw_advisory_metadata_dirty);
    assert!(!state.raw_checkpoint_required);
}

#[test]
fn clean_unmount_after_advisory_only_reads_never_persists_a_stale_integrity_hash() {
    let (_dir, images, fs, ino) = raw_fixture();
    for _ in 0..25 {
        assert_eq!(fs.read_inode(ino, 0, 4, false).unwrap(), b"read");
    }
    assert!(fs.runtime.read_advisory.pending_entries().0 > 0);
    let durable = fs.deferred_commit.lock().durable_metadata.clone().unwrap();
    let before = phase(&fs, CommitPhase::Checkpoint);
    let before_journal = phase(&fs, CommitPhase::Journal);
    fs.mark_clean_unmount().unwrap();
    let live_hash = fs.metadata_snapshot();
    // In-memory read telemetry does not match the trusted durable hash;
    // persisting it without a real transaction would corrupt recovery.
    assert_ne!(
        journal::canonical_metadata_hash(&live_hash).unwrap(),
        live_hash.integrity.meta_hash
    );
    assert_eq!(phase(&fs, CommitPhase::Checkpoint), before);
    assert_eq!(phase(&fs, CommitPhase::Journal), before_journal);
    let state = fs.deferred_commit.lock();
    assert!(state.raw_advisory_metadata_dirty);
    assert!(!state.raw_uncommitted_metadata_dirty);
    drop(state);
    drop(fs);
    let recovered = ArgosFs::open_loop(&images, false).unwrap();
    let recovered_meta = recovered.metadata_snapshot();
    assert_eq!(
        recovered.read_file("/value", false).unwrap(),
        b"read-only telemetry must not trigger a full checkpoint"
    );
    assert_eq!(recovered.transaction_report().unwrap().invalid_entries, 0);
    assert_eq!(
        recovered_meta.integrity.meta_hash,
        durable.integrity.meta_hash
    );
    assert_eq!(recovered_meta.txid, durable.txid);
    assert_eq!(
        recovered_meta.inodes[&ino].read_bytes,
        durable.inodes[&ino].read_bytes
    );
}

#[test]
fn immediate_metadata_commit_after_advisory_reads_reanchors_trusted_hash() {
    let dir = tempfile::tempdir().unwrap();
    let images = (0..3)
        .map(|i| dir.path().join(format!("immediate-{i}.img")))
        .collect::<Vec<_>>();
    let fs = ArgosFs::create_loop(
        &images,
        VolumeConfig {
            k: 2,
            m: 1,
            compression: Compression::None,
            chunk_size: 4096,
            defer_metadata_commit: false,
            ..VolumeConfig::default()
        },
        32 * 1024 * 1024,
        "immediate-advisory",
        false,
    )
    .unwrap();
    fs.write_file("/old", b"the old file", 0o600).unwrap();
    for _ in 0..25 {
        assert_eq!(fs.read_file("/old", false).unwrap(), b"the old file");
    }
    assert!(fs.runtime.read_advisory.pending_entries().0 > 0);
    assert!(!fs.deferred_commit.lock().raw_uncommitted_metadata_dirty);
    fs.write_file("/new", b"the new file", 0o600).unwrap();
    assert!(!fs.deferred_commit.lock().raw_advisory_metadata_dirty);
    fs.sync().unwrap();
    drop(fs);
    let recovered = ArgosFs::open_loop(&images, false).unwrap();
    assert_eq!(recovered.read_file("/old", false).unwrap(), b"the old file");
    assert_eq!(recovered.read_file("/new", false).unwrap(), b"the new file");
    assert_eq!(recovered.transaction_report().unwrap().invalid_entries, 0);
}

/// Observe real successful device writes, classified by raw on-disk region.
/// This catches unmeasured checkpoints in clean-unmount as well as commits.
struct WriteAuditBackend {
    inner: Arc<dyn StorageBackend>,
    regions: BTreeMap<String, RawSuperblock>,
    bytes: Mutex<BTreeMap<String, [u64; 3]>>,
}

impl WriteAuditBackend {
    fn new(inner: Arc<dyn StorageBackend>, superblocks: &[RawSuperblock]) -> Self {
        Self {
            inner,
            regions: superblocks
                .iter()
                .map(|sb| (sb.disk_id.clone(), sb.clone()))
                .collect(),
            bytes: Mutex::new(BTreeMap::new()),
        }
    }

    fn totals(&self) -> [u64; 3] {
        self.bytes
            .lock()
            .values()
            .fold([0u64; 3], |mut sum, bytes| {
                for (total, value) in sum.iter_mut().zip(bytes) {
                    *total += *value;
                }
                sum
            })
    }
}

impl StorageBackend for WriteAuditBackend {
    fn backend_kind(&self) -> BackendKind {
        self.inner.backend_kind()
    }

    fn list_devices(&self) -> Result<Vec<crate::storage::backend::BackendDeviceInfo>> {
        self.inner.list_devices()
    }

    fn read_at(&self, id: &String, offset: u64, data: &mut [u8]) -> Result<()> {
        self.inner.read_at(id, offset, data)
    }

    fn write_at(&self, id: &String, offset: u64, data: &[u8]) -> Result<()> {
        self.inner.write_at(id, offset, data)?;
        let sb = &self.regions[id];
        let in_region = |start: u64, len: u64| offset >= start && offset < start + len;
        // [metadata slots, journal records, superblock/label/other]
        let kind = if in_region(sb.metadata.offset, sb.metadata.length) {
            0
        } else if in_region(sb.journal.offset, sb.journal.length) {
            1
        } else {
            2
        };
        self.bytes.lock().entry(id.clone()).or_default()[kind] += data.len() as u64;
        Ok(())
    }

    fn flush_device(&self, id: &String) -> Result<()> {
        self.inner.flush_device(id)
    }

    fn flush_all(&self) -> Result<()> {
        self.inner.flush_all()
    }

    fn drop_cache(&self, id: &String, offset: u64, length: u64) -> Result<()> {
        self.inner.drop_cache(id, offset, length)
    }

    fn capacity(&self, id: &String) -> Result<u64> {
        self.inner.capacity(id)
    }

    fn device_status(&self, id: &String) -> Result<DiskStatus> {
        self.inner.device_status(id)
    }

    fn capabilities(&self) -> crate::storage::backend::BackendCapabilities {
        self.inner.capabilities()
    }
}

#[test]
fn nine_member_clean_read_cycle_only_writes_small_superblock_flags() {
    let dir = tempfile::tempdir().unwrap();
    let images = (0..9)
        .map(|i| dir.path().join(format!("nine-member-{i}.img")))
        .collect::<Vec<_>>();
    let mut fs = ArgosFs::create_loop(
        &images,
        VolumeConfig {
            k: 4,
            m: 4,
            chunk_size: 4096,
            compression: Compression::None,
            defer_metadata_commit: true,
            ..VolumeConfig::default()
        },
        32 * 1024 * 1024,
        "nine-member-read",
        false,
    )
    .unwrap();
    fs.upgrade_delta_integrity().unwrap();
    let ino = fs.create_file_path("/archive", 0o600).unwrap();
    fs.write_inode_range(ino, 0, b"borg verify read only")
        .unwrap();
    fs.sync().unwrap();
    let baseline = fs.metadata_snapshot();
    let bytes_before = fs
        .commit_metrics()
        .raw_write_byte_snapshots()
        .collect::<Vec<_>>();
    let reasons_before = fs
        .commit_metrics()
        .checkpoint_reason_snapshots()
        .collect::<Vec<_>>();
    let audit = Arc::new(WriteAuditBackend::new(
        fs.backend.clone(),
        &fs.raw_superblocks,
    ));
    fs.backend = audit.clone();
    for _ in 0..48 {
        assert_eq!(
            fs.read_inode(ino, 0, 21, true).unwrap(),
            b"borg verify read only"
        );
        assert!(!fs.sync_deferred_if_dirty().unwrap());
        fs.sync().unwrap();
    }
    fs.mark_clean_unmount().unwrap();
    let [metadata_bytes, journal_bytes, superblock_bytes] = audit.totals();
    assert_eq!(
        metadata_bytes, 0,
        "advisory-only reads wrote metadata slots"
    );
    assert_eq!(
        journal_bytes, 0,
        "advisory-only reads wrote journal records"
    );
    assert_eq!(
        superblock_bytes,
        9 * 3 * 4096,
        "only clean-state headers should change"
    );
    let bytes_after = fs
        .commit_metrics()
        .raw_write_byte_snapshots()
        .collect::<Vec<_>>();
    assert_eq!(bytes_after[0].1, bytes_before[0].1);
    assert_eq!(bytes_after[1].1, bytes_before[1].1);
    assert_eq!(bytes_after[2].1 - bytes_before[2].1, superblock_bytes);
    assert_eq!(
        fs.commit_metrics()
            .checkpoint_reason_snapshots()
            .collect::<Vec<_>>(),
        reasons_before
    );
    assert_eq!(fs.metadata_snapshot().txid, baseline.txid);
    assert_eq!(
        fs.metadata_snapshot().integrity.meta_hash,
        baseline.integrity.meta_hash
    );
    assert_eq!(phase(&fs, CommitPhase::Checkpoint), 0);
    assert_eq!(phase(&fs, CommitPhase::Journal), 1);
    drop(fs);
    let recovered = ArgosFs::open_loop(&images, false).unwrap();
    assert_eq!(
        recovered.read_file("/archive", false).unwrap(),
        b"borg verify read only"
    );
    assert_eq!(recovered.transaction_report().unwrap().invalid_entries, 0);
}

#[test]
fn immediate_raw_reads_and_clean_unmount_do_not_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let images = (0..3)
        .map(|i| dir.path().join(format!("immediate-read-{i}.img")))
        .collect::<Vec<_>>();
    let mut fs = ArgosFs::create_loop(
        &images,
        VolumeConfig {
            k: 2,
            m: 1,
            chunk_size: 4096,
            defer_metadata_commit: false,
            ..VolumeConfig::default()
        },
        32 * 1024 * 1024,
        "immediate-read",
        false,
    )
    .unwrap();
    fs.write_file("/archive", b"stable", 0o600).unwrap();
    let baseline = fs.metadata_snapshot();
    let audit = Arc::new(WriteAuditBackend::new(
        fs.backend.clone(),
        &fs.raw_superblocks,
    ));
    fs.backend = audit.clone();
    for _ in 0..20 {
        assert_eq!(fs.read_file("/archive", true).unwrap(), b"stable");
        fs.sync().unwrap();
    }
    fs.mark_clean_unmount().unwrap();
    assert_eq!(audit.totals(), [0, 0, 3 * 3 * 4096]);
    assert_eq!(fs.metadata_snapshot().txid, baseline.txid);
    assert_eq!(
        fs.metadata_snapshot().integrity.meta_hash,
        baseline.integrity.meta_hash
    );
    drop(fs);
    let reopened = ArgosFs::open_loop(&images, false).unwrap();
    assert_eq!(reopened.read_file("/archive", false).unwrap(), b"stable");
    assert_eq!(reopened.transaction_report().unwrap().invalid_entries, 0);
}

#[test]
fn clean_reopened_v2_pool_defers_reanchor_until_real_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let images = (0..3)
        .map(|i| dir.path().join(format!("v2-reopen-{i}.img")))
        .collect::<Vec<_>>();
    let mut fs = ArgosFs::create_loop(
        &images,
        VolumeConfig {
            k: 2,
            m: 1,
            defer_metadata_commit: true,
            ..VolumeConfig::default()
        },
        32 * 1024 * 1024,
        "clean-v2-reopen",
        false,
    )
    .unwrap();
    fs.upgrade_delta_integrity().unwrap();
    fs.write_file("/before", b"durable before reopen", 0o600)
        .unwrap();
    fs.sync().unwrap();
    let baseline = fs.metadata_snapshot();
    assert_eq!(
        baseline.integrity.hash_scheme,
        Some(journal::DELTA_INTEGRITY_SCHEME)
    );
    fs.mark_clean_unmount().unwrap();
    drop(fs);
    let mut fs = ArgosFs::open_loop(&images, true).unwrap();
    assert!(fs.deferred_commit.lock().raw_mount_reanchor_pending);
    let audit = Arc::new(WriteAuditBackend::new(
        fs.backend.clone(),
        &fs.raw_superblocks,
    ));
    fs.backend = audit.clone();
    for _ in 0..20 {
        assert_eq!(
            fs.read_file("/before", false).unwrap(),
            b"durable before reopen"
        );
        assert!(!fs.sync_deferred_if_dirty().unwrap());
        fs.sync().unwrap();
    }
    fs.mark_clean_unmount().unwrap();
    assert_eq!(audit.totals(), [0, 0, 3 * 3 * 4096]);
    drop(fs);
    let fs = ArgosFs::open_loop(&images, true).unwrap();
    assert_eq!(
        fs.metadata_snapshot().integrity.meta_hash,
        baseline.integrity.meta_hash
    );
    assert!(fs.deferred_commit.lock().raw_mount_reanchor_pending);
    fs.write_file("/after", b"durable after reopen", 0o600)
        .unwrap();
    fs.sync().unwrap();
    assert_eq!(phase(&fs, CommitPhase::Checkpoint), 1);
    assert!(!fs.deferred_commit.lock().raw_mount_reanchor_pending);
    drop(fs);
    let fs = ArgosFs::open_loop(&images, false).unwrap();
    assert_eq!(
        fs.read_file("/before", false).unwrap(),
        b"durable before reopen"
    );
    assert_eq!(
        fs.read_file("/after", false).unwrap(),
        b"durable after reopen"
    );
    assert_eq!(fs.transaction_report().unwrap().invalid_entries, 0);
}
