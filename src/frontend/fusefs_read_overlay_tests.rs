use super::*;
use crate::types::{Compression, VolumeConfig};
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

#[test]
fn buffered_four_plus_four_read_after_write_does_not_flush_partial_stripes() {
    let (_dir, volume, ino) = fixture(4, 4, 256 * 1024);
    let fuse = ArgosFuse::new(volume.clone());
    let mut bytes = Vec::new();
    for part in 0..4 {
        let chunk = vec![part as u8 + 7; 256 * 1024];
        assert!(fuse.queue_writeback(ino, bytes.len() as u64, &chunk));
        bytes.extend_from_slice(&chunk);
        assert_eq!(fuse.read_visible_inode(ino, 0, bytes.len()).unwrap(), bytes);
        assert_eq!(
            fuse.read_visible_inode(ino, ((part * 256 + 128) * 1024) as u64, 64 * 1024)
                .unwrap(),
            vec![part as u8 + 7; 64 * 1024]
        );
        assert_eq!(volume.attr_inode(ino).unwrap().size, 0);
        assert!(volume.metadata_snapshot().inodes[&ino].blocks.is_empty());
    }
    assert_eq!(
        fuse.read_visible_inode(ino, bytes.len() as u64, 1).unwrap(),
        b""
    );
    assert_eq!(fuse.read_visible_inode(ino, 42, 0).unwrap(), b"");
    assert_eq!(fuse.read_visible_inode(ino, u64::MAX, 2048).unwrap(), b"");
    assert_eq!(
        fuse.visible_inode_attr(ino).unwrap().size,
        bytes.len() as u64
    );
    fuse.flush_inode_writeback(ino).unwrap();
    assert_eq!(volume.read_file("/stream", true).unwrap(), bytes);
    assert_eq!(volume.metadata_snapshot().inodes[&ino].blocks.len(), 1);
    assert!(fuse.writeback.lock().dirty.is_empty());
}

#[test]
fn mixed_persisted_prefix_and_buffered_tail_return_exact_byte_ranges() {
    let (_dir, volume, ino) = fixture(2, 1, 4096);
    volume.write_file("/stream", b"12345678", 0o600).unwrap();
    let fuse = ArgosFuse::new(volume.clone());
    assert!(fuse.queue_writeback(ino, 8, b"abcdefgh"));
    assert_eq!(
        fuse.read_visible_inode(ino, 0, 16).unwrap(),
        b"12345678abcdefgh"
    );
    assert_eq!(fuse.read_visible_inode(ino, 6, 4).unwrap(), b"78ab");
    assert_eq!(fuse.read_visible_inode(ino, 7, 7).unwrap(), b"8abcdef");
    assert_eq!(fuse.read_visible_inode(ino, 8, 8).unwrap(), b"abcdefgh");
    assert_eq!(fuse.read_visible_inode(ino, 14, 16).unwrap(), b"gh");
    assert_eq!(fuse.read_visible_inode(ino, 16, 6).unwrap(), b"");
    assert_eq!(volume.attr_inode(ino).unwrap().size, 8);
    assert_eq!(fuse.writeback.lock().dirty[&ino].data, b"abcdefgh");
    fuse.flush_inode_writeback(ino).unwrap();
    assert_eq!(
        volume.read_file("/stream", true).unwrap(),
        b"12345678abcdefgh"
    );
}

#[test]
fn overwrite_sparse_and_invalid_dirty_extent_preserve_read_semantics() {
    let (_dir, volume, ino) = fixture(1, 0, 4096);
    volume.write_file("/stream", b"persisted", 0o600).unwrap();
    let fuse = ArgosFuse::new(volume.clone());
    assert!(fuse.queue_writeback(ino, 1, b"XY"));
    assert_eq!(fuse.read_visible_inode(ino, 0, 10).unwrap(), b"pXYsisted");
    assert!(fuse.writeback.lock().dirty.contains_key(&ino));
    fuse.flush_inode_writeback(ino).unwrap();

    assert!(fuse.queue_writeback(ino, 11, b"Z"));
    assert_eq!(
        fuse.read_visible_inode(ino, 0, 20).unwrap(),
        b"pXYsisted\0\0Z"
    );
    assert!(fuse.writeback.lock().dirty.contains_key(&ino));
    fuse.flush_inode_writeback(ino).unwrap();

    assert!(fuse.queue_writeback(ino, u64::MAX, b"error"));
    assert!(fuse.read_visible_inode(ino, 0, 20).is_err());
    assert!(fuse.writeback.lock().dirty.contains_key(&ino));
}

#[test]
fn concurrent_reads_and_flush_never_return_a_torn_dirty_tail() {
    let (_dir, volume, ino) = fixture(2, 1, 4096);
    let fuse = Arc::new(ArgosFuse::new(volume.clone()));
    let content = vec![0x4eu8; 256 * 1024];
    assert!(fuse.queue_writeback(ino, 0, &content));
    let reader = Arc::clone(&fuse);
    let expected = content.clone();
    let worker = std::thread::spawn(move || {
        for _ in 0..60 {
            assert_eq!(
                reader.read_visible_inode(ino, 0, 256 * 1024).unwrap(),
                expected
            );
        }
    });
    fuse.flush_inode_writeback(ino).unwrap();
    worker.join().unwrap();
    assert_eq!(volume.read_file("/stream", true).unwrap(), content);
}

#[test]
fn corrupted_persisted_prefix_fails_closed_without_dropping_dirty_append() {
    let (_dir, volume, ino) = fixture(1, 0, 4096);
    volume.write_file("/stream", b"physical", 0o600).unwrap();
    let meta = volume.metadata_snapshot();
    let shard = &meta.inodes[&ino].blocks[0].shards[0];
    let path = volume
        .root()
        .join(&meta.disks[&shard.disk_id].path)
        .join(&shard.relpath);
    let mut bytes = std::fs::read(&path).unwrap();
    bytes[0] ^= 0x01;
    std::fs::write(&path, bytes).unwrap();
    let fuse = ArgosFuse::new(volume.clone());
    assert!(fuse.queue_writeback(ino, 8, b"buffered"));
    assert!(fuse.read_visible_inode(ino, 0, 16).is_err());
    assert_eq!(volume.attr_inode(ino).unwrap().size, 8);
    assert_eq!(fuse.writeback.lock().dirty[&ino].data, b"buffered");
    assert_eq!(fuse.read_visible_inode(ino, 8, 8).unwrap(), b"buffered");
}

#[test]
fn buffered_read_accounting_counts_visible_bytes_once_and_not_partial_flushes() {
    let (_dir, volume, ino) = fixture(2, 1, 4096);
    volume.write_file("/stream", b"persisted", 0o600).unwrap();
    let before = volume.metadata_snapshot().inodes[&ino].clone();
    let fuse = ArgosFuse::new(volume.clone());
    assert!(fuse.queue_writeback(ino, 9, b"buffer"));
    assert_eq!(
        fuse.read_visible_inode(ino, 0, 15).unwrap(),
        b"persistedbuffer"
    );
    let after_mixed = volume.metadata_snapshot().inodes[&ino].clone();
    assert_eq!(after_mixed.access_count, before.access_count + 1);
    assert_eq!(after_mixed.read_bytes, before.read_bytes + 15);
    assert_eq!(fuse.read_visible_inode(ino, 9, 6).unwrap(), b"buffer");
    let after_buffered = volume.metadata_snapshot().inodes[&ino].clone();
    assert_eq!(after_buffered.access_count, before.access_count + 2);
    assert_eq!(after_buffered.read_bytes, before.read_bytes + 21);
    assert_eq!(volume.attr_inode(ino).unwrap().size, 9);
    assert_eq!(fuse.writeback.lock().dirty[&ino].data, b"buffer");
    assert_eq!(fuse.read_visible_inode(ino, 15, 8).unwrap(), b"");
    assert_eq!(fuse.read_visible_inode(ino, 0, 0).unwrap(), b"");
    let after_eof = volume.metadata_snapshot().inodes[&ino].clone();
    assert_eq!(after_eof.access_count, before.access_count + 4);
    assert_eq!(after_eof.read_bytes, before.read_bytes + 21);
}

#[test]
fn interleaved_reads_reduce_real_four_plus_four_cow_stripes() {
    let (_baseline_dir, baseline, baseline_ino) = fixture(4, 4, 256 * 1024);
    let (_optimized_dir, optimized, optimized_ino) = fixture(4, 4, 256 * 1024);
    let baseline_fuse = ArgosFuse::new(baseline.clone());
    let optimized_fuse = ArgosFuse::new(optimized.clone());
    let baseline_start = baseline.metadata_snapshot().next_stripe;
    let optimized_start = optimized.metadata_snapshot().next_stripe;
    let mut expected = Vec::new();
    for i in 0..4 {
        let chunk = vec![i as u8 + 1; 256 * 1024];
        assert!(baseline_fuse.queue_writeback(baseline_ino, expected.len() as u64, &chunk));
        assert!(optimized_fuse.queue_writeback(optimized_ino, expected.len() as u64, &chunk));
        expected.extend_from_slice(&chunk);

        // This is the original read-before-every-append behavior: each read
        // forces a partial-stripe COW rewrite, even with a 4 MiB buffer.
        baseline_fuse.flush_inode_writeback(baseline_ino).unwrap();
        assert_eq!(
            baseline
                .read_inode(baseline_ino, 0, expected.len(), true)
                .unwrap(),
            expected
        );
        // The new read path is immediately consistent but keeps the 1 MiB
        // systematic input buffered until the explicit durability boundary.
        assert_eq!(
            optimized_fuse
                .read_visible_inode(optimized_ino, 0, expected.len())
                .unwrap(),
            expected
        );
    }
    optimized_fuse.flush_inode_writeback(optimized_ino).unwrap();
    let baseline_stripes = baseline.metadata_snapshot().next_stripe - baseline_start;
    let optimized_stripes = optimized.metadata_snapshot().next_stripe - optimized_start;
    assert_eq!(
        baseline_stripes, 4,
        "baseline must actually rewrite incomplete stripes"
    );
    assert_eq!(
        optimized_stripes, 1,
        "interleaved reads should produce one full-stripe encode"
    );
    assert_eq!(baseline.read_file("/stream", true).unwrap(), expected);
    assert_eq!(optimized.read_file("/stream", true).unwrap(), expected);
}

#[test]
fn repair_of_corrupted_persisted_prefix_still_counts_full_combined_read() {
    let (_dir, volume, ino) = fixture(2, 1, 4096);
    let persisted = b"abcdefgh";
    volume.write_file("/stream", persisted, 0o600).unwrap();
    let meta = volume.metadata_snapshot();
    let shard = meta.inodes[&ino].blocks[0]
        .shards
        .iter()
        .find(|shard| shard.slot == 0)
        .unwrap();
    let path = volume
        .root()
        .join(&meta.disks[&shard.disk_id].path)
        .join(&shard.relpath);
    let mut physical = std::fs::read(&path).unwrap();
    physical[0] ^= 0x44;
    std::fs::write(path, physical).unwrap();

    let before = volume.metadata_snapshot().inodes[&ino].clone();
    let fuse = ArgosFuse::new(volume.clone());
    assert!(fuse.queue_writeback(ino, 8, b"tail"));
    // The prefix is repairable using the remaining two members, but the FUSE
    // read still needs to count the prefix+tail once, without committing tail.
    assert_eq!(
        fuse.read_visible_inode(ino, 0, 12).unwrap(),
        b"abcdefghtail"
    );
    let after = volume.metadata_snapshot().inodes[&ino].clone();
    assert_eq!(after.access_count, before.access_count + 1);
    assert_eq!(after.read_bytes, before.read_bytes + 12);
    assert_eq!(volume.attr_inode(ino).unwrap().size, 8);
    assert_eq!(fuse.writeback.lock().dirty[&ino].data, b"tail");
    assert_eq!(volume.fsck(false, false).unwrap().missing_shards, 0);
}

#[test]
fn raw_buffered_tail_reads_are_advisory_and_never_force_periodic_checkpoint() {
    use crate::commit_metrics::CommitPhase;

    let dir = tempfile::tempdir().unwrap();
    let images = (0..3)
        .map(|n| dir.path().join(format!("raw-buffer-read-{n}.img")))
        .collect::<Vec<_>>();
    let volume = ArgosFs::create_loop(
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
        "buffered-tail-read-telemetry",
        false,
    )
    .unwrap();
    let ino = volume.create_file_path("/stream", 0o600).unwrap();
    volume.write_inode_range(ino, 0, b"persisted").unwrap();
    volume.sync().unwrap();
    let initial = volume.metadata_snapshot();
    let count = |phase: CommitPhase| {
        volume
            .commit_metrics()
            .snapshots()
            .find(|(name, _)| *name == phase.name())
            .unwrap()
            .1
            .count
    };
    let checkpoint_before = count(CommitPhase::Checkpoint);
    let journal_before = count(CommitPhase::Journal);
    let group_before = count(CommitPhase::GroupCommit);
    let fuse = ArgosFuse::new(volume.clone());
    assert!(fuse.queue_writeback(ino, 9, b"buffered"));

    for _ in 0..40 {
        // The prefix is checked by the raw decoder; only the volatile tail
        // is served from FUSE's acknowledged append buffer.
        assert_eq!(
            fuse.read_visible_inode(ino, 0, 17).unwrap(),
            b"persistedbuffered"
        );
        assert_eq!(fuse.read_visible_inode(ino, 9, 8).unwrap(), b"buffered");
        assert!(fuse.read_visible_inode(ino, 17, 1).unwrap().is_empty());
        assert!(fuse.read_visible_inode(ino, 0, 0).unwrap().is_empty());
        assert!(!volume.sync_deferred_if_dirty().unwrap());
        volume.sync().unwrap();
    }
    assert_eq!(volume.metadata_snapshot().txid, initial.txid);
    assert_eq!(count(CommitPhase::Checkpoint), checkpoint_before);
    assert_eq!(count(CommitPhase::Journal), journal_before);
    assert_eq!(count(CommitPhase::GroupCommit), group_before);
    assert_eq!(volume.attr_inode(ino).unwrap().size, 9);
    assert_eq!(fuse.writeback.lock().dirty[&ino].data, b"buffered");

    // An actual writeback remains durable and its metadata/journal must
    // re-anchor the trusted previous hash despite advisory read counters.
    fuse.flush_inode_writeback(ino).unwrap();
    volume.sync().unwrap();
    assert_eq!(
        volume.read_inode(ino, 0, 17, false).unwrap(),
        b"persistedbuffered"
    );
    assert_eq!(volume.transaction_report().unwrap().invalid_entries, 0);
    drop(fuse);
    drop(volume);
    let reopened = ArgosFs::open_loop(&images, false).unwrap();
    assert_eq!(
        reopened.read_file("/stream", false).unwrap(),
        b"persistedbuffered"
    );
    assert_eq!(reopened.transaction_report().unwrap().invalid_entries, 0);
}
