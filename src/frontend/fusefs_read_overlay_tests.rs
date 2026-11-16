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
fn overwrite_sparse_and_invalid_dirty_extent_keep_flush_before_read_behavior() {
    let (_dir, volume, ino) = fixture(1, 0, 4096);
    volume.write_file("/stream", b"persisted", 0o600).unwrap();
    let fuse = ArgosFuse::new(volume.clone());
    assert!(fuse.queue_writeback(ino, 1, b"XY"));
    assert_eq!(fuse.read_visible_inode(ino, 0, 10).unwrap(), b"pXYsisted");
    assert!(fuse.writeback.lock().dirty.is_empty());

    assert!(fuse.queue_writeback(ino, 11, b"Z"));
    assert_eq!(
        fuse.read_visible_inode(ino, 0, 20).unwrap(),
        b"pXYsisted\0\0Z"
    );
    assert!(fuse.writeback.lock().dirty.is_empty());
    assert_eq!(volume.attr_inode(ino).unwrap().size, 12);

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
}
