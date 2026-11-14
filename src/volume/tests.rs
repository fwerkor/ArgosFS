use super::*;

#[test]
fn host_sync_drains_only_dirty_shard_set() {
    let tmp = tempfile::tempdir().unwrap();
    let fs = ArgosFs::create(
        tmp.path(),
        VolumeConfig {
            k: 1,
            m: 0,
            compression: Compression::None,
            ..VolumeConfig::default()
        },
        1,
        false,
    )
    .unwrap();

    fs.write_file("/dirty", b"dirty shard tracking", 0o644)
        .unwrap();
    assert!(!fs.dirty_host_shards.lock().is_empty());

    fs.sync().unwrap();

    assert!(fs.dirty_host_shards.lock().is_empty());
}

#[test]
fn data_operations_register_per_inode_locks() {
    let tmp = tempfile::tempdir().unwrap();
    let fs = ArgosFs::create(
        tmp.path(),
        VolumeConfig {
            k: 1,
            m: 0,
            compression: Compression::None,
            ..VolumeConfig::default()
        },
        1,
        false,
    )
    .unwrap();

    fs.write_file("/locked", b"abc", 0o644).unwrap();
    let ino = fs.resolve_path("/locked", true).unwrap();
    fs.write_inode_range(ino, 3, b"def").unwrap();
    fs.truncate_inode(ino, 4).unwrap();

    assert!(fs.inode_locks.lock().contains_key(&ino));
}

fn host_volume() -> (tempfile::TempDir, ArgosFs) {
    let dir = tempfile::tempdir().unwrap();
    let fs = ArgosFs::create(
        dir.path(),
        VolumeConfig {
            k: 1,
            m: 0,
            compression: Compression::None,
            chunk_size: 4096,
            ..VolumeConfig::default()
        },
        1,
        false,
    )
    .unwrap();
    (dir, fs)
}

#[test]
fn bulk_import_guards_restore_nested_thread_state() {
    assert!(!bulk_import_enabled());
    {
        let _outer = bulk_import_scope(true);
        assert!(bulk_import_enabled());
        {
            let _inner = bulk_import_scope(false);
            assert!(!bulk_import_enabled());
        }
        assert!(bulk_import_enabled());
    }
    assert!(!bulk_import_enabled());
}

#[test]
fn deferred_state_and_interval_distinguish_host_writable_and_readonly_block_backends() {
    let (_dir, host) = host_volume();
    assert!(host.deferred_commit_interval().is_none());
    assert!(!host.sync_deferred_if_dirty().unwrap());
    let host_state = DeferredCommitState::new(&host.metadata_snapshot());
    assert!(host_state.durable_metadata.is_none());
    assert_eq!(host_state.dirty_transactions, 0);
    assert!(host_state.pending_reclaims.is_empty());

    let dir = tempfile::tempdir().unwrap();
    let image = dir.path().join("deferred.img");
    let config = VolumeConfig {
        k: 1,
        m: 0,
        defer_metadata_commit: true,
        deferred_commit_interval_ms: 17,
        deferred_commit_max_transactions: 100,
        ..VolumeConfig::default()
    };
    let writable = ArgosFs::create_loop(
        std::slice::from_ref(&image),
        config,
        32 * 1024 * 1024,
        "deferred-tests",
        false,
    )
    .unwrap();
    assert_eq!(
        writable.deferred_commit_interval(),
        Some(Duration::from_millis(17))
    );
    writable.write_file("/dirty", b"data", 0o600).unwrap();
    assert!(writable.sync_deferred_if_dirty().unwrap());
    assert!(!writable.sync_deferred_if_dirty().unwrap());
    writable.mark_clean_unmount().unwrap();
    drop(writable);

    let readonly = ArgosFs::open_loop(std::slice::from_ref(&image), false).unwrap();
    assert!(readonly.deferred_commit_interval().is_none());
    assert!(!readonly.sync_deferred_if_dirty().unwrap());
    let meta = readonly.meta.read();
    assert!(readonly
        .ensure_block_backend_writable_locked(&meta)
        .is_err());
    let superblocks = readonly.metadata_superblocks_locked(&meta).unwrap();
    assert!(readonly
        .metadata_block_backend_locked(&meta, &superblocks, false)
        .is_ok());
}

#[test]
fn quorum_loss_write_fence_stops_mutations_and_background_retries() {
    let dir = tempfile::tempdir().unwrap();
    let image = dir.path().join("write-fence.img");
    let fs = ArgosFs::create_loop(
        std::slice::from_ref(&image),
        VolumeConfig {
            k: 1,
            m: 0,
            defer_metadata_commit: true,
            ..VolumeConfig::default()
        },
        32 * 1024 * 1024,
        "write-fence",
        false,
    )
    .unwrap();
    *fs.write_fence.lock() = Some("metadata quorum unavailable".to_string());

    assert!(matches!(
        fs.write_file("/rejected", b"data", 0o600),
        Err(ArgosError::ReadonlyRequired(_))
    ));
    assert!(!fs.sync_deferred_if_dirty().unwrap());
}

#[test]
fn snapshots_validate_names_sanitize_paths_and_reject_duplicates() {
    let (_dir, fs) = host_volume();
    fs.write_file("/file", b"data", 0o600).unwrap();
    for invalid in ["", "   ", ".", ".."] {
        assert!(fs.snapshot(invalid).is_err());
    }
    let snapshot = fs.snapshot(" release / candidate ").unwrap();
    assert_eq!(snapshot.file_name().unwrap(), "release___candidate.json");
    assert!(snapshot.exists());
    assert!(fs.snapshot(" release / candidate ").is_err());
    let persisted: Metadata = serde_json::from_slice(&std::fs::read(snapshot).unwrap()).unwrap();
    assert_eq!(persisted.uuid, fs.metadata_snapshot().uuid);
    let report = ArgosFs::audit_transactions(fs.root()).unwrap();
    assert!(report.valid_entries > 0);
}

#[test]
fn path_iteration_and_file_windows_cover_nested_non_utf8_and_cursor_wrap() {
    use std::os::unix::ffi::OsStringExt;
    let (_dir, fs) = host_volume();
    let dir = fs.mkdir("/dir", 0o755).unwrap();
    let raw_name = std::ffi::OsString::from_vec(b"raw-\xff".to_vec());
    fs.create_file_at(dir, &raw_name, 0o600).unwrap();
    fs.write_file("/a", b"a", 0o600).unwrap();
    fs.write_file("/b", b"b", 0o600).unwrap();
    fs.write_file("/c", b"c", 0o600).unwrap();

    let byte_paths = fs.iter_path_bytes();
    assert!(byte_paths.iter().any(|(path, _)| path == b"/"));
    assert!(byte_paths.iter().any(|(path, _)| path == b"/dir/raw-\xff"));
    assert!(fs
        .iter_paths()
        .iter()
        .any(|(path, _)| path.contains("raw-")));

    assert!(fs.file_window(None, 0).is_empty());
    let all = fs.file_window(None, usize::MAX);
    assert_eq!(all.len(), 4);
    assert!(all.windows(2).all(|pair| pair[0].0 < pair[1].0));
    let first = fs.file_window(None, 2);
    assert_eq!(first.len(), 2);
    assert_eq!(first[0].0, all[0].0);
    let after = fs.file_window(Some(first[1].0), 2);
    assert_eq!(after[0].0, all[2].0);
    let wrapped = fs.file_window(Some(u64::MAX), 2);
    assert_eq!(
        wrapped.iter().map(|(ino, _)| *ino).collect::<Vec<_>>(),
        first.iter().map(|(ino, _)| *ino).collect::<Vec<_>>()
    );
}

#[test]
fn symlink_resolution_detects_loops_and_non_utf8_targets() {
    use std::os::unix::ffi::OsStringExt;
    let (_dir, fs) = host_volume();
    fs.symlink_path("/b", "/a").unwrap();
    fs.symlink_path("/a", "/b").unwrap();
    assert!(matches!(
        fs.resolve_path("/a", true),
        Err(ArgosError::Invalid(_))
    ));
    assert!(fs.resolve_path("/a", false).is_ok());

    let raw_target = std::ffi::OsString::from_vec(b"target-\xff".to_vec());
    fs.symlink_at(ROOT_INO, OsStr::new("raw-link"), Path::new(&raw_target))
        .unwrap();
    assert_eq!(
        fs.readlink_inode_bytes(fs.resolve_path("/raw-link", false).unwrap())
            .unwrap(),
        b"target-\xff"
    );
    assert!(fs.resolve_path("/raw-link", true).is_err());
}

#[test]
fn directory_helpers_find_parents_descendants_and_sticky_permissions() {
    let (_dir, fs) = host_volume();
    let parent = fs
        .mkdir_at_with_owner(ROOT_INO, OsStr::new("sticky"), 0o1777, 1000, 1000)
        .unwrap();
    let child = fs
        .create_file_at_with_owner(parent.ino, OsStr::new("child"), 0o600, 2000, 2000)
        .unwrap();
    let grandchild = fs
        .mkdir_at(parent.ino, OsStr::new("nested"), 0o755)
        .unwrap();
    let meta = fs.meta.read();
    assert_eq!(fs.parent_inode_locked(&meta, ROOT_INO).unwrap(), ROOT_INO);
    assert_eq!(
        fs.parent_inode_locked(&meta, child.ino).unwrap(),
        parent.ino
    );
    assert!(fs.parent_inode_locked(&meta, 9999).is_err());
    assert!(ArgosFs::directory_contains_inode(
        &meta, ROOT_INO, child.ino
    ));
    assert!(ArgosFs::directory_contains_inode(
        &meta,
        parent.ino,
        grandchild.ino
    ));
    assert!(!ArgosFs::directory_contains_inode(
        &meta, child.ino, parent.ino
    ));
    assert!(!ArgosFs::directory_contains_inode(&meta, 9999, child.ino));
    fs.check_sticky_locked(&meta, parent.ino, child.ino, None)
        .unwrap();
    fs.check_sticky_locked(&meta, parent.ino, child.ino, Some(0))
        .unwrap();
    fs.check_sticky_locked(&meta, parent.ino, child.ino, Some(1000))
        .unwrap();
    fs.check_sticky_locked(&meta, parent.ino, child.ino, Some(2000))
        .unwrap();
    assert!(fs
        .check_sticky_locked(&meta, parent.ino, child.ino, Some(3000))
        .is_err());
}

#[test]
fn inode_helpers_touch_allocate_paths_and_attributes() {
    let (_dir, fs) = host_volume();
    let mut meta = fs.meta.write();
    let next = meta.next_inode;
    assert_eq!(fs.alloc_inode_locked(&mut meta).unwrap(), next);
    assert_eq!(meta.next_inode, next + 1);
    let before = meta.inodes[&ROOT_INO].clone();
    fs.touch_inode_locked(&mut meta, ROOT_INO, true, false);
    assert!(meta.inodes[&ROOT_INO].mtime >= before.mtime);
    assert_eq!(meta.inodes[&ROOT_INO].ctime, before.ctime);
    fs.touch_inode_locked(&mut meta, ROOT_INO, false, true);
    assert!(meta.inodes[&ROOT_INO].ctime >= before.ctime);
    fs.touch_inode_locked(&mut meta, 9999, true, true);

    let disk_id = meta.disks.keys().next().unwrap().clone();
    let relpath = Path::new("shards/test.blk");
    let path = fs.shard_path_locked(&meta, &disk_id, relpath);
    assert!(path.ends_with(relpath));
    assert_eq!(
        fs.shard_path_if_disk_exists_locked(&meta, &disk_id, relpath),
        Some(path)
    );
    assert!(fs
        .shard_path_if_disk_exists_locked(&meta, "missing", relpath)
        .is_none());

    let inode = meta.inodes[&ROOT_INO].clone();
    let attr = ArgosFs::attr_from_inode(&inode, 4096);
    assert_eq!(attr.ino, ROOT_INO);
    assert_eq!(attr.blocks, 0);
    let mut file = inode;
    file.kind = NodeKind::File;
    file.size = 513;
    let attr = ArgosFs::attr_from_inode(&file, 4096);
    assert_eq!(attr.blocks, 2);
    assert_eq!(attr.blksize, 4096);
}

#[test]
fn transaction_error_classification_covers_committed_and_uncommitted_points() {
    for point in [
        "after-journal",
        "after-primary-metadata",
        "after-secondary-metadata",
        "after-compatible-metadata",
        "after-journal-commit-before-metadata-commit",
        "after-metadata-commit-before-superblock-update",
    ] {
        assert!(ArgosFs::transaction_error_is_committed(
            &ArgosError::InjectedCrash(point.to_string())
        ));
    }
    assert!(ArgosFs::transaction_error_is_committed(
        &ArgosError::CommittedDurabilityLoss("data quorum lost".to_string())
    ));
    assert!(ArgosFs::transaction_error_is_committed(
        &ArgosError::IndeterminateCommit("flush outcome unknown".to_string())
    ));
    for error in [
        ArgosError::InjectedCrash("before-journal".to_string()),
        ArgosError::Invalid("x".to_string()),
        ArgosError::Conflict("x".to_string()),
        ArgosError::RetryableQuorumUnavailable {
            operation: "metadata".to_string(),
            need: 2,
            have: 1,
        },
    ] {
        assert!(!ArgosFs::transaction_error_is_committed(&error));
    }
}

#[test]
fn retryable_quorum_shortfall_does_not_write_fence_mount() {
    let tmp = tempfile::tempdir().unwrap();
    let fs = ArgosFs::create(
        tmp.path(),
        VolumeConfig {
            k: 1,
            m: 0,
            ..VolumeConfig::default()
        },
        1,
        false,
    )
    .unwrap();
    let retryable = ArgosError::RetryableQuorumUnavailable {
        operation: "metadata".to_string(),
        need: 2,
        have: 1,
    };
    fs.fence_on_quorum_loss(&retryable);
    assert!(fs.write_fence.lock().is_none());

    fs.fence_on_quorum_loss(&ArgosError::IndeterminateCommit(
        "outcome unknown".to_string(),
    ));
    assert!(fs.write_fence.lock().is_some());
}

#[test]
fn raw_journal_events_refresh_stale_read_telemetry_integrity() {
    let dir = tempfile::tempdir().unwrap();
    let image = dir.path().join("event-integrity.img");
    let fs = ArgosFs::create_loop(
        std::slice::from_ref(&image),
        VolumeConfig {
            k: 1,
            m: 0,
            compression: Compression::None,
            defer_journal_flush: true,
            defer_metadata_commit: true,
            defer_data_flush: true,
            ..VolumeConfig::default()
        },
        32 * 1024 * 1024,
        "event-integrity",
        false,
    )
    .unwrap();
    fs.write_file("/value", b"telemetry", 0o600).unwrap();
    fs.sync().unwrap();

    let durable = fs.metadata_snapshot();
    assert_eq!(
        journal::canonical_metadata_hash(&durable).unwrap(),
        durable.integrity.meta_hash
    );
    assert_eq!(fs.read_file("/value", true).unwrap(), b"telemetry");
    let dirty = fs.metadata_snapshot();
    assert_ne!(
        journal::canonical_metadata_hash(&dirty).unwrap(),
        dirty.integrity.meta_hash
    );
    assert!(fs.deferred_commit.lock().raw_uncommitted_metadata_dirty);

    {
        let meta = fs.meta.read();
        fs.journal_locked(&meta, "self-heal-deferred", json!({"reason": "test-event"}))
            .unwrap();
    }

    let report = fs.transaction_report().unwrap();
    assert_eq!(report.invalid_entries, 0);
    assert_eq!(report.raw_journal_quorum, Some(true));
    assert!(fs.deferred_commit.lock().raw_uncommitted_metadata_dirty);
}

#[test]
fn quorum_report_fences_when_failed_member_owns_required_data() {
    let dir = tempfile::tempdir().unwrap();
    let images = (0..3)
        .map(|index| dir.path().join(format!("fanout-{index}.img")))
        .collect::<Vec<_>>();
    let fs = ArgosFs::create_loop(
        &images,
        VolumeConfig {
            k: 1,
            m: 0,
            compression: Compression::None,
            chunk_size: 4096,
            ..VolumeConfig::default()
        },
        32 * 1024 * 1024,
        "fanout-durability",
        false,
    )
    .unwrap();
    fs.write_file("/payload", &vec![7u8; 64 * 1024], 0o600)
        .unwrap();
    fs.sync().unwrap();

    let meta = fs.metadata_snapshot();
    let failed_disk = meta
        .inodes
        .values()
        .find(|inode| inode.kind == NodeKind::File)
        .and_then(|inode| inode.blocks.first())
        .and_then(|block| block.shards.first())
        .map(|shard| shard.disk_id.clone())
        .unwrap();
    let mut report = raw_store::QuorumWriteReport::default();
    report
        .failed_devices
        .insert(failed_disk.clone(), "simulated metadata EIO".to_string());
    report.unavailable_devices.insert(failed_disk.clone());

    let err = fs.apply_quorum_write_report(&meta, report).unwrap_err();
    assert!(matches!(err, ArgosError::CommittedDurabilityLoss(_)));
    assert!(fs.is_device_quarantined(&failed_disk));
    assert!(fs.write_fence.lock().is_some());
}

#[test]
fn durability_validation_counts_offline_and_quarantined_shards_together() {
    let dir = tempfile::tempdir().unwrap();
    let images = (0..3)
        .map(|index| dir.path().join(format!("admin-unavailable-{index}.img")))
        .collect::<Vec<_>>();
    let fs = ArgosFs::create_loop(
        &images,
        VolumeConfig {
            k: 2,
            m: 1,
            compression: Compression::None,
            chunk_size: 4096,
            ..VolumeConfig::default()
        },
        32 * 1024 * 1024,
        "admin-unavailable",
        false,
    )
    .unwrap();
    fs.write_file("/payload", &vec![5u8; 64 * 1024], 0o600)
        .unwrap();
    fs.sync().unwrap();

    let meta = fs.metadata_snapshot();
    let block = meta
        .inodes
        .values()
        .find(|inode| inode.kind == NodeKind::File)
        .and_then(|inode| inode.blocks.first())
        .unwrap();
    let offline = block.shards[0].disk_id.clone();
    let quarantined = block.shards[1].disk_id.clone();

    let mut degraded = meta.clone();
    degraded.disks.get_mut(&offline).unwrap().status = DiskStatus::Offline;
    let unavailable = BTreeSet::from([quarantined]);
    assert!(matches!(
        fs.validate_data_durability_locked(&degraded, &unavailable),
        Err(ArgosError::QuorumUnavailable { .. })
    ));
}

#[test]
fn immediate_precommit_durability_rejection_rolls_back_namespace_mutation() {
    let dir = tempfile::tempdir().unwrap();
    let images = (0..3)
        .map(|index| dir.path().join(format!("rollback-{index}.img")))
        .collect::<Vec<_>>();
    let fs = ArgosFs::create_loop(
        &images,
        VolumeConfig {
            k: 2,
            m: 1,
            compression: Compression::None,
            chunk_size: 4096,
            defer_metadata_commit: false,
            defer_journal_flush: false,
            defer_data_flush: false,
            ..VolumeConfig::default()
        },
        32 * 1024 * 1024,
        "rollback-precommit",
        false,
    )
    .unwrap();
    fs.write_file("/payload", &vec![9u8; 64 * 1024], 0o600)
        .unwrap();
    fs.sync().unwrap();

    let snapshot = fs.metadata_snapshot();
    let block = snapshot
        .inodes
        .values()
        .find(|inode| inode.kind == NodeKind::File)
        .and_then(|inode| inode.blocks.first())
        .unwrap();
    let offline = block.shards[0].disk_id.clone();
    let quarantined = block.shards[1].disk_id.clone();
    fs.mark_disk(&offline, DiskStatus::Offline).unwrap();
    fs.quarantine_device(&quarantined);

    assert!(matches!(
        fs.mkdir("/rejected", 0o755),
        Err(ArgosError::QuorumUnavailable { .. })
    ));
    assert!(matches!(
        fs.resolve_path("/rejected", true),
        Err(ArgosError::NotFound(_))
    ));
    assert!(fs.write_fence.lock().is_some());
}

#[test]
fn failed_inspection_of_new_block_member_blocks_deferred_commit() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("base.img");
    let fs = ArgosFs::create_loop(
        std::slice::from_ref(&base),
        VolumeConfig {
            k: 1,
            m: 0,
            chunk_size: 4096,
            compression: Compression::None,
            defer_journal_flush: true,
            defer_metadata_commit: true,
            defer_data_flush: true,
            ..VolumeConfig::default()
        },
        32 * 1024 * 1024,
        "dynamic-member",
        false,
    )
    .unwrap();
    let added_path = dir.path().join("added.img");
    let disk_id = fs
        .add_block_device(added_path.clone(), 32 * 1024 * 1024, false)
        .unwrap();
    fs.sync().unwrap();
    assert!(!fs.raw_superblocks.iter().any(|sb| sb.disk_id == disk_id));

    // Force new data onto the dynamically added member, then make its path
    // disappear before the bounded group commit tries to inspect/flush it.
    fs.mark_disk("disk-0000", DiskStatus::Offline).unwrap();
    fs.sync().unwrap();
    fs.write_file("/new-data", &vec![3u8; 64 * 1024], 0o600)
        .unwrap();
    std::fs::remove_file(&added_path).unwrap();

    let err = fs.sync().unwrap_err();
    assert!(matches!(err, ArgosError::QuorumUnavailable { .. }));
    assert!(fs.is_device_quarantined(&disk_id));
    assert!(fs.write_fence.lock().is_some());
}

#[test]
fn dynamically_inspected_member_rejects_repointed_duplicate_identity() {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().join("identity-base.img");
    let fs = ArgosFs::create_loop(
        std::slice::from_ref(&base),
        VolumeConfig {
            k: 1,
            m: 0,
            chunk_size: 4096,
            compression: Compression::None,
            defer_journal_flush: true,
            defer_metadata_commit: true,
            defer_data_flush: true,
            ..VolumeConfig::default()
        },
        32 * 1024 * 1024,
        "dynamic-identity",
        false,
    )
    .unwrap();
    let added_path = dir.path().join("identity-added.img");
    let disk_id = fs
        .add_block_device(added_path.clone(), 32 * 1024 * 1024, false)
        .unwrap();
    fs.sync().unwrap();
    assert!(!fs.raw_superblocks.iter().any(|sb| sb.disk_id == disk_id));

    // Replace the path with another readable ArgosFS member from this pool.
    // Inspection must not accept disk-0000 as a second copy of disk-0001.
    std::fs::remove_file(&added_path).unwrap();
    std::fs::copy(&base, &added_path).unwrap();

    let meta = fs.meta.read();
    let superblocks = fs.metadata_superblocks_locked(&meta).unwrap();
    assert!(!superblocks.iter().any(|sb| sb.disk_id == disk_id));
    assert_eq!(
        superblocks
            .iter()
            .filter(|sb| sb.disk_id == "disk-0000")
            .count(),
        1
    );
    assert!(fs.is_device_quarantined(&disk_id));
}

#[test]
fn metadata_fanout_excludes_raw_superblock_absent_from_membership() {
    let dir = tempfile::tempdir().unwrap();
    let images = [
        dir.path().join("member-a.img"),
        dir.path().join("member-b.img"),
    ];
    let fs = ArgosFs::create_loop(
        &images,
        VolumeConfig {
            k: 1,
            m: 1,
            ..VolumeConfig::default()
        },
        32 * 1024 * 1024,
        "orphan-superblock",
        false,
    )
    .unwrap();
    let mut meta = fs.metadata_snapshot();
    meta.disks.remove("disk-0001");

    let superblocks = fs.metadata_superblocks_locked(&meta).unwrap();
    assert_eq!(superblocks.len(), 1);
    assert_eq!(superblocks[0].disk_id, "disk-0000");
}

#[test]
fn metadata_backend_helpers_cover_host_empty_and_removed_member_filtering() {
    let (_dir, host) = host_volume();
    let host_meta = host.meta.read();
    assert!(host
        .metadata_superblocks_locked(&host_meta)
        .unwrap()
        .is_empty());
    assert!(host
        .metadata_block_backend_locked(&host_meta, &[], false)
        .is_err());
    assert!(host.open_backend_covers_superblocks(&[]));
    drop(host_meta);

    let dir = tempfile::tempdir().unwrap();
    let images = [dir.path().join("a.img"), dir.path().join("b.img")];
    let loop_fs = ArgosFs::create_loop(
        &images,
        VolumeConfig {
            k: 1,
            m: 1,
            ..VolumeConfig::default()
        },
        32 * 1024 * 1024,
        "active-backend",
        false,
    )
    .unwrap();
    let mut meta = loop_fs.meta.write();
    assert!(loop_fs.open_backend_covers_superblocks(&loop_fs.raw_superblocks));
    assert_eq!(loop_fs.metadata_superblocks_locked(&meta).unwrap().len(), 2);
    let first = meta.disks.keys().next().unwrap().clone();
    meta.disks.get_mut(&first).unwrap().status = DiskStatus::Removed;
    assert_eq!(loop_fs.metadata_superblocks_locked(&meta).unwrap().len(), 1);
    assert_eq!(
        loop_fs
            .metadata_block_backend_locked(
                &meta,
                &loop_fs.metadata_superblocks_locked(&meta).unwrap(),
                false,
            )
            .unwrap()
            .list_devices()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn dirty_host_sync_ignores_deleted_paths_and_keeps_failed_paths() {
    let (_dir, fs) = host_volume();
    let missing = fs.root.join("missing-shard");
    fs.mark_host_shard_dirty(missing);
    fs.sync_dirty_host_shards().unwrap();
    assert!(fs.dirty_host_shards.lock().is_empty());

    let unsyncable = PathBuf::from("/dev/null");
    fs.mark_host_shard_dirty(unsyncable.clone());
    assert!(fs.sync_dirty_host_shards().is_err());
    assert!(fs.dirty_host_shards.lock().contains(&unsyncable));
}

#[test]
fn volume_constructors_validate_layouts_force_recreation_and_backend_kinds() {
    let dir = tempfile::tempdir().unwrap();
    assert!(matches!(
        ArgosFs::create(
            dir.path().join("k-zero"),
            VolumeConfig {
                k: 0,
                m: 1,
                ..VolumeConfig::default()
            },
            1,
            false,
        ),
        Err(ArgosError::Invalid(_))
    ));
    assert!(matches!(
        ArgosFs::create(
            dir.path().join("too-few"),
            VolumeConfig {
                k: 2,
                m: 1,
                ..VolumeConfig::default()
            },
            2,
            false,
        ),
        Err(ArgosError::NotEnoughDisks { .. })
    ));
    let root = dir.path().join("host");
    let fs = ArgosFs::create(
        &root,
        VolumeConfig {
            k: 1,
            m: 0,
            chunk_size: 0,
            ..VolumeConfig::default()
        },
        1,
        false,
    )
    .unwrap();
    assert_eq!(
        fs.metadata_snapshot().config.chunk_size,
        VolumeConfig::default().chunk_size
    );
    drop(fs);
    assert!(matches!(
        ArgosFs::create(
            &root,
            VolumeConfig {
                k: 1,
                m: 0,
                ..VolumeConfig::default()
            },
            1,
            false
        ),
        Err(ArgosError::AlreadyExists(_))
    ));
    let recreated = ArgosFs::create(
        &root,
        VolumeConfig {
            k: 1,
            m: 0,
            ..VolumeConfig::default()
        },
        1,
        true,
    )
    .unwrap();
    let recreated_meta = recreated.metadata_snapshot();
    assert_eq!(recreated_meta.backend, BackendKind::Host);
    assert!(!recreated_meta.uuid.is_empty());

    assert!(matches!(
        ArgosFs::create_block_backend(
            BackendKind::Host,
            &[dir.path().join("unused-host-device")],
            VolumeConfig {
                k: 1,
                m: 0,
                ..VolumeConfig::default()
            },
            "host",
            false,
        ),
        Err(ArgosError::Unsupported(_))
    ));
    assert!(matches!(
        ArgosFs::create_block_backend(
            BackendKind::LoopBlock,
            &[],
            VolumeConfig {
                k: 0,
                m: 0,
                ..VolumeConfig::default()
            },
            "invalid",
            false,
        ),
        Err(ArgosError::Invalid(_))
    ));
    assert!(matches!(
        ArgosFs::create_block_backend(
            BackendKind::LoopBlock,
            &[],
            VolumeConfig {
                k: 1,
                m: 0,
                ..VolumeConfig::default()
            },
            "empty",
            false,
        ),
        Err(ArgosError::NotEnoughDisks { .. })
    ));

    let loop_image = dir.path().join("loop.img");
    let loop_fs = ArgosFs::create_loop(
        std::slice::from_ref(&loop_image),
        VolumeConfig {
            k: 1,
            m: 0,
            chunk_size: 0,
            ..VolumeConfig::default()
        },
        32 * 1024 * 1024,
        "loop",
        false,
    )
    .unwrap();
    loop_fs.mark_clean_unmount().unwrap();
    drop(loop_fs);
    assert!(ArgosFs::open_loop(std::slice::from_ref(&loop_image), false).is_ok());

    let raw_image = dir.path().join("raw.img");
    std::fs::File::create(&raw_image)
        .unwrap()
        .set_len(32 * 1024 * 1024)
        .unwrap();
    let raw_fs = ArgosFs::create_raw(
        std::slice::from_ref(&raw_image),
        VolumeConfig {
            k: 1,
            m: 0,
            ..VolumeConfig::default()
        },
        "raw",
        false,
    )
    .unwrap();
    raw_fs.mark_clean_unmount().unwrap();
    drop(raw_fs);
    assert!(ArgosFs::open_raw(std::slice::from_ref(&raw_image), false).is_ok());
}

#[test]
fn loop_pool_tracks_absence_across_reopens_and_clears_grace_on_return() {
    let dir = tempfile::tempdir().unwrap();
    let images = (0..4)
        .map(|index| dir.path().join(format!("rejoin-{index}.img")))
        .collect::<Vec<_>>();
    let fs = ArgosFs::create_loop(
        &images,
        VolumeConfig {
            k: 2,
            m: 1,
            compression: Compression::None,
            chunk_size: 4096,
            ..VolumeConfig::default()
        },
        32 * 1024 * 1024,
        "rejoin",
        false,
    )
    .unwrap();
    let data = vec![8u8; 8192];
    fs.write_file("/file", &data, 0o600).unwrap();
    let ino = fs.resolve_path("/file", true).unwrap();
    let missing_id = fs.metadata_snapshot().inodes[&ino].blocks[0].shards[0]
        .disk_id
        .clone();
    fs.mark_clean_unmount().unwrap();
    drop(fs);

    let present_images = images
        .iter()
        .enumerate()
        .filter(|(index, _)| format!("disk-{index:04}") != missing_id)
        .map(|(_, path)| path.clone())
        .collect::<Vec<_>>();
    let first = ArgosFs::open_loop(&present_images, true).unwrap();
    let absent = &first.metadata_snapshot().disks[&missing_id];
    assert_eq!(absent.status, DiskStatus::Offline);
    assert!(!absent.manually_offline);
    let offline_since = absent
        .offline_since
        .expect("offline scan records first absence");
    assert_eq!(first.read_file("/file", false).unwrap(), data);
    first.mark_clean_unmount().unwrap();
    drop(first);

    let second = ArgosFs::open_loop(&present_images, true).unwrap();
    let absent = &second.metadata_snapshot().disks[&missing_id];
    let persisted_offline_since = absent
        .offline_since
        .expect("offline grace persists across reopen");
    assert!((persisted_offline_since - offline_since).abs() < 1e-6);
    second.mark_clean_unmount().unwrap();
    drop(second);

    let restored = ArgosFs::open_loop(&images, true).unwrap();
    let returned = &restored.metadata_snapshot().disks[&missing_id];
    assert_eq!(returned.status, DiskStatus::Online);
    assert_eq!(returned.offline_since, None);
    assert_eq!(restored.read_file("/file", false).unwrap(), data);
}

#[test]
fn mknod_validates_rdev_types_inheritance_and_duplicates() {
    let (_dir, fs) = host_volume();
    assert!(matches!(
        fs.mknod_path("/regular-rdev", libc::S_IFREG | 0o600, 1),
        Err(ArgosError::Invalid(_))
    ));
    assert!(matches!(
        fs.mknod_path("/fifo-rdev", libc::S_IFIFO | 0o600, 1),
        Err(ArgosError::Invalid(_))
    ));
    assert!(matches!(
        fs.mknod_path("/socket-rdev", libc::S_IFSOCK | 0o600, 1),
        Err(ArgosError::Invalid(_))
    ));
    assert!(matches!(
        fs.mknod_path("/directory-mode", libc::S_IFDIR | 0o755, 0),
        Err(ArgosError::Unsupported(_))
    ));
    let regular = fs.mknod_path("/regular", 0o640, 0).unwrap();
    assert_eq!(
        fs.attr_inode(regular).unwrap().mode & libc::S_IFMT,
        libc::S_IFREG
    );
    assert!(fs.mknod_path("/regular", 0o640, 0).is_err());

    let parent = fs
        .mkdir_at_with_owner(ROOT_INO, OsStr::new("sgid"), 0o2775, 1000, 4242)
        .unwrap();
    let child = fs
        .mknod_at_with_owner(parent.ino, OsStr::new("child"), 0o600, 0, 1001, 9999)
        .unwrap();
    assert_eq!(child.gid, 4242);
}

#[test]
fn rename_covers_same_inode_exchange_cycles_cross_parent_and_type_conflicts() {
    let (_dir, fs) = host_volume();
    fs.write_file("/same-a", b"same", 0o600).unwrap();
    let same = fs.resolve_path("/same-a", true).unwrap();
    fs.link_at(same, ROOT_INO, OsStr::new("same-b")).unwrap();
    fs.rename_path("/same-a", "/same-b").unwrap();
    assert_eq!(fs.attr_path("/same-a", false).unwrap().ino, same);
    assert_eq!(fs.attr_path("/same-b", false).unwrap().ino, same);

    let a = fs.mkdir("/a", 0o755).unwrap();
    fs.mkdir("/a/b", 0o755).unwrap();
    assert!(fs
        .rename_at_with_policy(
            a,
            OsStr::new("b"),
            ROOT_INO,
            OsStr::new("a"),
            RenamePolicy {
                exchange: true,
                ..RenamePolicy::default()
            },
        )
        .is_err());

    let p1 = fs.mkdir("/p1", 0o755).unwrap();
    let p2 = fs.mkdir("/p2", 0o755).unwrap();
    fs.mkdir("/p1/d1", 0o755).unwrap();
    fs.mkdir("/p2/d2", 0o755).unwrap();
    let before_p1 = fs.attr_inode(p1).unwrap().nlink;
    let before_p2 = fs.attr_inode(p2).unwrap().nlink;
    fs.rename_at_with_policy(
        p1,
        OsStr::new("d1"),
        p2,
        OsStr::new("d2"),
        RenamePolicy {
            exchange: true,
            ..RenamePolicy::default()
        },
    )
    .unwrap();
    assert_eq!(fs.attr_inode(p1).unwrap().nlink, before_p1);
    assert_eq!(fs.attr_inode(p2).unwrap().nlink, before_p2);

    fs.mkdir("/nonempty", 0o755).unwrap();
    fs.write_file("/nonempty/child", b"x", 0o600).unwrap();
    fs.mkdir("/source-dir", 0o755).unwrap();
    assert!(fs.rename_path("/source-dir", "/nonempty").is_err());
    fs.write_file("/source-file", b"x", 0o600).unwrap();
    assert!(fs.rename_path("/source-file", "/nonempty").is_err());
    fs.mkdir("/empty-dir", 0o755).unwrap();
    fs.write_file("/another-file", b"x", 0o600).unwrap();
    assert!(fs.rename_path("/empty-dir", "/another-file").is_err());

    fs.write_file("/replace-old", b"old", 0o600).unwrap();
    fs.write_file("/replace-new", b"new", 0o600).unwrap();
    let old_ino = fs.resolve_path("/replace-old", true).unwrap();
    let new_ino = fs.resolve_path("/replace-new", true).unwrap();
    fs.rename_path("/replace-old", "/replace-new").unwrap();
    assert_eq!(fs.resolve_path("/replace-new", true).unwrap(), old_ino);
    assert!(fs.attr_inode(new_ino).is_err());

    fs.mkdir("/move-source", 0o755).unwrap();
    let left = fs.mkdir("/left", 0o755).unwrap();
    let right = fs.mkdir("/right", 0o755).unwrap();
    let left_links = fs.attr_inode(left).unwrap().nlink;
    let right_links = fs.attr_inode(right).unwrap().nlink;
    fs.rename_path("/move-source", "/right/moved").unwrap();
    assert_eq!(fs.attr_inode(left).unwrap().nlink, left_links);
    assert_eq!(fs.attr_inode(right).unwrap().nlink, right_links + 1);
}

#[test]
fn quota_rejected_rename_does_not_stage_deferred_reclaims() {
    let dir = tempfile::tempdir().unwrap();
    let image = dir.path().join("quota-rename.img");
    let fs = ArgosFs::create_loop(
        std::slice::from_ref(&image),
        VolumeConfig {
            k: 1,
            m: 0,
            compression: Compression::None,
            defer_metadata_commit: true,
            ..VolumeConfig::default()
        },
        32 * 1024 * 1024,
        "quota-rename",
        false,
    )
    .unwrap();

    fs.mkdir("/src", 0o755).unwrap();
    fs.mkdir("/q", 0o755).unwrap();
    fs.write_file("/src/file", &vec![b's'; 800], 0o644).unwrap();
    fs.write_file("/q/existing", &vec![b'd'; 600], 0o644)
        .unwrap();
    fs.set_directory_quota("/q", 700).unwrap();
    fs.sync().unwrap();

    assert!(fs.deferred_commit.lock().pending_reclaims.is_empty());
    assert!(matches!(
        fs.rename_path("/src/file", "/q/existing").unwrap_err(),
        ArgosError::QuotaExceeded {
            limit: 700,
            used: 800,
            ..
        }
    ));
    assert!(fs.deferred_commit.lock().pending_reclaims.is_empty());
    assert_eq!(fs.read_file("/q/existing", false).unwrap(), vec![b'd'; 600]);
    assert_eq!(fs.read_file("/src/file", false).unwrap(), vec![b's'; 800]);
}

struct OverlapProbeBackend {
    inner: Arc<dyn StorageBackend>,
    data_offset: u64,
    active: AtomicUsize,
    max_active: AtomicUsize,
}

impl StorageBackend for OverlapProbeBackend {
    fn backend_kind(&self) -> BackendKind {
        self.inner.backend_kind()
    }
    fn list_devices(&self) -> Result<Vec<crate::backend::BackendDeviceInfo>> {
        self.inner.list_devices()
    }
    fn read_at(&self, device_id: &String, offset: u64, buf: &mut [u8]) -> Result<()> {
        if offset != self.data_offset {
            return self.inner.read_at(device_id, offset, buf);
        }
        let n = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.max_active.fetch_max(n, Ordering::SeqCst);
        // Long enough for a second reader to reach the backend, with a
        // bounded duration if an exclusive metadata lock serializes them.
        std::thread::sleep(Duration::from_millis(250));
        let result = self.inner.read_at(device_id, offset, buf);
        self.active.fetch_sub(1, Ordering::SeqCst);
        result
    }
    fn write_at(&self, device_id: &String, offset: u64, data: &[u8]) -> Result<()> {
        self.inner.write_at(device_id, offset, data)
    }
    fn flush_device(&self, device_id: &String) -> Result<()> {
        self.inner.flush_device(device_id)
    }
    fn flush_all(&self) -> Result<()> {
        self.inner.flush_all()
    }
    fn drop_cache(&self, device_id: &String, offset: u64, length: u64) -> Result<()> {
        self.inner.drop_cache(device_id, offset, length)
    }
    fn capacity(&self, device_id: &String) -> Result<u64> {
        self.inner.capacity(device_id)
    }
    fn device_status(&self, device_id: &String) -> Result<DiskStatus> {
        self.inner.device_status(device_id)
    }
    fn capabilities(&self) -> crate::backend::BackendCapabilities {
        self.inner.capabilities()
    }
}

#[test]
fn concurrent_readers_share_metadata_guard_and_merge_telemetry() {
    let dir = tempfile::tempdir().unwrap();
    let image = dir.path().join("parallel-read.img");
    let mut fs = ArgosFs::create_loop(
        std::slice::from_ref(&image),
        VolumeConfig {
            k: 1,
            m: 0,
            chunk_size: 4096,
            compression: Compression::None,
            ..VolumeConfig::default()
        },
        32 * 1024 * 1024,
        "parallel-read",
        false,
    )
    .unwrap();
    let payload = vec![0x59u8; 8192];
    fs.write_file("/data", &payload, 0o600).unwrap();
    fs.sync().unwrap();
    let ino = fs.resolve_path("/data", true).unwrap();
    let original = fs.metadata_snapshot();
    let block = &original.inodes[&ino].blocks[0];
    let offset = match block.shards[0].location.as_ref().unwrap() {
        ShardLocation::RawExtent(extent) => extent.offset,
        _ => panic!("expected raw extent"),
    };
    fs.cache.remove(&format!(
        "{}:{}:{}",
        original.uuid, block.stripe_id, block.raw_sha256
    ));
    let backend = Arc::new(OverlapProbeBackend {
        inner: fs.backend.clone(),
        data_offset: offset,
        active: AtomicUsize::new(0),
        max_active: AtomicUsize::new(0),
    });
    fs.backend = backend.clone();
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let handles: Vec<_> = (0..2)
        .map(|_| {
            let fs = fs.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                fs.read_inode(ino, 0, 8192, true).unwrap()
            })
        })
        .collect();
    barrier.wait();
    for handle in handles {
        assert_eq!(handle.join().unwrap(), payload);
    }
    assert_eq!(
        backend.max_active.load(Ordering::SeqCst),
        2,
        "parallel read requests must reach the raw backend concurrently"
    );
    let latest = fs.metadata_snapshot();
    assert_eq!(
        latest.inodes[&ino].read_bytes,
        original.inodes[&ino].read_bytes + 2 * 8192
    );
    assert!(
        latest.disks.values().next().unwrap().io_samples
            >= original.disks.values().next().unwrap().io_samples + 2
    );
}

#[test]
fn read_decoder_snapshot_excludes_allocator_and_dirty_inode_state() {
    let dir = tempfile::tempdir().unwrap();
    let image = dir.path().join("snapshot.img");
    let fs = ArgosFs::create_loop(
        std::slice::from_ref(&image),
        VolumeConfig {
            k: 1,
            m: 0,
            chunk_size: 4096,
            compression: Compression::None,
            ..VolumeConfig::default()
        },
        32 * 1024 * 1024,
        "read-snapshot",
        false,
    )
    .unwrap();
    fs.write_file("/data", &[0x41; 8192], 0o600).unwrap();
    let ino = fs.resolve_path("/data", true).unwrap();
    let live = fs.meta.read();
    assert!(!live.raw_pool.allocators.is_empty());
    assert!(!live.inodes.is_empty());
    let decoder = ArgosFs::read_decoder_metadata(&live);
    assert!(decoder.raw_pool.allocators.is_empty());
    assert!(decoder.inodes.is_empty());
    assert_eq!(decoder.uuid, live.uuid);
    assert_eq!(decoder.disks.len(), live.disks.len());
    assert_eq!(decoder.layouts.len(), live.layouts.len());
    drop(live);
    assert_eq!(
        fs.read_inode(ino, 0, 8192, false).unwrap(),
        vec![0x41; 8192]
    );
}

/// Freeze scrub after it starts decoding its first physical stripe, so a
/// concurrent read can cache a reconstructed *later* stripe before scrub
/// reaches it. Scrub must still discover the later stripe's damage.
struct ScrubPauseBackend {
    inner: Arc<dyn StorageBackend>,
    pause_offset: u64,
    pause_once: std::sync::atomic::AtomicBool,
    paused: std::sync::mpsc::Sender<()>,
    resume: std::sync::Mutex<std::sync::mpsc::Receiver<()>>,
}

impl StorageBackend for ScrubPauseBackend {
    fn backend_kind(&self) -> BackendKind {
        self.inner.backend_kind()
    }
    fn list_devices(&self) -> Result<Vec<crate::backend::BackendDeviceInfo>> {
        self.inner.list_devices()
    }
    fn read_at(&self, device_id: &String, offset: u64, buf: &mut [u8]) -> Result<()> {
        if offset == self.pause_offset && self.pause_once.swap(false, Ordering::SeqCst) {
            self.paused.send(()).unwrap();
            self.resume.lock().unwrap().recv().unwrap();
        }
        self.inner.read_at(device_id, offset, buf)
    }
    fn write_at(&self, device_id: &String, offset: u64, data: &[u8]) -> Result<()> {
        self.inner.write_at(device_id, offset, data)
    }
    fn flush_device(&self, device_id: &String) -> Result<()> {
        self.inner.flush_device(device_id)
    }
    fn flush_all(&self) -> Result<()> {
        self.inner.flush_all()
    }
    fn drop_cache(&self, device_id: &String, offset: u64, length: u64) -> Result<()> {
        self.inner.drop_cache(device_id, offset, length)
    }
    fn capacity(&self, device_id: &String) -> Result<u64> {
        self.inner.capacity(device_id)
    }
    fn device_status(&self, device_id: &String) -> Result<DiskStatus> {
        self.inner.device_status(device_id)
    }
    fn capabilities(&self) -> crate::backend::BackendCapabilities {
        self.inner.capabilities()
    }
}

#[test]
fn scrub_checks_physical_shards_despite_concurrent_reconstruction_cache_insert() {
    let dir = tempfile::tempdir().unwrap();
    let images = (0..3)
        .map(|i| dir.path().join(format!("scrub-race-{i}.img")))
        .collect::<Vec<_>>();
    let mut fs = ArgosFs::create_loop(
        &images,
        VolumeConfig {
            k: 2,
            m: 1,
            chunk_size: 4096,
            compression: Compression::None,
            ..VolumeConfig::default()
        },
        32 * 1024 * 1024,
        "scrub-cache-race",
        false,
    )
    .unwrap();
    let payload = (0..64 * 1024).map(|i| (i % 251) as u8).collect::<Vec<_>>();
    fs.write_file("/data", &payload, 0o600).unwrap();
    fs.sync().unwrap();
    let ino = fs.resolve_path("/data", true).unwrap();
    let meta = fs.metadata_snapshot();
    let blocks = &meta.inodes[&ino].blocks;
    assert!(blocks.len() >= 2);
    let first_extent = match blocks[0].shards[0].location.as_ref().unwrap() {
        ShardLocation::RawExtent(extent) => extent,
        _ => panic!("expected raw extent"),
    };
    let corrupt = &blocks[1];
    let corrupt_shard = &corrupt.shards[0];
    let damaged_extent = match corrupt_shard.location.as_ref().unwrap() {
        ShardLocation::RawExtent(extent) => extent,
        _ => panic!("expected raw extent"),
    };
    let path = &images[corrupt_shard
        .disk_id
        .trim_start_matches("disk-")
        .parse::<usize>()
        .unwrap()];
    let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    std::os::unix::fs::FileExt::write_at(&file, &[0x42; 128], damaged_extent.offset).unwrap();
    file.sync_all().unwrap();
    let cache_key = format!("{}:{}:{}", meta.uuid, corrupt.stripe_id, corrupt.raw_sha256);
    fs.cache.remove(&cache_key);

    let (paused_tx, paused_rx) = std::sync::mpsc::channel();
    let (resume_tx, resume_rx) = std::sync::mpsc::channel();
    fs.backend = Arc::new(ScrubPauseBackend {
        inner: fs.backend.clone(),
        pause_offset: first_extent.offset,
        pause_once: std::sync::atomic::AtomicBool::new(true),
        paused: paused_tx,
        resume: std::sync::Mutex::new(resume_rx),
    });
    let scrub_fs = fs.clone();
    let read_len = payload.len();
    let handle = std::thread::spawn(move || {
        scrub_fs.read_inode_with_damage_report_for_scrub(ino, 0, read_len, false)
    });
    paused_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("scrub must reach first stripe");
    let start = corrupt.raw_offset;
    let reader_fs = fs.clone();
    let read_size = corrupt.raw_size;
    let reader = std::thread::spawn(move || reader_fs.read_inode(ino, start, read_size, false));
    // The reader cannot finish merging telemetry while scrub holds its
    // shared metadata guard. Observe cache publication instead of waiting
    // for the entire read, or the test itself would deadlock.
    let mut cached = false;
    for _ in 0..500 {
        if fs
            .cache
            .get(&cache_key, Some(&corrupt.raw_sha256))
            .is_some()
        {
            cached = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    resume_tx.send(()).unwrap();
    let read = reader.join().unwrap().unwrap();
    assert_eq!(
        read,
        payload[start as usize..start as usize + corrupt.raw_size]
    );
    assert!(
        cached,
        "ordinary read should reconstruct and cache damaged stripe"
    );
    let (scrub_read, damaged, repaired) = handle.join().unwrap().unwrap();
    assert_eq!(scrub_read, payload);
    assert!(!repaired);
    assert!(
        damaged.iter().any(|entry| entry.contains(&corrupt_shard.disk_id)),
        "scrub must report physical shard damage despite the concurrent cache insertion: {damaged:?}"
    );
}

#[test]
fn concurrent_read_rechecks_corrupt_shard_before_self_heal() {
    let dir = tempfile::tempdir().unwrap();
    let images = (0..3)
        .map(|i| dir.path().join(format!("repair-shard-{i}.img")))
        .collect::<Vec<_>>();
    let fs = ArgosFs::create_loop(
        &images,
        VolumeConfig {
            k: 2,
            m: 1,
            chunk_size: 4096,
            compression: Compression::None,
            ..VolumeConfig::default()
        },
        32 * 1024 * 1024,
        "self-heal-cache",
        false,
    )
    .unwrap();
    let payload = (0..64 * 1024).map(|i| (i % 251) as u8).collect::<Vec<_>>();
    fs.write_file("/damage", &payload, 0o600).unwrap();
    fs.sync().unwrap();
    let ino = fs.resolve_path("/damage", true).unwrap();
    let metadata = fs.metadata_snapshot();
    let block = &metadata.inodes[&ino].blocks[0];
    let shard = &block.shards[0];
    let extent = match shard.location.as_ref().unwrap() {
        ShardLocation::RawExtent(extent) => extent,
        _ => panic!("expected raw extent"),
    };
    let path = &images[shard
        .disk_id
        .trim_start_matches("disk-")
        .parse::<usize>()
        .unwrap()];
    let file = std::fs::OpenOptions::new().write(true).open(path).unwrap();
    std::os::unix::fs::FileExt::write_at(&file, &[0x4F; 128], extent.offset).unwrap();
    file.sync_all().unwrap();
    fs.cache.remove(&format!(
        "{}:{}:{}",
        metadata.uuid, block.stripe_id, block.raw_sha256
    ));
    let txid = metadata.txid;
    let (read, damaged, repaired) = fs
        .read_inode_with_damage_report(ino, 0, payload.len(), true)
        .unwrap();
    assert_eq!(read, payload);
    assert!(
        repaired,
        "the exclusive reread must ignore cached reconstructed data"
    );
    assert!(
        !damaged.is_empty(),
        "physical shard damage must be reported"
    );
    assert!(
        fs.metadata_snapshot().txid > txid,
        "self-heal writes a new valid stripe"
    );
    let (second, damage_after, repaired_again) = fs
        .read_inode_with_damage_report(ino, 0, payload.len(), true)
        .unwrap();
    assert_eq!(second, payload);
    assert!(
        damage_after.is_empty(),
        "the recovered stripes must be healthy"
    );
    assert!(!repaired_again);
}
#[test]
fn offline_delta_integrity_upgrade_replays_transactions_and_fences_downgrade() {
    let dir = tempfile::tempdir().unwrap();
    let images = (0..3)
        .map(|index| dir.path().join(format!("v2-{index}.img")))
        .collect::<Vec<_>>();
    let mut fs = ArgosFs::create_loop(
        &images,
        VolumeConfig {
            k: 2,
            m: 1,
            chunk_size: 4096,
            compression: Compression::None,
            defer_metadata_commit: true,
            defer_journal_flush: true,
            defer_data_flush: true,
            ..VolumeConfig::default()
        },
        32 * 1024 * 1024,
        "delta-v2-upgrade",
        false,
    )
    .unwrap();
    fs.write_file("/before", b"legacy snapshot", 0o600).unwrap();
    fs.sync().unwrap();
    assert_eq!(fs.metadata_snapshot().integrity.hash_scheme, None);
    fs.upgrade_delta_integrity().unwrap();
    for sb in fs.raw_superblocks.iter() {
        assert_ne!(
            sb.required_feature_flags & raw_format::REQUIRED_FEATURE_DELTA_INTEGRITY,
            0
        );
        // Both copies have the mandatory feature. A pre-v2 binary with
        // SUPPORTED_REQUIRED_FEATURE_FLAGS=0 rejects each member.
        let image = images
            .iter()
            .find(|path| {
                path.to_string_lossy()
                    .contains(&format!("-{}.img", sb.disk_index))
            })
            .unwrap();
        let image_file = std::fs::File::open(image).unwrap();
        for offset in [
            raw_format::PRIMARY_SUPERBLOCK_OFFSET,
            sb.backup_superblock_offset,
        ] {
            let mut data = [0u8; raw_format::SUPERBLOCK_SIZE];
            std::os::unix::fs::FileExt::read_at(&image_file, &mut data, offset).unwrap();
            assert_ne!(u64::from_le_bytes(data[48..56].try_into().unwrap()), 0);
        }
    }
    fs.write_file("/after", b"fast and durable", 0o600).unwrap();
    fs.sync().unwrap();
    assert_eq!(
        fs.metadata_snapshot().integrity.hash_scheme,
        Some(journal::DELTA_INTEGRITY_SCHEME)
    );
    let txid = fs.metadata_snapshot().txid;
    drop(fs);
    let reopened = ArgosFs::open_loop(&images, false).unwrap();
    assert_eq!(reopened.metadata_snapshot().txid, txid);
    assert_eq!(
        reopened.read_file("/before", false).unwrap(),
        b"legacy snapshot"
    );
    assert_eq!(
        reopened.read_file("/after", false).unwrap(),
        b"fast and durable"
    );
}

#[test]
fn v2_recovery_checkpoint_reanchors_normalized_metadata_before_next_delta() {
    let dir = tempfile::tempdir().unwrap();
    let images = (0..3)
        .map(|index| dir.path().join(format!("v2-restart-{index}.img")))
        .collect::<Vec<_>>();
    let mut fs = ArgosFs::create_loop(
        &images,
        VolumeConfig {
            k: 2,
            m: 1,
            chunk_size: 4096,
            defer_metadata_commit: true,
            ..VolumeConfig::default()
        },
        32 * 1024 * 1024,
        "v2-restart",
        false,
    )
    .unwrap();
    fs.upgrade_delta_integrity().unwrap();
    fs.write_file("/first", b"one", 0o600).unwrap();
    fs.sync().unwrap();
    assert_eq!(
        fs.metadata_snapshot().integrity.hash_scheme,
        Some(journal::DELTA_INTEGRITY_SCHEME)
    );
    drop(fs);

    let fs = ArgosFs::open_loop(&images, true).unwrap();
    assert!(fs.deferred_commit.lock().raw_checkpoint_required);
    fs.write_file("/second", b"two", 0o600).unwrap();
    fs.sync().unwrap();
    // The first post-recovery checkpoint verifies the normalized current
    // namespace rather than treating its recovered v2 digest as canonical.
    assert_eq!(fs.metadata_snapshot().integrity.hash_scheme, None);
    assert!(!fs.deferred_commit.lock().raw_checkpoint_required);
    fs.write_file("/third", b"three", 0o600).unwrap();
    fs.sync().unwrap();
    assert_eq!(
        fs.metadata_snapshot().integrity.hash_scheme,
        Some(journal::DELTA_INTEGRITY_SCHEME)
    );
    drop(fs);
    let recovered = ArgosFs::open_loop(&images, false).unwrap();
    for (path, contents) in [
        ("/first", b"one".as_slice()),
        ("/second", b"two".as_slice()),
        ("/third", b"three".as_slice()),
    ] {
        assert_eq!(recovered.read_file(path, false).unwrap(), contents);
    }
}

#[test]
fn v2_event_rollover_reanchors_deferred_hash_before_next_commit() {
    let dir = tempfile::tempdir().unwrap();
    let images = (0..3)
        .map(|n| dir.path().join(format!("event-rollover-{n}.img")))
        .collect::<Vec<_>>();
    let mut fs = ArgosFs::create_loop(
        &images,
        VolumeConfig {
            k: 2,
            m: 1,
            chunk_size: 4096,
            defer_metadata_commit: true,
            ..VolumeConfig::default()
        },
        32 * 1024 * 1024,
        "v2-event-rollover",
        false,
    )
    .unwrap();
    fs.upgrade_delta_integrity().unwrap();
    fs.write_file("/base", b"base", 0o600).unwrap();
    fs.sync().unwrap();
    assert_eq!(
        fs.metadata_snapshot().integrity.hash_scheme,
        Some(journal::DELTA_INTEGRITY_SCHEME)
    );
    // >2 MiB entry forces raw journal rollover/checkpoint. The same-txid
    // event checkpoint has a canonical digest rather than the old v2 digest.
    {
        let meta = fs.meta.read();
        fs.journal_locked(
            &meta,
            "autopilot",
            json!({"audit": "x".repeat(3 * 1024 * 1024)}),
        )
        .unwrap();
    }
    assert!(fs.deferred_commit.lock().raw_checkpoint_required);
    fs.write_file("/after-event", b"acknowledged", 0o600)
        .unwrap();
    fs.sync().unwrap();
    assert_eq!(fs.metadata_snapshot().integrity.hash_scheme, None);
    fs.write_file("/v2-again", b"new-chain", 0o600).unwrap();
    fs.sync().unwrap();
    assert_eq!(
        fs.metadata_snapshot().integrity.hash_scheme,
        Some(journal::DELTA_INTEGRITY_SCHEME)
    );
    drop(fs);
    let reopened = ArgosFs::open_loop(&images, false).unwrap();
    assert_eq!(reopened.read_file("/base", false).unwrap(), b"base");
    assert_eq!(
        reopened.read_file("/after-event", false).unwrap(),
        b"acknowledged"
    );
    assert_eq!(
        reopened.read_file("/v2-again", false).unwrap(),
        b"new-chain"
    );
    assert_eq!(reopened.transaction_report().unwrap().invalid_entries, 0);
}

#[test]
fn v2_added_block_members_inherit_required_features_and_survive_recovery() {
    let dir = tempfile::tempdir().unwrap();
    let mut images = (0..3)
        .map(|n| dir.path().join(format!("grow-{n}.img")))
        .collect::<Vec<_>>();
    let mut fs = ArgosFs::create_loop(
        &images,
        VolumeConfig {
            k: 2,
            m: 1,
            chunk_size: 4096,
            defer_metadata_commit: true,
            ..VolumeConfig::default()
        },
        32 * 1024 * 1024,
        "v2-grow",
        false,
    )
    .unwrap();
    fs.upgrade_delta_integrity().unwrap();
    for n in 3..6 {
        let next = dir.path().join(format!("grow-{n}.img"));
        fs.add_block_device(next.clone(), 32 * 1024 * 1024, false)
            .unwrap();
        images.push(next);
    }
    fs.write_file("/grown", b"quorum with six upgraded members", 0o600)
        .unwrap();
    fs.sync().unwrap();
    assert_eq!(
        fs.metadata_snapshot().integrity.hash_scheme,
        Some(journal::DELTA_INTEGRITY_SCHEME)
    );
    for path in &images {
        let (sb, _) = raw_store::inspect_device(BackendKind::LoopBlock, path.clone()).unwrap();
        assert_ne!(
            sb.required_feature_flags & raw_format::REQUIRED_FEATURE_DELTA_INTEGRITY,
            0
        );
    }
    drop(fs);
    let recovered = ArgosFs::open_loop(&images, false).unwrap();
    assert_eq!(
        recovered.read_file("/grown", false).unwrap(),
        b"quorum with six upgraded members"
    );
}

#[test]
fn upgraded_legacy_event_rollover_with_read_telemetry_reanchors_before_first_v2_delta() {
    let dir = tempfile::tempdir().unwrap();
    let images = (0..3)
        .map(|n| dir.path().join(format!("event-legacy-{n}.img")))
        .collect::<Vec<_>>();
    let mut fs = ArgosFs::create_loop(
        &images,
        VolumeConfig {
            k: 2,
            m: 1,
            chunk_size: 4096,
            defer_metadata_commit: true,
            ..VolumeConfig::default()
        },
        32 * 1024 * 1024,
        "legacy-event-v2",
        false,
    )
    .unwrap();
    fs.write_file("/old", b"old payload", 0o600).unwrap();
    fs.sync().unwrap();
    fs.upgrade_delta_integrity().unwrap();
    assert_eq!(fs.metadata_snapshot().integrity.hash_scheme, None);
    assert_eq!(fs.read_file("/old", false).unwrap(), b"old payload");
    assert!(fs.deferred_commit.lock().raw_uncommitted_metadata_dirty);
    {
        let meta = fs.meta.read();
        fs.journal_locked(
            &meta,
            "snapshot",
            json!({"audit": "x".repeat(3 * 1024 * 1024)}),
        )
        .unwrap();
    }
    assert!(fs.deferred_commit.lock().raw_checkpoint_required);
    fs.write_file("/first-new", b"first new", 0o600).unwrap();
    fs.sync().unwrap();
    assert_eq!(fs.metadata_snapshot().integrity.hash_scheme, None);
    fs.write_file("/v2", b"v2 data", 0o600).unwrap();
    fs.sync().unwrap();
    assert_eq!(
        fs.metadata_snapshot().integrity.hash_scheme,
        Some(journal::DELTA_INTEGRITY_SCHEME)
    );
    drop(fs);
    let reopened = ArgosFs::open_loop(&images, false).unwrap();
    for (path, bytes) in [
        ("/old", b"old payload".as_slice()),
        ("/first-new", b"first new".as_slice()),
        ("/v2", b"v2 data".as_slice()),
    ] {
        assert_eq!(reopened.read_file(path, false).unwrap(), bytes);
    }
}
#[test]
fn deferred_durability_cache_tracks_changed_inodes_and_device_status() {
    let dir = tempfile::tempdir().unwrap();
    let images = (0..3)
        .map(|i| dir.path().join(format!("durability-cache-{i}.img")))
        .collect::<Vec<_>>();
    let fs = ArgosFs::create_loop(
        &images,
        VolumeConfig {
            k: 2,
            m: 1,
            chunk_size: 4096,
            compression: Compression::None,
            defer_metadata_commit: true,
            ..VolumeConfig::default()
        },
        32 * 1024 * 1024,
        "durability-cache",
        false,
    )
    .unwrap();
    fs.write_file("/one", &vec![1; 8192], 0o600).unwrap();
    fs.write_file("/two", &vec![2; 8192], 0o600).unwrap();
    fs.sync().unwrap();
    let base = fs.metadata_snapshot();
    let mut cache = None;
    let unavailable = BTreeSet::new();
    ArgosFs::validate_deferred_data_durability(&base, &unavailable, &mut cache).unwrap();
    assert_eq!(cache.as_ref().unwrap().last_scanned_blocks, 2);

    let mut changed = base.clone();
    let one = changed.inodes[&1].entries["one"];
    changed.inodes.get_mut(&one).unwrap().read_bytes += 1;
    ArgosFs::validate_deferred_data_durability(&changed, &unavailable, &mut cache).unwrap();
    assert_eq!(cache.as_ref().unwrap().last_scanned_blocks, 1);

    // Disk availability changes invalidate the saved whole-namespace proof,
    // even when the inode map and all shard descriptors are unchanged.
    let one_device = changed.inodes[&one].blocks[0].shards[0].disk_id.clone();
    let unavailable = BTreeSet::from([one_device]);
    ArgosFs::validate_deferred_data_durability(&changed, &unavailable, &mut cache).unwrap();
    assert_eq!(cache.as_ref().unwrap().last_scanned_blocks, 2);

    // Layout parity changes must also invalidate the previously checked
    // snapshot, even if no inode was dirtied since the last validation.
    let mut reduced_parity = changed.clone();
    for layout in reduced_parity.layouts.values_mut() {
        layout.m = 0;
    }
    assert!(matches!(
        ArgosFs::validate_deferred_data_durability(&reduced_parity, &unavailable, &mut cache),
        Err(ArgosError::QuorumUnavailable { .. })
    ));

    // Altering the placement of a changed block must still reject loss of
    // data quorum. A failed validation must not poison the good cache.
    let mut corrupted = changed.clone();
    let shards = &mut corrupted.inodes.get_mut(&one).unwrap().blocks[0].shards;
    shards[0].disk_id = "missing-A".to_string();
    shards[1].disk_id = "missing-B".to_string();
    assert!(matches!(
        ArgosFs::validate_deferred_data_durability(&corrupted, &unavailable, &mut cache),
        Err(ArgosError::QuorumUnavailable { .. })
    ));
    assert_eq!(cache.as_ref().unwrap().last_scanned_blocks, 2);
}
