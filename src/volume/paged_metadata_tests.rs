use super::*;

fn config(deferred: bool) -> VolumeConfig {
    VolumeConfig {
        k: 2,
        m: 1,
        chunk_size: 4096,
        compression: Compression::None,
        defer_metadata_commit: deferred,
        defer_data_flush: deferred,
        deferred_commit_max_transactions: 100,
        deferred_commit_interval_ms: 60_000,
        ..Default::default()
    }
}

fn images(dir: &tempfile::TempDir) -> Vec<PathBuf> {
    (0..3)
        .map(|i| dir.path().join(format!("member-{i}.img")))
        .collect()
}

#[test]
fn paged_pool_immediate_and_group_commit_roundtrip() {
    for deferred in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let paths = images(&dir);
        let fs =
            ArgosFs::create_loop_paged(&paths, config(deferred), 64 * 1024 * 1024, "paged", false)
                .unwrap();
        assert!(fs.paged_metadata.is_some());
        fs.mkdir("/dir", 0o755).unwrap();
        let bytes = vec![19; 32_768];
        fs.write_file("/dir/file", &bytes, 0o600).unwrap();
        fs.rename_path("/dir/file", "/renamed").unwrap();
        fs.write_file("/delete", b"deleted", 0o600).unwrap();
        fs.unlink_path("/delete").unwrap();
        fs.sync().unwrap();
        let expected = fs.metadata_snapshot();
        assert_eq!(
            expected.integrity.hash_scheme,
            Some(crate::storage::raw::paged_metadata::PAGED_INTEGRITY_SCHEME)
        );
        assert_eq!(
            fs.transaction_report().unwrap().last_valid_txid,
            expected.txid
        );
        assert_eq!(
            fs.transaction_report().unwrap().selected_metadata_source,
            "raw-paged-metadata"
        );
        assert_eq!(fs.read_file("/renamed", false).unwrap(), bytes);
        assert!(fs
            .add_block_device(dir.path().join("not-created.img"), 64 * 1024 * 1024, false)
            .is_err());
        assert!(!dir.path().join("not-created.img").exists());
        fs.mark_clean_unmount().unwrap();
        drop(fs);
        let fs = ArgosFs::open_loop(&paths, false).unwrap();
        assert_eq!(fs.metadata_snapshot().integrity, expected.integrity);
        assert_eq!(fs.read_file("/renamed", false).unwrap(), bytes);
        assert!(fs.read_file("/delete", false).is_err());
        drop(fs);
        let fs = ArgosFs::open_loop(&paths, true).unwrap();
        fs.truncate_path("/renamed", 16384).unwrap();
        fs.sync().unwrap();
        fs.mark_clean_unmount().unwrap();
        drop(fs);
        let fs = ArgosFs::open_loop(&paths, false).unwrap();
        assert_eq!(fs.read_file("/renamed", false).unwrap(), bytes[..16384]);
    }
}

#[test]
fn paged_pool_degraded_commit_and_returning_member_repair() {
    let dir = tempfile::tempdir().unwrap();
    let paths = images(&dir);
    let fs = ArgosFs::create_loop_paged(
        &paths,
        config(false),
        64 * 1024 * 1024,
        "paged-degraded",
        false,
    )
    .unwrap();
    fs.write_file("/before", &vec![7; 16384], 0o600).unwrap();
    fs.mark_clean_unmount().unwrap();
    drop(fs);
    let fs = ArgosFs::open_loop(&paths[..2], true).unwrap();
    assert_eq!(fs.read_file("/before", false).unwrap(), vec![7; 16384]);
    // Namespace-only mutation doesn't require a third data member.
    fs.mkdir("/degraded", 0o755).unwrap();
    fs.mark_clean_unmount().unwrap();
    let expected = fs.metadata_snapshot().txid;
    drop(fs);
    assert!(ArgosFs::open_loop(&paths[2..], false).is_err());
    let fs = ArgosFs::open_loop(&paths, true).unwrap();
    assert_eq!(fs.metadata_snapshot().txid, expected);
    fs.mkdir("/after-repair", 0o755).unwrap();
    fs.mark_clean_unmount().unwrap();
    drop(fs);
    let fs = ArgosFs::open_loop(&paths[1..], false).unwrap();
    assert!(fs
        .metadata_snapshot()
        .inodes
        .values()
        .any(|inode| inode.entries.contains_key("after-repair")));
}

#[test]
fn paged_pool_offline_upgrade_preserves_data_and_is_retryable() {
    let dir = tempfile::tempdir().unwrap();
    let paths = images(&dir);
    let fs = ArgosFs::create_loop(
        &paths,
        config(true),
        64 * 1024 * 1024,
        "legacy-to-paged",
        false,
    )
    .unwrap();
    let data = vec![37; 32768];
    fs.mkdir("/dir", 0o755).unwrap();
    fs.write_file("/dir/file", &data, 0o600).unwrap();
    fs.sync().unwrap();
    fs.mark_clean_unmount().unwrap();
    let old = fs.metadata_snapshot();
    drop(fs);
    assert!(raw_store::upgrade_paged_metadata(
        BackendKind::LoopBlock,
        &paths[..2],
        Some("legacy-to-paged"),
        None
    )
    .is_err());
    assert!(raw_store::upgrade_paged_metadata(
        BackendKind::LoopBlock,
        &paths,
        Some("wrong-pool"),
        None
    )
    .is_err());
    assert!(raw_store::upgrade_paged_metadata(
        BackendKind::LoopBlock,
        &paths,
        None,
        Some(128 * 1024 * 1024)
    )
    .is_err());
    let legacy = ArgosFs::open_loop(&paths, false).unwrap();
    assert_eq!(legacy.read_file("/dir/file", false).unwrap(), data);
    assert!(legacy.paged_metadata.is_none());
    drop(legacy);

    let converted = raw_store::upgrade_paged_metadata(
        BackendKind::LoopBlock,
        &paths,
        Some("legacy-to-paged"),
        None,
    )
    .unwrap();
    assert_eq!(converted.txid, old.txid + 1);
    for (disk, alloc) in &converted.raw_pool.allocators {
        assert!(alloc.data_end < old.raw_pool.allocators[disk].data_end);
        assert_eq!(alloc.next_offset, old.raw_pool.allocators[disk].next_offset);
    }
    let fs = ArgosFs::open_loop(&paths, true).unwrap();
    assert_eq!(fs.read_file("/dir/file", false).unwrap(), data);
    fs.rename_path("/dir/file", "/migrated").unwrap();
    fs.sync().unwrap();
    let latest = fs.metadata_snapshot();
    fs.mark_clean_unmount().unwrap();
    drop(fs);
    // Retry must recover the new authority, not the old legacy checkpoint.
    let repeated =
        raw_store::upgrade_paged_metadata(BackendKind::LoopBlock, &paths, None, None).unwrap();
    assert_eq!(repeated.integrity, latest.integrity);
    let fs = ArgosFs::open_loop(&paths, false).unwrap();
    assert_eq!(fs.read_file("/migrated", false).unwrap(), data);
    assert!(fs.read_file("/dir/file", false).is_err());
}
