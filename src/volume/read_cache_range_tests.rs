use super::*;

#[test]
fn four_plus_four_borg_style_repeated_substripe_reads_use_checked_cached_ranges() {
    let dir = tempfile::tempdir().unwrap();
    let fs = ArgosFs::create(
        dir.path(),
        VolumeConfig {
            k: 4,
            m: 4,
            chunk_size: 256 * 1024,
            compression: Compression::None,
            ..VolumeConfig::default()
        },
        9,
        false,
    )
    .unwrap();
    let ino = fs.create_file_path("/backup", 0o600).unwrap();
    let payload = (0..1024 * 1024)
        .map(|i| ((i * 17 + i / 119) % 251) as u8)
        .collect::<Vec<_>>();
    fs.write_inode_range(ino, 0, &payload).unwrap();
    assert_eq!(
        fs.read_inode(ino, 0, payload.len(), false).unwrap(),
        payload
    );
    let before = fs.cache.stats();
    assert!(before["memory_bytes"].as_u64().unwrap() >= payload.len() as u64);
    for i in 0..8 {
        let start = i * 128 * 1024;
        let part = fs.read_inode(ino, start as u64, 128 * 1024, true).unwrap();
        assert_eq!(part, payload[start..start + 128 * 1024]);
    }
    let after = fs.cache.stats();
    assert!(after["hits"].as_u64().unwrap() >= before["hits"].as_u64().unwrap() + 8);
    assert_eq!(after["memory_bytes"], before["memory_bytes"]);
    assert_eq!(fs.read_file("/backup", false).unwrap(), payload);
}

#[test]
fn physical_parity_scrub_bypasses_range_cache_after_populating_it() {
    let dir = tempfile::tempdir().unwrap();
    let fs = ArgosFs::create(
        dir.path(),
        VolumeConfig {
            k: 2,
            m: 1,
            chunk_size: 128 * 1024,
            compression: Compression::None,
            ..VolumeConfig::default()
        },
        4,
        false,
    )
    .unwrap();
    let payload = vec![0x4cu8; 256 * 1024];
    fs.write_file("/backup", &payload, 0o600).unwrap();
    let ino = fs.resolve_path("/backup", true).unwrap();
    assert_eq!(
        fs.read_inode(ino, 0, payload.len(), false).unwrap(),
        payload
    );
    for i in 0..16 {
        assert_eq!(
            fs.read_inode(ino, (i * 16 * 1024) as u64, 16 * 1024, false)
                .unwrap(),
            &payload[i * 16 * 1024..(i + 1) * 16 * 1024]
        );
    }
    let meta = fs.metadata_snapshot();
    let parity = meta.inodes[&ino].blocks[0]
        .shards
        .iter()
        .find(|shard| shard.slot == 2)
        .unwrap();
    let path = fs.shard_path_locked(&meta, &parity.disk_id, &parity.relpath);
    let mut physical = std::fs::read(&path).unwrap();
    physical[0] ^= 0x70;
    std::fs::write(&path, physical).unwrap();
    let (report, _) = fs.scrub_limited(1, None);
    assert_eq!(report.unrecoverable_files, 0, "{report:?}");
    assert_eq!(report.damaged_files, 1, "{report:?}");
    assert_eq!(report.repaired_files, 1, "{report:?}");
    assert_eq!(fs.read_file("/backup", false).unwrap(), payload);
}

#[test]
fn single_shard_verified_range_does_not_trust_a_prior_whole_block_cache_hit() {
    let dir = tempfile::tempdir().unwrap();
    let fs = ArgosFs::create(
        dir.path(),
        VolumeConfig {
            k: 1,
            m: 0,
            chunk_size: 64 * 1024,
            compression: Compression::None,
            ..VolumeConfig::default()
        },
        1,
        false,
    )
    .unwrap();
    let payload = vec![0x59u8; 64 * 1024];
    fs.write_file("/single", &payload, 0o600).unwrap();
    let ino = fs.resolve_path("/single", true).unwrap();
    let meta = fs.metadata_snapshot();
    let block = meta.inodes[&ino].blocks[0].clone();
    // Full-block decode can populate L1. Partial direct reads are specified
    // to verify the physical shard's subblock checksums, even afterwards.
    {
        let mut live = fs.meta.write();
        assert_eq!(
            fs.decode_block_locked(&mut live, &block, None, &mut Vec::new())
                .unwrap(),
            payload
        );
    }
    assert_eq!(fs.read_inode(ino, 0, 8192, false).unwrap(), payload[..8192]);
    let shard = &block.shards[0];
    let path = fs.shard_path_locked(&meta, &shard.disk_id, &shard.relpath);
    let mut raw = std::fs::read(&path).unwrap();
    raw[0] ^= 0xff;
    std::fs::write(&path, raw).unwrap();
    assert!(
        matches!(
            fs.read_inode(ino, 0, 8192, false),
            Err(ArgosError::UnrecoverableStripe { .. })
        ),
        "a cached prior full decode must not conceal damaged physical 1+0 data"
    );
}
