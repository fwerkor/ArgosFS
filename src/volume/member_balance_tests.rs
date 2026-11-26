use super::*;

#[test]
fn returning_member_receives_verified_shards_without_rewriting_files() {
    let dir = tempfile::tempdir().unwrap();
    let paths = (0..4)
        .map(|i| dir.path().join(format!("member-{i}.img")))
        .collect::<Vec<_>>();
    let fs = ArgosFs::create_loop(
        &paths,
        VolumeConfig {
            k: 2,
            m: 1,
            chunk_size: 4096,
            compression: Compression::None,
            defer_metadata_commit: true,
            defer_data_flush: true,
            ..Default::default()
        },
        32 * 1024 * 1024,
        "member-balance",
        false,
    )
    .unwrap();
    let target = "disk-0003";
    fs.mark_disk(target, DiskStatus::Offline).unwrap();
    let bytes = (0..256 * 1024).map(|i| (i % 251) as u8).collect::<Vec<_>>();
    fs.write_file("/older", &bytes, 0o600).unwrap();
    fs.mkdir("/repository", 0o700).unwrap();
    fs.write_file("/repository/file", &bytes, 0o600).unwrap();
    fs.mark_disk(target, DiskStatus::Online).unwrap();
    fs.sync().unwrap();
    let ino = fs.resolve_path("/repository/file", true).unwrap();
    let before = fs.metadata_snapshot();
    assert_eq!(before.disks[target].used_bytes, 0);
    {
        let _crash = journal::thread_crash_point(FaultPoint::BeforeDataWrite.as_str());
        assert!(fs.rebalance_member(target, 1).is_err());
    }
    assert_eq!(fs.metadata_snapshot().disks[target].used_bytes, 0);
    let older = fs.resolve_path("/older", true).unwrap();
    assert!(older < ino);
    let options = MemberBalanceOptions {
        path: Some("/repository".into()),
        min_inode: Some(ino),
        max_inode: Some(ino),
        prefer_data: true,
    };
    let first = fs
        .rebalance_member_with_options(target, 2, &options)
        .unwrap();
    assert_eq!(first.moved_shards, 2);
    assert_eq!(first.data_shards_moved, 2);
    assert_eq!(first.parity_shards_moved, 0);
    let first_snapshot = fs.metadata_snapshot();
    assert!(first_snapshot.inodes[&older]
        .blocks
        .iter()
        .all(|b| b.shards.iter().all(|s| s.disk_id != target)));
    assert_eq!(
        first_snapshot.inodes[&ino]
            .blocks
            .iter()
            .flat_map(|b| &b.shards)
            .filter(|s| s.disk_id == target && s.slot < 2)
            .count(),
        2
    );
    let report = fs.rebalance_member(target, usize::MAX).unwrap();
    assert!(report.moved_shards > 0, "{report:?}");
    assert!(report.used_after > report.used_before);
    let after = fs.metadata_snapshot();
    let a = &before.inodes[&ino];
    let b = &after.inodes[&ino];
    assert_eq!(a.mtime, b.mtime);
    assert_eq!(a.ctime, b.ctime);
    assert_eq!(a.blocks.len(), b.blocks.len());
    for (old, new) in a.blocks.iter().zip(&b.blocks) {
        assert_eq!(old.stripe_id, new.stripe_id);
        assert_eq!(old.raw_sha256, new.raw_sha256);
        assert_eq!(old.layout_id, new.layout_id);
        assert_eq!(
            new.shards
                .iter()
                .map(|s| &s.disk_id)
                .collect::<BTreeSet<_>>()
                .len(),
            new.shards.len()
        );
        for (x, y) in old.shards.iter().zip(&new.shards) {
            assert_eq!(x.sha256, y.sha256);
            assert_eq!(x.slot, y.slot);
            if y.disk_id != target {
                assert_eq!(
                    serde_json::to_value(x).unwrap(),
                    serde_json::to_value(y).unwrap()
                );
            }
        }
    }
    assert_eq!(fs.read_file("/repository/file", false).unwrap(), bytes);
    assert!(fs.fsck(false, false).unwrap().errors.is_empty());
    fs.mark_clean_unmount().unwrap();
    drop(fs);
    let fs = ArgosFs::open_loop(&paths, false).unwrap();
    assert_eq!(fs.read_file("/repository/file", false).unwrap(), bytes);
    assert!(fs.fsck(false, false).unwrap().errors.is_empty());
    assert!(fs.metadata_snapshot().disks[target].used_bytes > 0);
}
