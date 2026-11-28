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
    let fs = ArgosFs::create_loop_legacy(
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
fn raw_sync_after_read_retains_durable_hash_until_a_real_write() {
    let tmp = TempDir::new().unwrap();
    let image = tmp.path().join("disk-sync.img");
    let images = vec![image];
    let fs = ArgosFs::create_loop_legacy(
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
    let durable = fs.metadata_snapshot();
    assert_eq!(fs.read_file("/seed", false).unwrap(), seed_payload);
    fs.sync().unwrap();

    // Read telemetry is ephemeral and must not turn sync into a checkpoint.
    // Preserve the last journal-durable hash until an actual mutation commits.
    let synced = fs.metadata_snapshot();
    assert_eq!(synced.txid, durable.txid);
    assert_eq!(synced.integrity.meta_hash, durable.integrity.meta_hash);
    assert_ne!(
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

#[test]
fn legacy_rebalance_replays_tier_classification_with_rewritten_blocks() {
    let tmp = TempDir::new().unwrap();
    let images: Vec<_> = (0..3)
        .map(|i| tmp.path().join(format!("disk-{i}.img")))
        .collect();
    let mut cfg = config();
    cfg.k = 2;
    cfg.m = 1;
    let fs = ArgosFs::create_loop_legacy(
        &images,
        cfg,
        32 * 1024 * 1024,
        "rebalance-base-regression",
        false,
    )
    .unwrap();
    let payload = vec![0x5a; 8192];
    fs.write_file("/seed", &payload, 0o644).unwrap();
    for _ in 0..16 {
        assert_eq!(fs.read_file("/seed", false).unwrap(), payload);
    }
    // Persist the read counters without classifying the file yet.
    fs.mkdir("/commit-advisory", 0o755).unwrap();
    fs.rebalance().unwrap();
    fs.sync().unwrap();
    let committed = fs.metadata_snapshot();
    std::mem::forget(fs);
    let recovered = ArgosFs::open_loop(&images, false).unwrap();
    assert_eq!(recovered.metadata_snapshot().txid, committed.txid);
    assert_eq!(recovered.read_file("/seed", false).unwrap(), payload);
}
