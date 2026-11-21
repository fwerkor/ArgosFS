use super::*;
use crate::types::{Compression, VolumeConfig};

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
    let ino = volume.create_file_path("/stream", 0o644).unwrap();
    (dir, volume, ino)
}

#[test]
fn full_stripe_flush_keeps_incomplete_eof_tail_visible_and_bounded() {
    const CHUNK: usize = 256 * 1024;
    let (_dir, volume, ino) = fixture(4, 4, CHUNK);
    let initial = vec![0x83; CHUNK];
    volume.write_file("/stream", &initial, 0o644).unwrap();
    let fuse = ArgosFuse::new(volume.clone());
    let mut expected = initial;
    for i in 0..16 {
        let chunk = vec![i as u8; CHUNK];
        assert!(fuse
            .queue_writeback_staged(ino, expected.len() as u64, &chunk)
            .unwrap());
        expected.extend_from_slice(&chunk);
    }
    assert_eq!(volume.attr_inode(ino).unwrap().size, CHUNK as u64);
    assert_eq!(
        fuse.writeback.lock().pending_bytes,
        FUSE_WRITEBACK_MAX_BYTES
    );

    let last = vec![0xe7; CHUNK];
    assert!(fuse
        .queue_writeback_staged(ino, expected.len() as u64, &last)
        .unwrap());
    expected.extend_from_slice(&last);
    // 3.75 MiB of buffered data advanced the persisted EOF from 0.25 MiB
    // to precisely 4 MiB. The 0.25 MiB tail and the new chunk stay dirty.
    assert_eq!(
        volume.attr_inode(ino).unwrap().size,
        (4 * 1024 * 1024) as u64
    );
    {
        let wb = fuse.writeback.lock();
        let dirty = &wb.dirty[&ino];
        assert_eq!(dirty.offset, (4 * 1024 * 1024) as u64);
        assert_eq!(dirty.data.len(), 2 * CHUNK);
        assert_eq!(wb.pending_bytes, 2 * CHUNK);
    }
    assert_eq!(
        fuse.visible_inode_attr(ino).unwrap().size,
        expected.len() as u64
    );
    assert_eq!(
        fuse.read_visible_inode(ino, 0, expected.len()).unwrap(),
        expected
    );
    fuse.flush_inode_writeback(ino).unwrap();
    volume.sync().unwrap();
    assert_eq!(fuse.writeback.lock().pending_bytes, 0);
    assert_eq!(volume.read_file("/stream", true).unwrap(), expected);
}

#[test]
fn staging_an_unaligned_tail_encodes_fewer_stripes_than_full_buffer_flush() {
    const CHUNK: usize = 256 * 1024;
    let (_before_dir, before, before_ino) = fixture(4, 4, CHUNK);
    let (_after_dir, after, after_ino) = fixture(4, 4, CHUNK);
    let initial = vec![0x83; CHUNK];
    for volume in [&before, &after] {
        volume.write_file("/stream", &initial, 0o644).unwrap();
    }
    let old = ArgosFuse::new(before.clone());
    let new = ArgosFuse::new(after.clone());
    let start_old = before.metadata_snapshot().next_stripe;
    let start_new = after.metadata_snapshot().next_stripe;
    let mut expected = initial;
    for i in 0..16 {
        let chunk = vec![i as u8; CHUNK];
        assert!(old.queue_writeback(before_ino, expected.len() as u64, &chunk));
        assert!(new
            .queue_writeback_staged(after_ino, expected.len() as u64, &chunk)
            .unwrap());
        expected.extend_from_slice(&chunk);
    }
    old.flush_inode_writeback(before_ino).unwrap();
    let last = vec![0xe7; CHUNK];
    assert!(old.queue_writeback(before_ino, expected.len() as u64, &last));
    assert!(new
        .queue_writeback_staged(after_ino, expected.len() as u64, &last)
        .unwrap());
    expected.extend_from_slice(&last);
    old.flush_inode_writeback(before_ino).unwrap();
    new.flush_inode_writeback(after_ino).unwrap();
    assert_eq!(before.read_file("/stream", true).unwrap(), expected);
    assert_eq!(after.read_file("/stream", true).unwrap(), expected);
    let old_stripes = before.metadata_snapshot().next_stripe - start_old;
    let new_stripes = after.metadata_snapshot().next_stripe - start_new;
    assert!(
        new_stripes < old_stripes,
        "old={old_stripes}, new={new_stripes}"
    );
}

#[test]
fn nonsequential_write_cannot_trigger_partial_prefix_staging() {
    let (_dir, volume, ino) = fixture(2, 1, 4096);
    let fuse = ArgosFuse::new(volume.clone());
    let bytes = vec![0x11; FUSE_WRITEBACK_MAX_BYTES];
    assert!(fuse.queue_writeback(ino, 0, &bytes));
    assert!(!fuse
        .queue_writeback_staged(ino, bytes.len() as u64 + 2, b"gap")
        .unwrap());
    assert_eq!(volume.attr_inode(ino).unwrap().size, 0);
    assert_eq!(fuse.writeback.lock().pending_bytes, bytes.len());
    fuse.discard_inode_writeback(ino);
    assert_eq!(fuse.writeback.lock().pending_bytes, 0);
}

#[test]
fn total_mount_budget_prevents_unbounded_per_inode_accumulation() {
    let (_dir, volume, first) = fixture(1, 0, 4096);
    let fuse = ArgosFuse::new(volume.clone());
    let bytes = vec![0x33; FUSE_WRITEBACK_MAX_BYTES];
    let mut inodes = vec![first];
    for i in 1..=16 {
        inodes.push(
            volume
                .create_file_path(&format!("/stream-{i}"), 0o644)
                .unwrap(),
        );
    }
    for &ino in inodes.iter().take(16) {
        assert!(fuse.queue_writeback(ino, 0, &bytes));
    }
    assert_eq!(
        fuse.writeback.lock().pending_bytes,
        FUSE_WRITEBACK_MAX_MOUNT_BYTES
    );
    assert!(!fuse.queue_writeback(*inodes.last().unwrap(), 0, b"a"));
    assert_eq!(fuse.writeback.lock().dirty.len(), 16);
    fuse.discard_inode_writeback(first);
    assert!(fuse.queue_writeback(*inodes.last().unwrap(), 0, b"a"));
    assert_eq!(
        fuse.writeback.lock().pending_bytes,
        15 * FUSE_WRITEBACK_MAX_BYTES + 1
    );
    for ino in inodes {
        fuse.discard_inode_writeback(ino);
    }
    assert_eq!(fuse.writeback.lock().pending_bytes, 0);
}

#[test]
fn raw_loop_stripe_prefix_and_tail_survive_sync_and_reopen() {
    const CHUNK: usize = 256 * 1024;
    let dir = tempfile::tempdir().unwrap();
    let images = (0..3)
        .map(|i| dir.path().join(format!("staged-{i}.img")))
        .collect::<Vec<_>>();
    let volume = ArgosFs::create_loop(
        &images,
        VolumeConfig {
            k: 2,
            m: 1,
            chunk_size: CHUNK,
            compression: Compression::None,
            defer_journal_flush: true,
            defer_metadata_commit: true,
            defer_data_flush: true,
            ..VolumeConfig::default()
        },
        64 * 1024 * 1024,
        "staged-stripes-loop-test",
        false,
    )
    .unwrap();
    let ino = volume.create_file_path("/stream", 0o600).unwrap();
    let mut expected = vec![0x71; CHUNK];
    volume.write_inode_range(ino, 0, &expected).unwrap();
    volume.sync().unwrap();
    let fuse = ArgosFuse::new(volume.clone());
    for i in 0..17 {
        let data = vec![i as u8 + 1; CHUNK];
        assert!(fuse
            .queue_writeback_staged(ino, expected.len() as u64, &data)
            .unwrap());
        expected.extend_from_slice(&data);
    }
    // The 4 MiB cap has caused exactly one completed-prefix write, while
    // the latest tail remains visible to FUSE until fsync/close.
    assert_eq!(volume.attr_inode(ino).unwrap().size, 4 * 1024 * 1024);
    assert_eq!(
        fuse.read_visible_inode(ino, 0, expected.len()).unwrap(),
        expected
    );
    assert_eq!(fuse.writeback.lock().pending_bytes, 2 * CHUNK);
    fuse.flush_inode_writeback(ino).unwrap();
    volume.sync().unwrap();
    assert_eq!(volume.transaction_report().unwrap().invalid_entries, 0);
    drop(fuse);
    drop(volume);
    let reopened = ArgosFs::open_loop(&images, false).unwrap();
    assert_eq!(reopened.read_file("/stream", false).unwrap(), expected);
    assert_eq!(reopened.transaction_report().unwrap().invalid_entries, 0);
    assert_eq!(reopened.fsck(false, false).unwrap().missing_shards, 0);
}

#[test]
fn repeated_partial_stripe_staging_preserves_data_and_byte_budget() {
    const STEP: usize = 128 * 1024;
    let (_dir, volume, ino) = fixture(2, 1, 4096);
    let initial = vec![0x59; 512];
    volume.write_file("/stream", &initial, 0o644).unwrap();
    let fuse = ArgosFuse::new(volume.clone());
    let mut expected = initial;
    for i in 0..82 {
        let bytes = vec![i as u8; STEP];
        assert!(fuse
            .queue_writeback_staged(ino, expected.len() as u64, &bytes)
            .unwrap());
        expected.extend_from_slice(&bytes);
        let wb = fuse.writeback.lock();
        assert_eq!(wb.pending_bytes, wb.dirty[&ino].data.len());
        assert!(wb.pending_bytes <= FUSE_WRITEBACK_MAX_BYTES);
    }
    assert_eq!(
        fuse.visible_inode_attr(ino).unwrap().size,
        expected.len() as u64
    );
    assert_eq!(
        fuse.read_visible_inode(ino, 0, expected.len()).unwrap(),
        expected
    );
    fuse.flush_inode_writeback(ino).unwrap();
    assert_eq!(fuse.writeback.lock().pending_bytes, 0);
    assert_eq!(volume.read_file("/stream", true).unwrap(), expected);
}
