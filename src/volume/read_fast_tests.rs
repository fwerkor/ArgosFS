use super::*;
use tempfile::tempdir;

fn redundant_volume() -> (tempfile::TempDir, ArgosFs, Vec<u8>) {
    let dir = tempdir().unwrap();
    let fs = ArgosFs::create(
        dir.path(),
        VolumeConfig {
            k: 2,
            m: 1,
            compression: Compression::None,
            chunk_size: 4096,
            ..VolumeConfig::default()
        },
        4,
        false,
    )
    .unwrap();
    let data = (0..8_192)
        .map(|i| ((i * 31 + i / 7) % 251) as u8)
        .collect::<Vec<_>>();
    fs.write_file("/file", &data, 0o600).unwrap();
    (dir, fs, data)
}

fn shard_path(fs: &ArgosFs, slot: usize) -> (String, std::path::PathBuf) {
    let meta = fs.metadata_snapshot();
    let ino = fs.resolve_path("/file", true).unwrap();
    let shard = meta.inodes[&ino].blocks[0]
        .shards
        .iter()
        .find(|s| s.slot == slot)
        .unwrap();
    (
        shard.disk_id.clone(),
        fs.shard_path_locked(&meta, &shard.disk_id, &shard.relpath),
    )
}

#[test]
fn normal_read_skips_parity_while_scrub_verifies_and_repairs_it() {
    let (_dir, fs, bytes) = redundant_volume();
    let (disk_id, parity_path) = shard_path(&fs, 2);
    let mut damaged = std::fs::read(&parity_path).unwrap();
    damaged[0] ^= 0xff;
    std::fs::write(&parity_path, &damaged).unwrap();
    let before = fs.metadata_snapshot().disks[&disk_id].io_samples;
    assert_eq!(fs.read_file("/file", true).unwrap(), bytes);
    // A normal verified read never touches healthy-unneeded parity.
    let after = fs.metadata_snapshot().disks[&disk_id].io_samples;
    assert_eq!(before, after);
    assert_eq!(std::fs::read(&parity_path).unwrap(), damaged);

    // Physical scrub bypasses reconstructed cache, detects the parity error,
    // and repairs it using the verified data shards.
    let (report, _) = fs.scrub_limited(1, None);
    assert_eq!(report.unrecoverable_files, 0, "{report:?}");
    assert_eq!(report.damaged_files, 1, "{report:?}");
    assert_eq!(report.repaired_files, 1, "{report:?}");
    assert_eq!(fs.read_file("/file", false).unwrap(), bytes);
    let (after_report, _) = fs.scrub_limited(1, None);
    assert_eq!(after_report.damaged_files, 0, "{after_report:?}");
}

#[test]
fn data_shard_damage_fetches_parity_and_recovers_even_without_repair() {
    let (_dir, fs, bytes) = redundant_volume();
    let (disk_id, data_path) = shard_path(&fs, 0);
    let (parity_disk, _) = shard_path(&fs, 2);
    std::fs::remove_file(data_path).unwrap();
    let before = fs.metadata_snapshot().disks[&parity_disk].io_samples;
    // Even an ordinary non-repairing read must recover via parity.
    assert_eq!(fs.read_file("/file", false).unwrap(), bytes);
    let after = fs.metadata_snapshot().disks[&parity_disk].io_samples;
    assert!(
        after > before,
        "missing data shard did not trigger parity I/O"
    );
    // The missing shard was not silently classified as healthy.
    let report = fs.fsck(false, false).unwrap();
    assert!(report.missing_shards > 0, "{report:?}");
    assert_ne!(disk_id, parity_disk);
}

#[test]
fn missing_data_and_corrupt_parity_fail_closed() {
    let (_dir, fs, _bytes) = redundant_volume();
    let (_, data_path) = shard_path(&fs, 0);
    let (_, parity_path) = shard_path(&fs, 2);
    std::fs::remove_file(data_path).unwrap();
    let mut parity = std::fs::read(&parity_path).unwrap();
    parity[0] ^= 0x80;
    std::fs::write(parity_path, parity).unwrap();
    assert!(matches!(
        fs.read_file("/file", false),
        Err(ArgosError::UnrecoverableStripe { .. })
    ));
}
