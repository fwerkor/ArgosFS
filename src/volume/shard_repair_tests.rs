use super::*;
use tempfile::tempdir;

fn host_pool(disks: usize) -> (tempfile::TempDir, ArgosFs) {
    let dir = tempdir().unwrap();
    let fs = ArgosFs::create(
        dir.path(),
        VolumeConfig {
            k: 2,
            m: 1,
            chunk_size: 4096,
            compression: Compression::None,
            ..VolumeConfig::default()
        },
        disks,
        false,
    )
    .unwrap();
    (dir, fs)
}

fn file_inode(fs: &ArgosFs) -> Inode {
    let ino = fs.resolve_path("/file", true).unwrap();
    fs.metadata_snapshot().inodes[&ino].clone()
}

fn bytes() -> Vec<u8> {
    (0..29_000).map(|i| ((i * 17 + 19) % 251) as u8).collect()
}

#[test]
fn rebuild_missing_shards_without_touching_healthy_extents_or_file_times() {
    let (dir, fs) = host_pool(4);
    let content = bytes();
    fs.write_file("/file", &content, 0o600).unwrap();
    let before = file_inode(&fs);
    assert!(before.blocks.len() > 2);
    let lost = before.blocks[0].shards[0].disk_id.clone();
    let expected_shards = before
        .blocks
        .iter()
        .flat_map(|block| &block.shards)
        .filter(|shard| shard.disk_id == lost)
        .count() as u64;
    let expected_bytes: u64 = before
        .blocks
        .iter()
        .flat_map(|block| &block.shards)
        .filter(|shard| shard.disk_id == lost)
        .map(|shard| shard.size as u64)
        .sum();

    fs.mark_disk(&lost, DiskStatus::Removed).unwrap();
    assert_eq!(fs.read_file("/file", false).unwrap(), content);
    let report = fs.scrub().unwrap();
    assert_eq!(report.unrecoverable_files, 0, "{report:?}");
    assert_eq!(report.repaired_files, 1, "{report:?}");
    assert_eq!(report.reconstructed_shards, expected_shards, "{report:?}");
    assert_eq!(report.repair_bytes_written, expected_bytes, "{report:?}");
    assert_eq!(report.full_file_rewrites, 0, "{report:?}");

    let after = file_inode(&fs);
    assert_eq!(before.size, after.size);
    assert_eq!(before.mtime, after.mtime);
    assert_eq!(before.ctime, after.ctime);
    assert_eq!(before.write_count, after.write_count);
    for (old, new) in before.blocks.iter().zip(&after.blocks) {
        assert_eq!(old.stripe_id, new.stripe_id);
        assert_eq!(old.raw_sha256, new.raw_sha256);
        assert_eq!(old.nonce_hex, new.nonce_hex);
        for shard in &old.shards {
            let replacement = new
                .shards
                .iter()
                .find(|other| other.slot == shard.slot)
                .unwrap();
            if shard.disk_id != lost {
                assert_eq!(
                    serde_json::to_value(shard).unwrap(),
                    serde_json::to_value(replacement).unwrap(),
                    "healthy shard changed"
                );
            } else {
                assert_ne!(shard.disk_id, replacement.disk_id);
            }
        }
    }
    assert_eq!(fs.read_file("/file", false).unwrap(), content);
    let second = fs.scrub().unwrap();
    assert_eq!(second.reconstructed_shards, 0, "{second:?}");
    assert_eq!(second.repaired_files, 0, "{second:?}");
    assert_eq!(fs.fsck(false, false).unwrap().orphan_shards, 0);
    fs.sync().unwrap();
    drop(fs);
    let reopened = ArgosFs::open(dir.path()).unwrap();
    assert_eq!(reopened.read_file("/file", false).unwrap(), content);
    assert_eq!(reopened.fsck(false, false).unwrap().missing_shards, 0);
}

#[test]
fn reconstruct_checksum_bad_parity_to_a_new_cow_path() {
    let (_dir, fs) = host_pool(4);
    let content = bytes();
    fs.write_file("/file", &content, 0o600).unwrap();
    let before = file_inode(&fs);
    let corrupt = before.blocks[0].shards.last().unwrap().clone();
    let path = fs.shard_path_locked(&fs.metadata_snapshot(), &corrupt.disk_id, &corrupt.relpath);
    std::fs::write(&path, vec![0x5a; corrupt.size]).unwrap();

    let report = fs.scrub().unwrap();
    assert_eq!(report.reconstructed_shards, 1, "{report:?}");
    assert_eq!(report.checksum_errors, 1, "{report:?}");
    assert_eq!(report.repaired_files, 1, "{report:?}");
    let after = file_inode(&fs);
    assert_eq!(after.blocks[0].stripe_id, before.blocks[0].stripe_id);
    let replacement = &after.blocks[0].shards[corrupt.slot];
    assert_ne!(replacement.relpath, corrupt.relpath);
    assert_eq!(fs.read_file("/file", false).unwrap(), content);
    let check = fs.fsck(false, false).unwrap();
    assert_eq!(check.checksum_errors, 0, "{check:?}");
    assert_eq!(check.orphan_shards, 0, "{check:?}");
}

#[test]
fn repair_without_spare_keeps_degraded_data_and_retries_after_add_disk() {
    let (dir, fs) = host_pool(3);
    let content = bytes();
    fs.write_file("/file", &content, 0o600).unwrap();
    let before = file_inode(&fs);
    let lost = before.blocks[0].shards[0].disk_id.clone();
    fs.mark_disk(&lost, DiskStatus::Removed).unwrap();
    let failed = fs.scrub().unwrap();
    assert_eq!(failed.repaired_files, 0, "{failed:?}");
    assert_eq!(failed.reconstructed_shards, 0, "{failed:?}");
    assert!(!failed.errors.is_empty(), "{failed:?}");
    assert_eq!(
        serde_json::to_value(&file_inode(&fs).blocks).unwrap(),
        serde_json::to_value(&before.blocks).unwrap()
    );
    assert_eq!(fs.read_file("/file", false).unwrap(), content);
    fs.add_disk(
        Some(dir.path().join("replacement")),
        None,
        None,
        None,
        false,
    )
    .unwrap();
    let report = fs.scrub().unwrap();
    assert_eq!(report.repaired_files, 1, "{report:?}");
    assert!(report.reconstructed_shards > 0, "{report:?}");
    assert_eq!(fs.read_file("/file", false).unwrap(), content);
}

#[test]
fn encrypted_reconstruction_keeps_ciphertext_layout_and_nonce() {
    let _guard = crypto::test_env_lock();
    let previous = std::env::var_os("ARGOSFS_KEY");
    unsafe { std::env::set_var("ARGOSFS_KEY", "correct horse battery staple") };
    let (_dir, fs) = host_pool(4);
    fs.enable_encryption("correct horse battery staple")
        .unwrap();
    let content = bytes();
    fs.write_file("/file", &content, 0o600).unwrap();
    let before = file_inode(&fs);
    assert!(before.blocks.iter().all(|block| block.encrypted));
    let lost = before.blocks[0].shards[0].disk_id.clone();
    fs.mark_disk(&lost, DiskStatus::Failed).unwrap();
    let report = fs.scrub().unwrap();
    assert_eq!(report.repaired_files, 1, "{report:?}");
    let after = file_inode(&fs);
    for (old, new) in before.blocks.iter().zip(&after.blocks) {
        assert_eq!(old.stripe_id, new.stripe_id);
        assert_eq!(old.nonce_hex, new.nonce_hex);
        assert_eq!(old.raw_sha256, new.raw_sha256);
        for shard in &old.shards {
            if shard.disk_id != lost {
                let saved = new
                    .shards
                    .iter()
                    .find(|other| other.slot == shard.slot)
                    .unwrap();
                assert_eq!(shard.sha256, saved.sha256);
                assert_eq!(shard.disk_id, saved.disk_id);
            }
        }
    }
    assert_eq!(fs.read_file("/file", false).unwrap(), content);
    assert_eq!(fs.fsck(false, false).unwrap().checksum_errors, 0);
    if let Some(key) = previous {
        unsafe { std::env::set_var("ARGOSFS_KEY", key) };
    } else {
        unsafe { std::env::remove_var("ARGOSFS_KEY") };
    }
}

#[test]
fn interrupted_host_commit_removes_unpublished_cow_shards() {
    let (_dir, fs) = host_pool(4);
    let content = bytes();
    fs.write_file("/file", &content, 0o600).unwrap();
    let before = file_inode(&fs);
    let lost = before.blocks[0].shards[0].disk_id.clone();
    fs.mark_disk(&lost, DiskStatus::Removed).unwrap();
    {
        let _crash = journal::thread_crash_point("before-journal");
        let failed = fs.scrub().unwrap();
        assert_eq!(failed.repaired_files, 0, "{failed:?}");
        assert_eq!(failed.reconstructed_shards, 0, "{failed:?}");
        assert!(!failed.errors.is_empty());
    }
    assert_eq!(
        serde_json::to_value(&file_inode(&fs).blocks).unwrap(),
        serde_json::to_value(&before.blocks).unwrap()
    );
    let check = fs.fsck(false, false).unwrap();
    assert_eq!(check.orphan_shards, 0, "{check:?}");
    assert_eq!(fs.read_file("/file", false).unwrap(), content);
    assert_eq!(fs.scrub().unwrap().repaired_files, 1);
    assert_eq!(fs.read_file("/file", false).unwrap(), content);
}

#[test]
fn two_lost_members_in_one_stripe_are_not_published_as_a_partial_repair() {
    let (_dir, fs) = host_pool(5);
    let content = bytes();
    fs.write_file("/file", &content, 0o600).unwrap();
    let before = file_inode(&fs);
    let first_stripe = &before.blocks[0];
    let first = first_stripe.shards[0].disk_id.clone();
    let second = first_stripe.shards[1].disk_id.clone();
    fs.mark_disk(&first, DiskStatus::Removed).unwrap();
    fs.mark_disk(&second, DiskStatus::Failed).unwrap();
    let failed = fs.scrub().unwrap();
    assert_eq!(failed.reconstructed_shards, 0, "{failed:?}");
    assert_eq!(failed.repaired_files, 0, "{failed:?}");
    assert_eq!(failed.unrecoverable_files, 1, "{failed:?}");
    assert_eq!(
        serde_json::to_value(&file_inode(&fs).blocks).unwrap(),
        serde_json::to_value(&before.blocks).unwrap()
    );
    fs.mark_disk(&second, DiskStatus::Online).unwrap();
    let fixed = fs.scrub().unwrap();
    assert_eq!(fixed.repaired_files, 1, "{fixed:?}");
    assert_eq!(fixed.unrecoverable_files, 0, "{fixed:?}");
    assert_eq!(fs.read_file("/file", false).unwrap(), content);
}

#[test]
fn raw_extent_reconstruction_is_restartable_and_reclaims_old_allocations() {
    let dir = tempdir().unwrap();
    let images = (0..4)
        .map(|i| dir.path().join(format!("member-{i}.img")))
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
        "repair",
        false,
    )
    .unwrap();
    let content = bytes();
    fs.write_file("/file", &content, 0o600).unwrap();
    let before = file_inode(&fs);
    let lost = before.blocks[0].shards[0].disk_id.clone();
    fs.mark_disk(&lost, DiskStatus::Removed).unwrap();

    // A data-write failure must leave inode references and allocators intact;
    // re-running repair may safely allocate a fresh extent.
    {
        let _crash = journal::thread_crash_point(FaultPoint::BeforeDataWrite.as_str());
        let failed = fs.scrub().unwrap();
        assert_eq!(failed.repaired_files, 0, "{failed:?}");
        assert_eq!(failed.reconstructed_shards, 0, "{failed:?}");
        assert!(!failed.errors.is_empty(), "{failed:?}");
    }
    assert_eq!(
        serde_json::to_value(&file_inode(&fs).blocks).unwrap(),
        serde_json::to_value(&before.blocks).unwrap()
    );
    // Also exercise a failure after the new payload was written and flushed,
    // but before a metadata reference could be committed.
    {
        let _crash =
            journal::thread_crash_point(FaultPoint::AfterDataFlushBeforeJournalCommit.as_str());
        let failed = fs.scrub().unwrap();
        assert_eq!(failed.repaired_files, 0, "{failed:?}");
        assert_eq!(failed.reconstructed_shards, 0, "{failed:?}");
        assert!(!failed.errors.is_empty(), "{failed:?}");
    }
    assert_eq!(
        serde_json::to_value(&file_inode(&fs).blocks).unwrap(),
        serde_json::to_value(&before.blocks).unwrap()
    );
    let fixed = fs.scrub().unwrap();
    assert!(fixed.reconstructed_shards > 0, "{fixed:?}");
    assert_eq!(fixed.repaired_files, 1, "{fixed:?}");
    let valid = fs.fsck(false, false).unwrap();
    assert!(valid.errors.is_empty(), "{valid:?}");
    assert_eq!(fs.read_file("/file", false).unwrap(), content);

    // The old member can return without resurrecting its stale shard extents.
    fs.mark_disk(&lost, DiskStatus::Online).unwrap();
    fs.sync().unwrap();
    fs.mark_clean_unmount().unwrap();
    drop(fs);
    let reopened = ArgosFs::open_loop(&images, true).unwrap();
    assert_eq!(reopened.read_file("/file", false).unwrap(), content);
    let checked = reopened.fsck(false, false).unwrap();
    assert_eq!(checked.missing_shards, 0, "{checked:?}");
    assert!(checked.errors.is_empty(), "{checked:?}");
    assert_eq!(reopened.scrub().unwrap().reconstructed_shards, 0);
}

#[test]
fn raw_fsck_reclaims_unreferenced_extents_and_preserves_live_data() {
    let dir = tempdir().unwrap();
    let images = (0..3)
        .map(|i| dir.path().join(format!("reclaim-{i}.img")))
        .collect::<Vec<_>>();
    let fs = ArgosFs::create_loop(
        &images,
        VolumeConfig {
            k: 2,
            m: 1,
            chunk_size: 4096,
            compression: Compression::None,
            defer_metadata_commit: true,
            ..Default::default()
        },
        32 * 1024 * 1024,
        "reclaim",
        false,
    )
    .unwrap();
    let payload = bytes();
    fs.write_file("/file", &payload, 0o600).unwrap();
    fs.sync().unwrap();
    let lost = {
        let mut meta = fs.meta.write();
        let generation = meta.txid + 1;
        let extent = allocator::allocate(
            meta.raw_pool.allocators.get_mut("disk-0000").unwrap(),
            "disk-0000",
            8192,
            generation,
        )
        .unwrap();
        fs.commit_locked(&mut meta, "test-unreferenced-allocation", json!({}))
            .unwrap();
        extent
    };
    fs.sync().unwrap();
    let check = fs.fsck(false, false).unwrap();
    assert_eq!(check.orphan_extent_bytes, lost.length);
    assert_eq!(check.reclaimed_extent_bytes, 0);
    fs.mark_clean_unmount().unwrap();
    drop(fs);

    let readonly = ArgosFs::open_loop(&images, false).unwrap();
    assert!(matches!(
        readonly.fsck(false, true),
        Err(ArgosError::ReadonlyRequired(_))
    ));
    assert_eq!(
        readonly.fsck(false, false).unwrap().orphan_extent_bytes,
        lost.length
    );
    assert_eq!(readonly.read_file("/file", false).unwrap(), payload);
    drop(readonly);

    let fs = ArgosFs::open_loop(&images, true).unwrap();
    let fixed = fs.fsck(false, true).unwrap();
    assert_eq!(fixed.reclaimed_extent_bytes, lost.length);
    assert_eq!(fs.fsck(false, false).unwrap().orphan_extent_bytes, 0);
    assert_eq!(fs.read_file("/file", false).unwrap(), payload);
    fs.mark_clean_unmount().unwrap();
    drop(fs);
    let fs = ArgosFs::open_loop(&images, false).unwrap();
    let checked = fs.fsck(true, true).unwrap();
    assert!(checked.errors.is_empty(), "{checked:?}");
    assert_eq!(checked.orphan_extent_bytes, 0);
    assert_eq!(checked.reclaimed_extent_bytes, 0);
    assert_eq!(fs.read_file("/file", false).unwrap(), payload);
}

#[test]
fn raw_fsck_failed_immediate_commit_restores_allocator_and_txid() {
    let dir = tempdir().unwrap();
    let images = (0..3)
        .map(|i| dir.path().join(format!("rollback-{i}.img")))
        .collect::<Vec<_>>();
    let fs = ArgosFs::create_loop(
        &images,
        VolumeConfig {
            k: 2,
            m: 1,
            chunk_size: 4096,
            compression: Compression::None,
            defer_metadata_commit: false,
            ..Default::default()
        },
        32 * 1024 * 1024,
        "rollback",
        false,
    )
    .unwrap();
    let payload = bytes();
    fs.write_file("/file", &payload, 0o600).unwrap();
    let lost = {
        let mut meta = fs.meta.write();
        let generation = meta.txid + 1;
        let extent = allocator::allocate(
            meta.raw_pool.allocators.get_mut("disk-0000").unwrap(),
            "disk-0000",
            8192,
            generation,
        )
        .unwrap();
        fs.commit_locked(&mut meta, "test-unreferenced-allocation", json!({}))
            .unwrap();
        extent
    };
    fs.sync().unwrap();
    let before = fs.metadata_snapshot();
    {
        let _crash = journal::thread_crash_point(FaultPoint::AfterPartialJournalFanout.as_str());
        assert!(matches!(
            fs.fsck(false, true),
            Err(ArgosError::InjectedCrash(_))
        ));
    }
    let after = fs.metadata_snapshot();
    assert_eq!(after.txid, before.txid);
    assert_eq!(
        serde_json::to_value(&after.raw_pool.allocators).unwrap(),
        serde_json::to_value(&before.raw_pool.allocators).unwrap()
    );
    assert_eq!(
        fs.fsck(false, false).unwrap().orphan_extent_bytes,
        lost.length
    );
    assert_eq!(fs.read_file("/file", false).unwrap(), payload);
    assert_eq!(
        fs.fsck(false, true).unwrap().reclaimed_extent_bytes,
        lost.length
    );
    fs.mark_clean_unmount().unwrap();
    drop(fs);
    let reopened = ArgosFs::open_loop(&images, false).unwrap();
    assert_eq!(reopened.fsck(false, false).unwrap().orphan_extent_bytes, 0);
    assert_eq!(reopened.read_file("/file", false).unwrap(), payload);
}
