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
    assert!(fs.read_advisory.pending_entries().0 > 0);
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
    assert!(fs.read_advisory.pending_entries().0 > 0);
    let before = phase(&fs, CommitPhase::Checkpoint);
    fs.mark_clean_unmount().unwrap();
    let live_hash = fs.metadata_snapshot();
    // Check the actual checkpoint image *before* open normalizes disk usage
    // and other runtime metadata fields; these are intentionally allowed to
    // differ from the stored canonical hash in a recovered live metadata view.
    assert_eq!(
        journal::canonical_metadata_hash(&live_hash).unwrap(),
        live_hash.integrity.meta_hash
    );
    assert_eq!(phase(&fs, CommitPhase::Checkpoint), before + 1);
    let state = fs.deferred_commit.lock();
    assert!(!state.raw_advisory_metadata_dirty);
    assert!(!state.raw_uncommitted_metadata_dirty);
    drop(state);
    drop(fs);
    let recovered = ArgosFs::open_loop(&images, false).unwrap();
    assert_eq!(
        recovered.read_file("/value", false).unwrap(),
        b"read-only telemetry must not trigger a full checkpoint"
    );
    assert_eq!(recovered.transaction_report().unwrap().invalid_entries, 0);
    let recovered_meta = recovered.metadata_snapshot();
    assert_eq!(
        recovered_meta.integrity.meta_hash,
        live_hash.integrity.meta_hash
    );
    assert_eq!(recovered_meta.txid, live_hash.txid);
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
    assert!(fs.read_advisory.pending_entries().0 > 0);
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
