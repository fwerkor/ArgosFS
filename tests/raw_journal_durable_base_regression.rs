use argosfs::journal;
use argosfs::types::{Compression, VolumeConfig};
use argosfs::ArgosFs;
use tempfile::TempDir;

fn config() -> VolumeConfig {
    VolumeConfig {
        k: 1,
        m: 0,
        chunk_size: 1024,
        compression: Compression::None,
        compression_level: 0,
        l2_cache_bytes: 0,
        fsname: "argosfs-durable-base-regression".to_string(),
        ..VolumeConfig::default()
    }
}

#[test]
fn raw_journal_replays_write_after_read_side_metadata_mutation() {
    let tmp = TempDir::new().unwrap();
    let image = tmp.path().join("disk.img");
    let images = vec![image];
    let fs = ArgosFs::create_loop(
        &images,
        config(),
        32 * 1024 * 1024,
        "durable-base-regression",
        false,
    )
    .unwrap();

    let seed_payload = vec![0x5a; 2048];
    fs.write_file("/seed", &seed_payload, 0o644).unwrap();
    let seed = fs.resolve_path("/seed", true).unwrap();
    let before_snapshot = fs.metadata_snapshot();
    assert!(!before_snapshot.inodes[&seed].blocks.is_empty());
    let before = before_snapshot.inodes[&seed].access_count;
    assert_eq!(fs.read_file("/seed", false).unwrap(), seed_payload);
    assert!(fs.metadata_snapshot().inodes[&seed].access_count > before);

    fs.write_file("/after-read", b"must survive replay", 0o644)
        .unwrap();
    let committed_txid = fs.metadata_snapshot().txid;

    // Model power loss: keep the journal newer than the metadata checkpoint and
    // skip ArgosFs::drop's clean-unmount path.
    std::mem::forget(fs);

    let reopened = ArgosFs::open_loop(&images, false).unwrap();
    assert_eq!(
        reopened.read_file("/after-read", false).unwrap(),
        b"must survive replay"
    );
    assert_eq!(reopened.metadata_snapshot().txid, committed_txid);
}


#[test]
fn raw_sync_after_read_refreshes_integrity_and_dirty_state() {
    let tmp = TempDir::new().unwrap();
    let image = tmp.path().join("disk-sync.img");
    let images = vec![image];
    let fs = ArgosFs::create_loop(
        &images,
        config(),
        32 * 1024 * 1024,
        "read-sync-regression",
        false,
    )
    .unwrap();

    let seed_payload = vec![0x5a; 2048];
    fs.write_file("/seed", &seed_payload, 0o644).unwrap();
    let seed = fs.resolve_path("/seed", true).unwrap();
    assert!(!fs.metadata_snapshot().inodes[&seed].blocks.is_empty());
    assert_eq!(fs.read_file("/seed", false).unwrap(), seed_payload);
    fs.sync().unwrap();

    let synced = fs.metadata_snapshot();
    assert_eq!(
        synced.integrity.meta_hash,
        journal::canonical_metadata_hash(&synced).unwrap()
    );

    fs.write_file("/after-sync", b"must survive replay", 0o644)
        .unwrap();
    let committed_txid = fs.metadata_snapshot().txid;
    std::mem::forget(fs);

    let reopened = ArgosFs::open_loop(&images, false).unwrap();
    assert_eq!(
        reopened.read_file("/after-sync", false).unwrap(),
        b"must survive replay"
    );
    assert_eq!(reopened.metadata_snapshot().txid, committed_txid);
}
