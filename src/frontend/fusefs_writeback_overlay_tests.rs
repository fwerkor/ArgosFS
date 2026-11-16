use super::*;
use crate::types::{Compression, VolumeConfig};
use std::ffi::OsStr;
use std::sync::Arc;

fn make_volume(k: usize, m: usize, chunk_size: usize) -> (tempfile::TempDir, ArgosFs, InodeId) {
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
    let ino = volume.create_file_path("/stream", 0o6755).unwrap();
    (dir, volume, ino)
}

#[test]
fn append_getattr_and_lookup_preserve_stripe_until_required_flush() {
    let (_dir, volume, ino) = make_volume(4, 4, 256 * 1024);
    let fuse = ArgosFuse::new(volume.clone());
    let mut expected = Vec::new();
    for part in 0..4 {
        let bytes = vec![part as u8 + 13; 256 * 1024];
        assert!(fuse.queue_writeback(ino, expected.len() as u64, &bytes));
        expected.extend_from_slice(&bytes);
        let attr = fuse.visible_inode_attr(ino).unwrap();
        assert_eq!(attr.size, expected.len() as u64);
        assert_eq!(attr.blocks, (expected.len() as u64).div_ceil(512));
        assert_eq!(attr.mode & (libc::S_ISUID | libc::S_ISGID), 0);
        assert!(attr.mtime >= volume.attr_inode(ino).unwrap().mtime);
        assert_eq!(
            fuse.lookup_after_access(1, OsStr::new("stream"))
                .unwrap()
                .size,
            expected.len() as u64
        );
        assert_eq!(volume.attr_inode(ino).unwrap().size, 0);
    }
    // Dirty bytes remain one complete logical 1 MiB stripe, and have not
    // been written as four separate partial-stripe COW replacements.
    assert_eq!(
        fuse.writeback.lock().dirty.get(&ino).unwrap().data,
        expected
    );
    let staged = fuse.visible_inode_attr(ino).unwrap();
    std::thread::sleep(Duration::from_millis(15));
    fuse.flush_inode_writeback(ino).unwrap();
    volume.sync().unwrap();
    let persisted = volume.attr_inode(ino).unwrap();
    assert_eq!(persisted.mtime, staged.mtime);
    assert_eq!(persisted.ctime, staged.ctime);
    assert_eq!(volume.read_file("/stream", true).unwrap(), expected);
    let inode = volume.metadata_snapshot().inodes[&ino].clone();
    assert_eq!(inode.blocks.len(), 1);
    assert_eq!(inode.blocks[0].shards.len(), 8);
    assert_eq!(inode.mode & (libc::S_ISUID | libc::S_ISGID), 0);
    assert!(fuse.writeback.lock().dirty.is_empty());
}

#[test]
fn only_eof_append_can_skip_writeback_flush_on_stat() {
    let (_dir, volume, ino) = make_volume(2, 1, 4096);
    volume.write_file("/stream", b"persisted", 0o644).unwrap();
    let fuse = ArgosFuse::new(volume.clone());

    // An overwrite changes the existing bytes, so the existing flush path
    // is still required for the metadata and subsequent read semantics.
    assert!(fuse.queue_writeback(ino, 1, b"XY"));
    assert_eq!(fuse.visible_inode_attr(ino).unwrap().size, 9);
    assert!(fuse.writeback.lock().dirty.is_empty());
    assert_eq!(volume.read_file("/stream", true).unwrap(), b"pXYsisted");

    // A sparse write cannot be modeled as a simple contiguous EOF append.
    assert!(fuse.queue_writeback(ino, 11, b"Z"));
    assert_eq!(fuse.visible_inode_attr(ino).unwrap().size, 12);
    assert!(fuse.writeback.lock().dirty.is_empty());
    assert_eq!(
        volume.read_file("/stream", true).unwrap(),
        b"pXYsisted\0\0Z"
    );
}

#[test]
fn dirty_failed_writeback_is_retained_and_never_advertised_as_a_valid_append() {
    let (_dir, volume, ino) = make_volume(1, 0, 4096);
    let fuse = ArgosFuse::new(volume.clone());
    assert!(fuse.queue_writeback(ino, u64::MAX, b"nope"));
    assert!(fuse.visible_inode_attr(ino).is_err());
    assert!(fuse.writeback.lock().dirty.contains_key(&ino));
    assert_eq!(volume.attr_inode(ino).unwrap().size, 0);
    fuse.discard_inode_writeback(ino);
}

#[test]
fn concurrent_stat_and_flush_never_publish_stale_metadata_size() {
    let (_dir, volume, ino) = make_volume(2, 1, 4096);
    let fuse = Arc::new(ArgosFuse::new(volume.clone()));
    let expected = vec![3u8; 32 * 1024];
    assert!(fuse.queue_writeback(ino, 0, &expected));
    let clone = Arc::clone(&fuse);
    let worker = std::thread::spawn(move || {
        for _ in 0..50 {
            let attr = clone.visible_inode_attr(ino).unwrap();
            assert_eq!(attr.size, 32 * 1024);
        }
    });
    fuse.flush_inode_writeback(ino).unwrap();
    worker.join().unwrap();
    assert_eq!(fuse.visible_inode_attr(ino).unwrap().size, 32 * 1024);
    assert_eq!(volume.read_file("/stream", false).unwrap(), expected);
}

#[test]
fn future_mtime_is_replaced_by_acknowledged_append_time_and_remains_stable() {
    let (_dir, volume, ino) = make_volume(1, 0, 4096);
    let future = now_f64() + 3600.0;
    volume.utimens_inode(ino, future, future).unwrap();
    let fuse = ArgosFuse::new(volume.clone());
    assert!(fuse.queue_writeback(ino, 0, b"acknowledged"));
    let staged = fuse.visible_inode_attr(ino).unwrap();
    assert!(
        staged.mtime < future,
        "pending write must replace a future mtime"
    );
    std::thread::sleep(Duration::from_millis(20));
    fuse.flush_inode_writeback(ino).unwrap();
    let persisted = volume.attr_inode(ino).unwrap();
    assert_eq!(persisted.mtime, staged.mtime);
    assert_eq!(persisted.ctime, staged.ctime);
    assert_eq!(volume.read_file("/stream", true).unwrap(), b"acknowledged");
}

#[test]
fn partial_stripe_rewrite_keeps_acknowledged_write_time_through_flush() {
    let (_dir, volume, ino) = make_volume(2, 1, 4096);
    volume.write_file("/stream", b"original", 0o644).unwrap();
    let fuse = ArgosFuse::new(volume.clone());
    assert!(fuse.queue_writeback(ino, 8, b"tail"));
    let staged = fuse.visible_inode_attr(ino).unwrap();
    assert_eq!(staged.size, 12);
    std::thread::sleep(Duration::from_millis(20));
    fuse.flush_inode_writeback(ino).unwrap();
    let persisted = volume.attr_inode(ino).unwrap();
    assert_eq!(persisted.mtime, staged.mtime);
    assert_eq!(persisted.ctime, staged.ctime);
    assert_eq!(volume.read_file("/stream", true).unwrap(), b"originaltail");
}

#[test]
fn unlink_open_file_keeps_later_ctime_through_buffered_append_flush() {
    let (_dir, volume, ino) = make_volume(1, 0, 4096);
    let fuse = ArgosFuse::new(volume.clone());
    let open_handle = fuse.handles.lock().open(OpenFileHandle {
        ino,
        flags: libc::O_RDWR,
    });
    assert!(fuse.queue_writeback(ino, 0, b"unlinked-open"));
    let staged_before_unlink = fuse.visible_inode_attr(ino).unwrap();
    std::thread::sleep(Duration::from_millis(20));
    fuse.unlink_after_access(1, OsStr::new("stream"), 0)
        .unwrap();
    let unlinked = volume.attr_inode(ino).unwrap();
    assert_eq!(unlinked.nlink, 0);
    assert!(unlinked.ctime > staged_before_unlink.ctime);

    let staged_after_unlink = fuse.visible_inode_attr(ino).unwrap();
    assert_eq!(staged_after_unlink.size, 13);
    assert_eq!(staged_after_unlink.ctime, unlinked.ctime);
    assert_eq!(staged_after_unlink.mtime, staged_before_unlink.mtime);
    std::thread::sleep(Duration::from_millis(20));
    fuse.flush_inode_writeback(ino).unwrap();
    let persisted = volume.attr_inode(ino).unwrap();
    assert_eq!(persisted.ctime, unlinked.ctime);
    assert_eq!(persisted.mtime, staged_before_unlink.mtime);
    assert_eq!(
        volume.read_inode(ino, 0, 30, true).unwrap(),
        b"unlinked-open"
    );
    assert!(fuse.handles.lock().close(open_handle).unwrap().1);
}

#[test]
fn append_after_unlink_supersedes_unlink_ctime_before_final_flush() {
    let (_dir, volume, ino) = make_volume(1, 0, 4096);
    let fuse = ArgosFuse::new(volume.clone());
    let fh = fuse.handles.lock().open(OpenFileHandle {
        ino,
        flags: libc::O_RDWR,
    });
    assert!(fuse.queue_writeback(ino, 0, b"before"));
    std::thread::sleep(Duration::from_millis(15));
    fuse.unlink_after_access(1, OsStr::new("stream"), 0)
        .unwrap();
    let unlink_ctime = volume.attr_inode(ino).unwrap().ctime;
    std::thread::sleep(Duration::from_millis(15));
    assert!(fuse.queue_writeback(ino, 6, b"after"));
    let staged = fuse.visible_inode_attr(ino).unwrap();
    assert!(staged.ctime > unlink_ctime);
    fuse.flush_inode_writeback(ino).unwrap();
    let persisted = volume.attr_inode(ino).unwrap();
    assert_eq!(persisted.ctime, staged.ctime);
    assert_eq!(persisted.mtime, staged.mtime);
    assert_eq!(volume.read_inode(ino, 0, 30, true).unwrap(), b"beforeafter");
    assert!(fuse.handles.lock().close(fh).unwrap().1);
}
