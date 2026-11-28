use super::*;
use tempfile::tempdir;

fn redundant_host_volume() -> (tempfile::TempDir, ArgosFs) {
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
    (dir, fs)
}

#[test]
fn targeted_member_repair_only_visits_referenced_stripes() {
    let (_dir, fs) = redundant_host_volume();
    for index in 0..20 {
        fs.write_file(&format!("/file-{index}"), &vec![index as u8; 8192], 0o600)
            .unwrap();
    }
    let before = fs.metadata_snapshot();
    let member = before
        .inodes
        .values()
        .find(|inode| inode.kind == NodeKind::File)
        .unwrap()
        .blocks[0]
        .shards[0]
        .disk_id
        .clone();
    let affected = before
        .inodes
        .values()
        .filter(|inode| {
            inode.kind == NodeKind::File
                && inode
                    .blocks
                    .iter()
                    .any(|block| block.shards.iter().any(|shard| shard.disk_id == member))
        })
        .count();
    assert!(affected > 0);
    assert!(affected < 20, "fixture must have unaffected files");

    assert!(fs.repair_member_shards(&member, |_, _, _| {}).is_err());
    fs.mark_disk(&member, DiskStatus::Failed).unwrap();

    let mut updates = Vec::new();
    let report = fs
        .repair_member_shards(&member, |done, total, progress| {
            updates.push((done, total, progress.reconstructed_shards));
        })
        .unwrap();
    assert_eq!(report.files_checked, affected as u64);
    assert_eq!(report.repaired_files, affected as u64, "{report:?}");
    assert_eq!(report.unrecoverable_files, 0, "{report:?}");
    assert_eq!(report.full_file_rewrites, 0);
    assert!(report.reconstructed_shards > 0);
    assert_eq!(updates.last().unwrap().0, affected);
    assert_eq!(updates.last().unwrap().1, affected);
    assert_eq!(updates.last().unwrap().2, report.reconstructed_shards);
    for index in 0..20 {
        let path = format!("/file-{index}");
        assert_eq!(fs.read_file(&path, false).unwrap(), vec![index as u8; 8192]);
    }
    let after = fs.metadata_snapshot();
    for inode in after.inodes.values().filter(|i| i.kind == NodeKind::File) {
        assert!(inode
            .blocks
            .iter()
            .flat_map(|block| &block.shards)
            .all(|shard| shard.disk_id != member));
    }
    let healthy = fs.fsck(false, false).unwrap();
    assert_eq!(healthy.unrecoverable_files, 0, "{healthy:?}");
    assert_eq!(healthy.checksum_errors, 0, "{healthy:?}");
}

#[test]
fn targeted_member_repair_rejects_unknown_and_transient_offline_members() {
    let (_dir, fs) = redundant_host_volume();
    fs.write_file("/test", &[7u8; 8192], 0o600).unwrap();
    assert!(matches!(
        fs.repair_member_shards("not-present", |_, _, _| {}),
        Err(ArgosError::NotFound(_))
    ));
    let id = fs
        .metadata_snapshot()
        .inodes
        .values()
        .find(|inode| inode.kind == NodeKind::File)
        .unwrap()
        .blocks[0]
        .shards[0]
        .disk_id
        .clone();
    fs.mark_disk(&id, DiskStatus::Offline).unwrap();
    // A member that just went offline must not be rebuilt before its
    // transient-failure grace expires.
    assert!(matches!(
        fs.repair_member_shards(&id, |_, _, _| {}),
        Err(ArgosError::Invalid(_))
    ));
}

#[test]
fn targeted_member_repair_without_spare_is_deferred_and_retryable() {
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
        3,
        false,
    )
    .unwrap();
    let payload = vec![0x5a; 8192];
    fs.write_file("/recoverable", &payload, 0o600).unwrap();
    let member = fs
        .metadata_snapshot()
        .inodes
        .values()
        .find(|inode| inode.kind == NodeKind::File)
        .unwrap()
        .blocks[0]
        .shards[0]
        .disk_id
        .clone();
    fs.mark_disk(&member, DiskStatus::Failed).unwrap();

    let report = fs.repair_member_shards(&member, |_, _, _| {}).unwrap();
    assert_eq!(report.files_checked, 1, "{report:?}");
    assert_eq!(report.damaged_files, 1, "{report:?}");
    assert_eq!(report.repaired_files, 0, "{report:?}");
    assert_eq!(report.unrecoverable_files, 0, "{report:?}");
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.contains("repair deferred")
                && error.contains("not enough online disks")),
        "{report:?}"
    );
    assert_eq!(fs.read_file("/recoverable", false).unwrap(), payload);

    fs.add_disk(
        Some(dir.path().join("replacement")),
        None,
        None,
        Some(64 * 1024 * 1024),
        false,
    )
    .unwrap();
    let retry = fs.repair_member_shards(&member, |_, _, _| {}).unwrap();
    assert_eq!(retry.repaired_files, 1, "{retry:?}");
    assert_eq!(retry.unrecoverable_files, 0, "{retry:?}");
    assert_eq!(fs.read_file("/recoverable", false).unwrap(), payload);
}

#[test]
fn targeted_member_repair_reports_insufficient_survivors_as_unrecoverable() {
    let (_dir, fs) = redundant_host_volume();
    fs.write_file("/lost", &[0x29; 8192], 0o600).unwrap();
    let snapshot = fs.metadata_snapshot();
    let block = &snapshot
        .inodes
        .values()
        .find(|inode| inode.kind == NodeKind::File)
        .unwrap()
        .blocks[0];
    let failed = &block.shards[0].disk_id;
    let corrupt = &block.shards[1];
    let path = fs.shard_path_locked(&snapshot, &corrupt.disk_id, &corrupt.relpath);
    fs.mark_disk(failed, DiskStatus::Failed).unwrap();
    std::fs::write(path, b"corrupted shard").unwrap();

    let report = fs.repair_member_shards(failed, |_, _, _| {}).unwrap();
    assert_eq!(report.files_checked, 1, "{report:?}");
    assert_eq!(report.damaged_files, 1, "{report:?}");
    assert_eq!(report.repaired_files, 0, "{report:?}");
    assert_eq!(report.unrecoverable_files, 1, "{report:?}");
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.contains("unrecoverable stripe")),
        "{report:?}"
    );
}

#[test]
fn full_scrub_progress_preserves_complete_verification() {
    let (_dir, fs) = redundant_host_volume();
    fs.write_file("/data", &[3_u8; 4096], 0o600).unwrap();
    let mut last = None;
    let report = fs
        .scrub_with_progress(|done, total, partial| {
            last = Some((done, total, partial.files_checked));
        })
        .unwrap();
    let (done, total, checked) = last.unwrap();
    assert_eq!(done, total);
    assert_eq!(checked, report.files_checked);
    assert!(report.files_checked > 0);
    assert_eq!(report.unrecoverable_files, 0);
}

#[test]
fn targeted_member_repair_recovers_on_loop_backend_after_reopen() {
    let dir = tempdir().unwrap();
    let images = (0..4)
        .map(|index| dir.path().join(format!("member-{index}.img")))
        .collect::<Vec<_>>();
    let fs = ArgosFs::create_loop_legacy(
        &images,
        VolumeConfig {
            k: 2,
            m: 1,
            compression: Compression::None,
            chunk_size: 4096,
            ..VolumeConfig::default()
        },
        32 * 1024 * 1024,
        "targeted-rebuild",
        false,
    )
    .unwrap();
    for index in 0..4 {
        fs.write_file(&format!("/block-{index}"), &[index as u8; 8192], 0o600)
            .unwrap();
    }
    let member = fs
        .metadata_snapshot()
        .inodes
        .values()
        .find(|inode| inode.kind == NodeKind::File)
        .unwrap()
        .blocks[0]
        .shards[0]
        .disk_id
        .clone();
    fs.mark_disk(&member, DiskStatus::Failed).unwrap();
    let report = fs.repair_member_shards(&member, |_, _, _| {}).unwrap();
    assert!(report.reconstructed_shards > 0, "{report:?}");
    assert_eq!(report.unrecoverable_files, 0, "{report:?}");
    fs.sync().unwrap();
    fs.mark_clean_unmount().unwrap();
    drop(fs);

    let reopened = ArgosFs::open_loop(&images, true).unwrap();
    for index in 0..4 {
        assert_eq!(
            reopened
                .read_file(&format!("/block-{index}"), false)
                .unwrap(),
            vec![index as u8; 8192]
        );
    }
    let health = reopened.fsck(false, false).unwrap();
    assert_eq!(health.unrecoverable_files, 0, "{health:?}");
    assert_eq!(health.checksum_errors, 0, "{health:?}");
}

#[test]
fn encrypted_targeted_member_repair_derives_key_once_for_all_affected_inodes() {
    let _guard = crypto::test_env_lock();
    let previous = std::env::var_os("ARGOSFS_KEY");
    unsafe { std::env::set_var("ARGOSFS_KEY", "member-targeted-crypt-test") };
    let (_dir, fs) = redundant_host_volume();
    fs.enable_encryption("member-targeted-crypt-test").unwrap();
    for index in 0..8 {
        fs.write_file(&format!("/encrypted-{index}"), &[index as u8; 8192], 0o600)
            .unwrap();
    }
    let before = fs.metadata_snapshot();
    let mut usage = BTreeMap::<String, usize>::new();
    for inode in before
        .inodes
        .values()
        .filter(|inode| inode.kind == NodeKind::File)
    {
        for block in &inode.blocks {
            assert!(block.encrypted);
            for shard in &block.shards {
                *usage.entry(shard.disk_id.clone()).or_default() += 1;
            }
        }
    }
    let member = usage.into_iter().max_by_key(|(_, count)| *count).unwrap().0;
    let affected = before
        .inodes
        .values()
        .filter(|inode| {
            inode
                .blocks
                .iter()
                .any(|block| block.shards.iter().any(|shard| shard.disk_id == member))
        })
        .count();
    assert!(affected >= 2);
    fs.mark_disk(&member, DiskStatus::Failed).unwrap();

    super::maintenance_key::reset_derivation_attempts();
    let report = fs.repair_member_shards(&member, |_, _, _| {}).unwrap();
    assert_eq!(report.files_checked, affected as u64);
    assert_eq!(report.unrecoverable_files, 0, "{report:?}");
    assert_eq!(report.repaired_files, affected as u64, "{report:?}");
    assert_eq!(super::maintenance_key::derivation_attempts(), 1);
    for index in 0..8 {
        assert_eq!(
            fs.read_file(&format!("/encrypted-{index}"), false).unwrap(),
            vec![index as u8; 8192]
        );
    }
    if let Some(key) = previous {
        unsafe { std::env::set_var("ARGOSFS_KEY", key) };
    } else {
        unsafe { std::env::remove_var("ARGOSFS_KEY") };
    }
}

#[test]
fn missing_targeted_rebuild_key_is_deferred_and_reports_final_progress() {
    let _guard = crypto::test_env_lock();
    let previous = std::env::var_os("ARGOSFS_KEY");
    unsafe { std::env::set_var("ARGOSFS_KEY", "member-rebuild-key-required") };
    let (_dir, fs) = redundant_host_volume();
    fs.enable_encryption("member-rebuild-key-required").unwrap();
    fs.write_file("/encrypted", &[42; 8192], 0o600).unwrap();
    let snapshot = fs.metadata_snapshot();
    let block = &snapshot
        .inodes
        .values()
        .find(|inode| inode.kind == NodeKind::File)
        .unwrap()
        .blocks[0];
    let member = block.shards[0].disk_id.clone();
    fs.mark_disk(&member, DiskStatus::Failed).unwrap();
    unsafe { std::env::remove_var("ARGOSFS_KEY") };

    let mut last_progress = None;
    let report = fs
        .repair_member_shards(&member, |done, total, partial| {
            last_progress = Some((done, total, partial.repaired_files));
        })
        .unwrap();
    assert_eq!(report.files_checked, 1);
    assert_eq!(report.repaired_files, 0);
    assert_eq!(report.unrecoverable_files, 0, "{report:?}");
    assert!(report
        .errors
        .iter()
        .any(|line| line.contains("ARGOSFS_KEY")));
    assert_eq!(last_progress, Some((1, 1, 0)));

    if let Some(key) = previous {
        unsafe { std::env::set_var("ARGOSFS_KEY", key) };
    } else {
        unsafe { std::env::remove_var("ARGOSFS_KEY") };
    }
}

#[test]
fn member_catalog_tracks_mutations_and_shared_snapshots_without_wire_changes() {
    let (_dir, fs) = redundant_host_volume();
    fs.write_file("/file", &vec![9; 8192], 0o600).unwrap();
    let base = fs.metadata_snapshot();
    let ino = base.inodes[&1].entries["file"];
    let member = base.inodes[&ino].blocks[0].shards[0].disk_id.clone();
    let before = serde_json::to_vec(&base.inodes).unwrap();
    base.inodes.initialize_member_catalog();
    assert_eq!(before, serde_json::to_vec(&base.inodes).unwrap());
    assert_eq!(base.inodes.member_inode_ids(&member), vec![ino]);
    let mut changed = base.inodes.clone();
    changed.get_mut(&ino).unwrap().blocks[0]
        .shards
        .iter_mut()
        .filter(|s| s.disk_id == member)
        .for_each(|s| s.disk_id = "replacement".into());
    assert!(changed.member_inode_ids(&member).is_empty());
    assert_eq!(changed.member_inode_ids("replacement"), vec![ino]);
    assert_eq!(base.inodes.member_inode_ids(&member), vec![ino]);
    changed.mark_committed();
    assert!(changed.member_inode_ids(&member).is_empty());
    assert_eq!(changed.member_inode_ids("replacement"), vec![ino]);
    let bytes = serde_json::to_vec(&changed).unwrap();
    let mut recovered: crate::model::inodes::TrackedInodes =
        serde_json::from_slice(&bytes).unwrap();
    assert_eq!(recovered.member_inode_ids("replacement"), vec![ino]);
    let removed = recovered.remove(&ino).unwrap();
    assert!(recovered.member_inode_ids("replacement").is_empty());
    recovered.mark_committed();
    assert!(recovered.member_inode_ids("replacement").is_empty());
    recovered.insert(ino, removed);
    assert_eq!(recovered.member_inode_ids("replacement"), vec![ino]);
    recovered.mark_committed();
    assert_eq!(recovered.member_inode_ids("replacement"), vec![ino]);
}

#[test]
fn member_catalog_selects_sparse_work_from_100k_inode_namespace() {
    let (_dir, fs) = redundant_host_volume();
    fs.write_file("/file", &vec![9; 8192], 0o600).unwrap();
    let mut snapshot = fs.metadata_snapshot();
    let ino = snapshot.inodes[&1].entries["file"];
    let member = snapshot.inodes[&ino].blocks[0].shards[0].disk_id.clone();
    let empty = snapshot.inodes[&1].clone();
    for n in 1000..101000 {
        let mut inode = empty.clone();
        inode.id = n;
        inode.entries.clear();
        snapshot.inodes.insert(n, inode);
    }
    snapshot.inodes.mark_committed();
    assert_eq!(snapshot.inodes.len(), 100002);
    assert_eq!(snapshot.inodes.member_inode_ids(&member), vec![ino]);
    snapshot.inodes.get_mut(&1000).unwrap().mtime += 1.0;
    assert_eq!(snapshot.inodes.member_inode_ids(&member), vec![ino]);
    snapshot.inodes.mark_committed();
    assert_eq!(snapshot.inodes.member_inode_ids(&member), vec![ino]);
}
