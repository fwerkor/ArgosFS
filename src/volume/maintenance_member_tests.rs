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
