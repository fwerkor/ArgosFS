use super::*;
use crate::types::{Compression, DiskStatus, VolumeConfig};
use std::ffi::OsStr;
use std::sync::Arc;

fn fixture(k: usize, m: usize, chunk_size: usize) -> (tempfile::TempDir, ArgosFs, InodeId) {
    let dir = tempfile::tempdir().unwrap();
    let volume = ArgosFs::create(
        dir.path(),
        VolumeConfig {
            k,
            m,
            chunk_size,
            compression: Compression::None,
            ..VolumeConfig::default()
        },
        k + m,
        false,
    )
    .unwrap();
    let ino = volume.create_file_path("/stream", 0o600).unwrap();
    (dir, volume, ino)
}

fn bytes_written(volume: &ArgosFs, kind: &str) -> u64 {
    volume
        .commit_metrics()
        .data_plane_byte_snapshots()
        .find(|(label, _)| *label == kind)
        .unwrap()
        .1
}

#[test]
fn four_plus_four_non_eof_read_stat_workload_reduces_reencode_and_shard_bytes() {
    const STRIPE: usize = 1024 * 1024;
    const PART: usize = STRIPE / 8;
    let (_before_dir, before, before_ino) = fixture(4, 4, STRIPE / 4);
    let (_after_dir, after, after_ino) = fixture(4, 4, STRIPE / 4);
    let initial = vec![0x45; STRIPE];
    before.write_inode_range(before_ino, 0, &initial).unwrap();
    after.write_inode_range(after_ino, 0, &initial).unwrap();
    let before_base = before.metadata_snapshot().next_stripe;
    let after_base = after.metadata_snapshot().next_stripe;
    let before_rewrite = bytes_written(&before, "range_rewrite_input");
    let after_rewrite = bytes_written(&after, "range_rewrite_input");
    let before_shards = bytes_written(&before, "shard_payload_written");
    let after_shards = bytes_written(&after, "shard_payload_written");
    let baseline = ArgosFuse::new(before.clone());
    let optimized = ArgosFuse::new(after.clone());
    let mut expected = initial;
    for part in 0..8 {
        let offset = part * PART;
        let patch = vec![part as u8 + 1; PART];
        expected[offset..offset + PART].copy_from_slice(&patch);
        assert!(baseline.queue_writeback(before_ino, offset as u64, &patch));
        assert!(optimized.queue_writeback(after_ino, offset as u64, &patch));

        // Model the old FUSE behavior: each read/stat flushed an incomplete
        // stripe, despite the writes being individually acknowledged.
        baseline.flush_inode_writeback(before_ino).unwrap();
        assert_eq!(
            before.read_inode(before_ino, 0, STRIPE, true).unwrap(),
            expected
        );
        assert_eq!(
            optimized.read_visible_inode(after_ino, 0, STRIPE).unwrap(),
            expected
        );
        assert_eq!(
            optimized.visible_inode_attr(after_ino).unwrap().size,
            STRIPE as u64
        );
        assert_eq!(
            optimized
                .lookup_after_access(1, OsStr::new("stream"))
                .unwrap()
                .size,
            STRIPE as u64
        );
        assert_eq!(
            after.read_inode(after_ino, 0, STRIPE, true).unwrap(),
            vec![0x45; STRIPE]
        );
    }
    assert_eq!(after.metadata_snapshot().next_stripe, after_base);
    let flushes = after
        .commit_metrics()
        .writeback_flush_snapshots()
        .collect::<Vec<_>>();
    assert_eq!(
        flushes
            .iter()
            .find(|(reason, _, _)| *reason == "read_fallback")
            .unwrap()
            .1,
        0
    );
    assert_eq!(
        flushes
            .iter()
            .find(|(reason, _, _)| *reason == "stat_fallback")
            .unwrap()
            .1,
        0
    );
    optimized
        .flush_inode_writeback_reason(after_ino, WritebackFlushReason::Sync)
        .unwrap();
    assert_eq!(
        after
            .commit_metrics()
            .writeback_flush_snapshots()
            .find(|(reason, _, _)| *reason == "sync")
            .unwrap()
            .1,
        1
    );
    after.sync().unwrap();
    assert_eq!(before.metadata_snapshot().next_stripe - before_base, 8);
    assert_eq!(after.metadata_snapshot().next_stripe - after_base, 1);
    assert_eq!(before.read_file("/stream", true).unwrap(), expected);
    assert_eq!(after.read_file("/stream", true).unwrap(), expected);
    assert_eq!(before.transaction_report().unwrap().invalid_entries, 0);
    assert_eq!(after.transaction_report().unwrap().invalid_entries, 0);

    let old_rewrite = bytes_written(&before, "range_rewrite_input") - before_rewrite;
    let new_rewrite = bytes_written(&after, "range_rewrite_input") - after_rewrite;
    let old_shards = bytes_written(&before, "shard_payload_written") - before_shards;
    let new_shards = bytes_written(&after, "shard_payload_written") - after_shards;
    assert_eq!(old_rewrite, 8 * STRIPE as u64);
    assert_eq!(new_rewrite, STRIPE as u64);
    assert!(
        old_shards > new_shards,
        "old={old_shards}, new={new_shards}"
    );
    assert_eq!(
        bytes_written(&before, "range_accepted"),
        bytes_written(&after, "range_accepted")
    );
}

#[test]
fn non_eof_overlap_concurrent_reads_and_flush_never_torn() {
    let (_dir, volume, ino) = fixture(2, 1, 4096);
    let mut expected = vec![0x32; 16 * 1024];
    volume.write_inode_range(ino, 0, &expected).unwrap();
    let fuse = Arc::new(ArgosFuse::new(volume.clone()));
    assert!(fuse.queue_writeback(ino, 4096, &vec![0x13; 8192]));
    assert!(fuse.queue_writeback(ino, 8192, &vec![0x9a; 4096]));
    expected[4096..8192].fill(0x13);
    expected[8192..12288].fill(0x9a);
    assert_eq!(
        fuse.visible_inode_attr(ino).unwrap().size,
        expected.len() as u64
    );
    assert_eq!(fuse.writeback.lock().pending_bytes, 8192);
    let reader = Arc::clone(&fuse);
    let reader_expected = expected.clone();
    let worker = std::thread::spawn(move || {
        for _ in 0..40 {
            assert_eq!(
                reader.read_visible_inode(ino, 0, 16 * 1024).unwrap(),
                reader_expected
            );
            assert_eq!(reader.visible_inode_attr(ino).unwrap().size, 16 * 1024);
        }
    });
    fuse.flush_inode_writeback(ino).unwrap();
    worker.join().unwrap();
    volume.sync().unwrap();
    assert_eq!(volume.read_file("/stream", true).unwrap(), expected);
}

#[test]
fn sparse_non_eof_quota_failure_retains_dirty_data_for_retry() {
    let (_dir, volume, _) = fixture(2, 1, 4096);
    volume.mkdir("/quota", 0o755).unwrap();
    let ino = volume.create_file_path("/quota/data", 0o600).unwrap();
    volume.write_inode_range(ino, 0, b"original").unwrap();
    volume.set_directory_quota("/quota", 8).unwrap();
    let fuse = ArgosFuse::new(volume.clone());
    assert!(fuse.queue_writeback(ino, 12, b"tail"));
    assert_eq!(fuse.visible_inode_attr(ino).unwrap().size, 16);
    assert_eq!(
        fuse.read_visible_inode(ino, 0, 20).unwrap(),
        b"original\0\0\0\0tail"
    );
    assert!(matches!(
        fuse.flush_inode_writeback_reason(ino, WritebackFlushReason::Sync),
        Err(ArgosError::QuotaExceeded { .. })
    ));
    let failures = volume
        .commit_metrics()
        .writeback_flush_snapshots()
        .find(|(reason, _, _)| *reason == "sync")
        .unwrap();
    assert_eq!((failures.1, failures.2), (1, 1));
    assert_eq!(fuse.writeback.lock().pending_bytes, 4);
    assert_eq!(fuse.read_visible_inode(ino, 8, 8).unwrap(), b"\0\0\0\0tail");
    assert_eq!(volume.attr_inode(ino).unwrap().size, 8);
    volume.clear_directory_quota("/quota").unwrap();
    fuse.flush_inode_writeback(ino).unwrap();
    volume.sync().unwrap();
    assert_eq!(
        volume.read_file("/quota/data", true).unwrap(),
        b"original\0\0\0\0tail"
    );
    assert_eq!(fuse.writeback.lock().pending_bytes, 0);
}

#[test]
fn sparse_dirty_hole_read_respects_global_in_memory_limit() {
    let (_dir, volume, ino) = fixture(1, 0, 4096);
    let fuse = ArgosFuse::new(volume);
    let size = crate::volume::MAX_IN_MEMORY_IO_BYTES;
    assert!(fuse.queue_writeback(ino, size as u64, b"tail"));
    let err = fuse.read_visible_inode(ino, 0, size + 1).unwrap_err();
    assert!(matches!(err, ArgosError::FileTooLarge(_)));
    assert_eq!(fuse.writeback.lock().pending_bytes, 4);
    fuse.discard_inode_writeback(ino);
}

#[test]
fn encrypted_non_eof_overwrite_survives_sync_and_reopen() {
    let _env_guard = crate::crypto::test_env_lock();
    let (dir, volume, ino) = fixture(2, 1, 4096);
    std::env::set_var("ARGOSFS_KEY", "non-eof-overlay-test-key");
    volume
        .enable_encryption("non-eof-overlay-test-key")
        .unwrap();
    volume
        .write_inode_range(ino, 0, b"encrypted-payload")
        .unwrap();
    let fuse = ArgosFuse::new(volume.clone());
    assert!(fuse.queue_writeback(ino, 2, b"UPDATED"));
    assert_eq!(
        fuse.read_visible_inode(ino, 0, 17).unwrap(),
        b"enUPDATED-payload"
    );
    assert!(fuse.writeback.lock().dirty.contains_key(&ino));
    fuse.flush_inode_writeback(ino).unwrap();
    volume.sync().unwrap();
    let meta = volume.metadata_snapshot();
    assert!(meta.inodes[&ino].blocks.iter().all(|block| block.encrypted));
    drop(fuse);
    drop(volume);
    let reopened = ArgosFs::open(dir.path()).unwrap();
    assert_eq!(
        reopened.read_file("/stream", true).unwrap(),
        b"enUPDATED-payload"
    );
    assert_eq!(reopened.transaction_report().unwrap().invalid_entries, 0);
    std::env::remove_var("ARGOSFS_KEY");
}

#[test]
fn degraded_non_eof_read_reconstructs_without_eager_rewrite() {
    let (_dir, volume, ino) = fixture(2, 1, 4096);
    volume
        .write_inode_range(ino, 0, b"persistent-shard-data")
        .unwrap();
    let meta = volume.metadata_snapshot();
    let missing_id = meta.inodes[&ino].blocks[0].shards[0].disk_id.clone();
    volume.mark_disk(&missing_id, DiskStatus::Offline).unwrap();
    let fuse = ArgosFuse::new(volume.clone());
    assert!(fuse.queue_writeback(ino, 4, b"UPDATED"));
    assert_eq!(
        fuse.read_visible_inode(ino, 0, 21).unwrap(),
        b"persUPDATEDshard-data"
    );
    assert!(fuse.writeback.lock().dirty.contains_key(&ino));
    assert_eq!(volume.attr_inode(ino).unwrap().size, 21);
}
