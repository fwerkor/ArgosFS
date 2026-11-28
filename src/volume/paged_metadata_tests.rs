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
        let fs = ArgosFs::create_loop(&paths, config(deferred), 64 * 1024 * 1024, "paged", false)
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
    let fs = ArgosFs::create_loop_legacy(
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

#[test]
fn paged_topology_add_remove_and_reopen_with_retired_members() {
    topology_roundtrip(false);
}

#[test]
fn v3_experimental_topology_add_remove_and_reopen_with_retired_members() {
    topology_roundtrip(true);
}

fn topology_roundtrip(v3: bool) {
    for deferred in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let mut paths = images(&dir);
        let fs = if v3 {
            ArgosFs::create_loop_v3_experimental(
                &paths,
                config(deferred),
                64 * 1024 * 1024,
                "topology",
            )
            .unwrap()
        } else {
            ArgosFs::create_loop_paged(
                &paths,
                config(deferred),
                64 * 1024 * 1024,
                "topology",
                false,
            )
            .unwrap()
        };
        let bytes = vec![61; 16384];
        fs.write_file("/before", &bytes, 0o600).unwrap();
        fs.sync().unwrap();
        let new = dir.path().join("added.img");
        let id = fs
            .add_block_device(new.clone(), 64 * 1024 * 1024, false)
            .unwrap();
        assert_eq!(id, "disk-0003");
        paths.push(new);
        fs.write_file("/after-add", &bytes, 0o600).unwrap();
        fs.sync().unwrap();
        assert_eq!(fs.read_file("/before", false).unwrap(), bytes);
        fs.mark_clean_unmount().unwrap();
        drop(fs);
        let fs = ArgosFs::open_loop(&paths, true).unwrap();
        fs.remove_disk("disk-0000").unwrap();
        fs.write_file("/after-remove", &bytes, 0o600).unwrap();
        fs.sync().unwrap();
        fs.mark_clean_unmount().unwrap();
        let expected = fs.metadata_snapshot();
        drop(fs);
        for available in [&paths[..], &paths[1..]] {
            let fs = ArgosFs::open_loop(available, false).unwrap();
            assert_eq!(fs.metadata_snapshot().txid, expected.txid);
            assert_eq!(
                fs.metadata_snapshot().disks["disk-0000"].status,
                DiskStatus::Removed
            );
            assert_eq!(fs.read_file("/before", false).unwrap(), bytes);
            assert_eq!(fs.read_file("/after-add", false).unwrap(), bytes);
            assert_eq!(fs.read_file("/after-remove", false).unwrap(), bytes);
        }
        // A formerly valid old quorum cannot mount the obsolete namespace.
        assert!(ArgosFs::open_loop(&paths[..2], false).is_err());
        // Exercise a third epoch, reusing the first alternate configuration slot.
        let fs = ArgosFs::open_loop(&paths[1..], true).unwrap();
        let extra = dir.path().join("extra.img");
        fs.add_block_device(extra.clone(), 64 * 1024 * 1024, false)
            .unwrap();
        fs.mkdir("/epoch-three", 0o755).unwrap();
        fs.sync().unwrap();
        fs.mark_clean_unmount().unwrap();
        drop(fs);
        paths.push(extra);
        let fs = ArgosFs::open_loop(&paths, false).unwrap();
        assert!(fs.metadata_snapshot().inodes[&1]
            .entries
            .contains_key("epoch-three"));
    }
}

#[test]
fn paged_audit_event_does_not_commit_dirty_namespace_and_survives_reopen() {
    let dir = tempfile::tempdir().unwrap();
    let paths = images(&dir);
    let fs =
        ArgosFs::create_loop_paged(&paths, config(true), 64 * 1024 * 1024, "audit", false).unwrap();
    fs.write_file("/durable", b"durable", 0o600).unwrap();
    fs.sync().unwrap();
    let durable = fs.metadata_snapshot().txid;
    fs.write_file("/pending", b"pending", 0o600).unwrap();
    {
        let mut meta = fs.meta.write();
        fs.journal_locked(&mut meta, "audit-only", json!({"reason":"test"}))
            .unwrap();
    }
    let report = fs.transaction_report().unwrap();
    assert_eq!(report.last_valid_txid, durable);
    assert!(report
        .paged_events
        .iter()
        .any(|event| event.advisory && event.action == "audit-only"));
    let opened = ArgosFs::open_loop(&paths, false).unwrap();
    assert!(opened.read_file("/pending", false).is_err());
    assert_eq!(opened.read_file("/durable", false).unwrap(), b"durable");
    drop(opened);
    fs.sync().unwrap();
    fs.mark_clean_unmount().unwrap();
    drop(fs);
    let fs = ArgosFs::open_loop(&paths, false).unwrap();
    assert_eq!(fs.read_file("/pending", false).unwrap(), b"pending");
    assert!(fs
        .transaction_report()
        .unwrap()
        .paged_events
        .iter()
        .any(|event| event.advisory && event.action == "audit-only"));
}

#[test]
fn paged_topology_replace_missing_member() {
    let dir = tempfile::tempdir().unwrap();
    let paths = images(&dir);
    let fs = ArgosFs::create_loop_paged(
        &paths,
        config(false),
        64 * 1024 * 1024,
        "replace-missing",
        false,
    )
    .unwrap();
    let data = vec![81; 32768];
    fs.write_file("/data", &data, 0o600).unwrap();
    fs.mark_clean_unmount().unwrap();
    drop(fs);
    let missing = paths[2].clone();
    let hidden = dir.path().join("offline.img");
    std::fs::rename(&missing, &hidden).unwrap();
    let fs = ArgosFs::open_loop(&paths[..2], true).unwrap();
    assert_eq!(fs.read_file("/data", false).unwrap(), data);
    let added = dir.path().join("replacement.img");
    fs.add_block_device(added.clone(), 64 * 1024 * 1024, false)
        .unwrap();
    fs.mark_clean_unmount().unwrap();
    drop(fs);
    let available = vec![paths[0].clone(), paths[1].clone(), added];
    let fs = ArgosFs::open_loop(&available, true).unwrap();
    fs.remove_disk("disk-0002").unwrap();
    fs.write_file("/after", b"after", 0o600).unwrap();
    fs.mark_clean_unmount().unwrap();
    drop(fs);
    std::fs::rename(hidden, missing).unwrap();
    let mut with_retired = available;
    with_retired.push(paths[2].clone());
    let fs = ArgosFs::open_loop(&with_retired, false).unwrap();
    assert_eq!(
        fs.metadata_snapshot().disks["disk-0002"].status,
        DiskStatus::Removed
    );
    assert_eq!(fs.read_file("/data", false).unwrap(), data);
    assert_eq!(fs.read_file("/after", false).unwrap(), b"after");
}

#[test]
fn v3_experimental_volume_namespace_data_quota_and_reopen() {
    use crate::storage::raw::paged_metadata::V3_INTEGRITY_SCHEME;
    for deferred in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let paths = images(&dir);
        let fs = ArgosFs::create_loop_v3_experimental(
            &paths,
            config(deferred),
            128 * 1024 * 1024,
            "v3-integration",
        )
        .unwrap();
        let parent = fs.mkdir("/dir", 0o755).unwrap();
        let data = (0..128 * 1024).map(|i| (i % 251) as u8).collect::<Vec<_>>();
        fs.write_file("/dir/file", &data, 0o600).unwrap();
        let ino = fs.resolve_path("/dir/file", true).unwrap();
        fs.link_at(ino, parent, std::ffi::OsStr::new("alias"))
            .unwrap();
        fs.set_directory_quota("/dir", 256 * 1024).unwrap();
        assert_eq!(
            fs.directory_quota("/dir").unwrap(),
            (Some(256 * 1024), data.len() as u64)
        );
        fs.rename_path("/dir/file", "/dir/renamed").unwrap();
        fs.sync().unwrap();
        assert_eq!(fs.read_file("/dir/alias", false).unwrap(), data);
        assert_eq!(
            fs.metadata_snapshot().integrity.hash_scheme,
            Some(V3_INTEGRITY_SCHEME)
        );
        assert_eq!(
            fs.transaction_report().unwrap().selected_metadata_source,
            "raw-v3-compatibility"
        );
        fs.mark_clean_unmount().unwrap();
        drop(fs);
        let fs = ArgosFs::open_loop(&paths, true).unwrap();
        assert_eq!(fs.read_file("/dir/renamed", false).unwrap(), data);
        assert_eq!(fs.resolve_path("/dir/alias", true).unwrap(), ino);
        fs.truncate_path("/dir/renamed", 64 * 1024).unwrap();
        fs.unlink_path("/dir/alias").unwrap();
        fs.sync().unwrap();
        assert_eq!(
            fs.directory_quota("/dir").unwrap(),
            (Some(256 * 1024), 64 * 1024)
        );
        fs.mark_clean_unmount().unwrap();
        drop(fs);
        let fs = ArgosFs::open_loop(&paths, false).unwrap();
        assert_eq!(
            fs.read_file("/dir/renamed", false).unwrap(),
            data[..64 * 1024]
        );
        assert!(fs.resolve_path("/dir/alias", true).is_err());
        assert_eq!(
            fs.metadata_snapshot().integrity.hash_scheme,
            Some(V3_INTEGRITY_SCHEME)
        );
        assert_eq!(
            fs.directory_quota("/dir").unwrap(),
            (Some(256 * 1024), 64 * 1024)
        );
    }
}

#[test]
fn v3_experimental_metadata_growth_reuses_alternate_region_on_topology_changes() {
    let dir = tempfile::tempdir().unwrap();
    let mut paths = images(&dir);
    let fs =
        ArgosFs::create_loop_v3_experimental(&paths, config(false), 512 * 1024 * 1024, "v3-grow")
            .unwrap();
    let data = vec![81; 96 * 1024];
    fs.write_file("/before", &data, 0o600).unwrap();
    fs.sync().unwrap();
    let before = fs.metadata_snapshot();
    let size = fs.raw_superblocks[0].metadata.length * 2;
    assert!(matches!(
        fs.grow_v3_metadata_arena(size * 4),
        Err(ArgosError::DiskFull { .. })
    ));
    assert_eq!(fs.metadata_snapshot().txid, before.txid);
    fs.grow_v3_metadata_arena(size).unwrap();
    let grown = fs.metadata_snapshot();
    for (id, alloc) in &grown.raw_pool.allocators {
        assert_eq!(
            before.raw_pool.allocators[id].data_end - alloc.data_end,
            2 * size
        );
    }
    assert_eq!(fs.read_file("/before", false).unwrap(), data);
    assert!(fs.grow_v3_metadata_arena(size).is_err());
    assert_eq!(fs.metadata_snapshot().txid, grown.txid);
    fs.mark_clean_unmount().unwrap();
    drop(fs);
    let fs = ArgosFs::open_loop(&paths, true).unwrap();
    let added = dir.path().join("added.img");
    fs.add_block_device(added.clone(), 512 * 1024 * 1024, false)
        .unwrap();
    paths.push(added);
    for (id, alloc) in &grown.raw_pool.allocators {
        assert_eq!(
            fs.metadata_snapshot().raw_pool.allocators[id].data_end,
            alloc.data_end
        );
    }
    fs.write_file("/after", &data, 0o600).unwrap();
    fs.mark_clean_unmount().unwrap();
    drop(fs);
    let fs = ArgosFs::open_loop(&paths, true).unwrap();
    fs.remove_disk("disk-0000").unwrap();
    for (id, alloc) in &grown.raw_pool.allocators {
        if id != "disk-0000" {
            assert_eq!(
                fs.metadata_snapshot().raw_pool.allocators[id].data_end,
                alloc.data_end
            );
        }
    }
    fs.mark_clean_unmount().unwrap();
    drop(fs);
    let fs = ArgosFs::open_loop(&paths[1..], false).unwrap();
    assert_eq!(fs.read_file("/before", false).unwrap(), data);
    assert_eq!(fs.read_file("/after", false).unwrap(), data);
}
