use super::*;
use std::sync::mpsc;

fn fixture() -> (tempfile::TempDir, Vec<PathBuf>, ArgosFs, InodeId) {
    let dir = tempfile::tempdir().unwrap();
    let images = (0..3)
        .map(|i| dir.path().join(format!("reads-{i}.img")))
        .collect::<Vec<_>>();
    let fs = ArgosFs::create_loop(
        &images,
        VolumeConfig {
            k: 2,
            m: 1,
            chunk_size: 4096,
            compression: Compression::None,
            defer_metadata_commit: true,
            defer_data_flush: true,
            ..VolumeConfig::default()
        },
        32 * 1024 * 1024,
        "read-advisory",
        false,
    )
    .unwrap();
    fs.write_file("/file", &vec![0x5a; 8192], 0o600).unwrap();
    fs.sync().unwrap();
    let ino = fs.resolve_path("/file", true).unwrap();
    (dir, images, fs, ino)
}

#[test]
fn normal_reads_do_not_require_exclusive_namespace_lock() {
    let (_dir, _images, fs, ino) = fixture();
    let (tx, rx) = mpsc::channel();
    let reader = fs.clone();
    // A concurrent reader can hold its shared guard indefinitely. The fast
    // path must still finish and account the data without taking meta.write.
    let guard = fs.meta.read();
    let handle = std::thread::spawn(move || {
        tx.send(reader.read_inode(ino, 0, 4096, true)).unwrap();
    });
    let result = rx.recv_timeout(Duration::from_secs(4));
    drop(guard);
    handle.join().unwrap();
    assert_eq!(result.unwrap().unwrap(), vec![0x5a; 4096]);
    assert_eq!(fs.metadata_snapshot().inodes[&ino].read_bytes, 4096);
}

#[test]
fn concurrent_reads_aggregate_without_lost_updates_or_checkpoint_writes() {
    let (_dir, images, fs, ino) = fixture();
    let original = fs.metadata_snapshot();
    let start_txid = original.txid;
    let writers = 8;
    let reads_per_thread = 40;
    let sync = Arc::new(std::sync::Barrier::new(writers));
    let handles = (0..writers)
        .map(|_| {
            let fs = fs.clone();
            let sync = sync.clone();
            std::thread::spawn(move || {
                sync.wait();
                for _ in 0..reads_per_thread {
                    assert_eq!(fs.read_inode(ino, 0, 512, false).unwrap(), vec![0x5a; 512]);
                }
            })
        })
        .collect::<Vec<_>>();
    for handle in handles {
        handle.join().unwrap();
    }
    let total_reads = writers * reads_per_thread;
    let snapshot = fs.metadata_snapshot();
    assert_eq!(
        snapshot.inodes[&ino].read_bytes,
        original.inodes[&ino].read_bytes + (total_reads * 512) as u64
    );
    assert_eq!(
        snapshot.inodes[&ino].access_count,
        original.inodes[&ino].access_count + total_reads as u64
    );
    assert_eq!(snapshot.txid, start_txid);
    assert!(!fs.sync_deferred_if_dirty().unwrap());
    // The live *durable* namespace is not modified by reads; projecting
    // stats onto an explicit snapshot is not a journal transaction.
    assert_eq!(
        fs.meta.read().inodes.get(&ino).unwrap().read_bytes,
        original.inodes[&ino].read_bytes
    );
    assert!(!fs.deferred_commit.lock().raw_advisory_metadata_dirty);
    fs.write_file("/next", b"durable", 0o600).unwrap();
    fs.sync().unwrap();
    assert_eq!(
        fs.metadata_snapshot().inodes[&ino].read_bytes,
        snapshot.inodes[&ino].read_bytes
    );
    drop(fs);
    let reopened = ArgosFs::open_loop(&images, false).unwrap();
    assert_eq!(reopened.read_file("/next", false).unwrap(), b"durable");
    assert_eq!(reopened.transaction_report().unwrap().invalid_entries, 0);
}

#[test]
fn buffered_read_accounting_never_requires_metadata_write_guard() {
    let (_dir, _images, fs, ino) = fixture();
    let (tx, rx) = mpsc::channel();
    let reader = fs.clone();
    let guard = fs.meta.read();
    let handle = std::thread::spawn(move || {
        reader.account_buffered_read(ino, 4096, false);
        reader.account_buffered_read(ino, 512, true);
        tx.send(()).unwrap();
    });
    let result = rx.recv_timeout(Duration::from_secs(4));
    drop(guard);
    handle.join().unwrap();
    result.unwrap();
    let latest = fs.metadata_snapshot();
    assert_eq!(latest.inodes[&ino].access_count, 1);
    assert_eq!(latest.inodes[&ino].read_bytes, 4608);
}

#[test]
fn advisory_runtime_memory_is_bounded_by_distinct_inodes_and_members() {
    let (_dir, _images, fs, ino) = fixture();
    for _ in 0..1500 {
        fs.read_inode(ino, 0, 1024, true).unwrap();
    }
    let (inode_entries, disk_entries) = fs.read_advisory.pending_entries();
    assert_eq!(inode_entries, 1);
    assert!(disk_entries <= 3);
    let latest = fs.metadata_snapshot();
    assert_eq!(latest.inodes[&ino].access_count, 1500);
}
