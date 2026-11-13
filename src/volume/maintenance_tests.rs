use super::*;
use std::sync::{Mutex as StdMutex, OnceLock};
use tempfile::tempdir;

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<StdMutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| StdMutex::new(())).lock().unwrap()
}

fn host_volume(disks: usize) -> (tempfile::TempDir, ArgosFs) {
    let dir = tempdir().unwrap();
    let fs = ArgosFs::create(
        dir.path(),
        VolumeConfig {
            k: 1,
            m: 0,
            ..VolumeConfig::default()
        },
        disks,
        false,
    )
    .unwrap();
    (dir, fs)
}

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

/// Remove a member's shard and mark the member offline.  The file is still
/// fully recoverable from parity, so this is degraded rather than damaged.
fn lose_one_member(fs: &ArgosFs, path: &str) -> String {
    let ino = fs.resolve_path(path, true).unwrap();
    let (disk_id, relpath) = {
        let meta = fs.metadata_snapshot();
        let shard = &meta.inodes[&ino].blocks[0].shards[0];
        (shard.disk_id.clone(), shard.relpath.clone())
    };
    let shard_path = fs.shard_path_locked(&fs.metadata_snapshot(), &disk_id, &relpath);
    {
        let mut meta = fs.meta.write();
        let disk = meta.disks.get_mut(&disk_id).unwrap();
        disk.status = DiskStatus::Offline;
        disk.manually_offline = false;
        disk.offline_since = Some(now_f64());
    }
    std::fs::remove_file(shard_path).unwrap();
    disk_id
}

#[test]
fn scrub_does_not_rewrite_files_for_an_offline_member() {
    let _guard = env_lock();
    std::env::set_var("ARGOSFS_OFFLINE_GRACE_MS", "60000");
    let (_dir, fs) = redundant_host_volume();
    fs.write_file("/file", &vec![7u8; 8192], 0o600).unwrap();
    lose_one_member(&fs, "/file");

    let report = fs.scrub().unwrap();

    assert_eq!(report.unrecoverable_files, 0, "{report:?}");
    assert_eq!(report.damaged_files, 0, "{report:?}");
    assert_eq!(report.repaired_files, 0, "{report:?}");
    assert_eq!(report.checksum_errors, 0, "{report:?}");
    assert!(report.missing_shards >= 1, "{report:?}");
    assert_eq!(
        fs.read_file("/file", false).unwrap(),
        vec![7u8; 8192],
        "the file must still read correctly"
    );
    std::env::remove_var("ARGOSFS_OFFLINE_GRACE_MS");
}

#[test]
fn scrub_repairs_an_offline_member_after_the_grace_period() {
    let _guard = env_lock();
    std::env::set_var("ARGOSFS_OFFLINE_GRACE_MS", "60000");
    let (_dir, fs) = redundant_host_volume();
    fs.write_file("/file", &vec![7u8; 8192], 0o600).unwrap();
    let disk_id = lose_one_member(&fs, "/file");
    {
        let mut meta = fs.meta.write();
        meta.disks.get_mut(&disk_id).unwrap().offline_since = Some(now_f64() - 120.0);
    }

    let report = fs.scrub().unwrap();

    assert_eq!(report.unrecoverable_files, 0, "{report:?}");
    assert!(report.damaged_files >= 1, "{report:?}");
    assert!(report.repaired_files >= 1, "{report:?}");
    let report_after = fs.scrub().unwrap();
    assert_eq!(report_after.damaged_files, 0, "{report_after:?}");
    assert_eq!(report_after.repaired_files, 0, "{report_after:?}");
    assert_eq!(
        fs.read_file("/file", false).unwrap(),
        vec![7u8; 8192],
        "the file must still read correctly after the repair"
    );
    std::env::remove_var("ARGOSFS_OFFLINE_GRACE_MS");
}

#[test]
fn incremental_scrub_rechecks_cached_shards_after_offline_grace() {
    let _guard = env_lock();
    std::env::set_var("ARGOSFS_OFFLINE_GRACE_MS", "60000");
    let (_dir, fs) = redundant_host_volume();
    fs.write_file("/file", &vec![4u8; 8192], 0o600).unwrap();
    let disk_id = lose_one_member(&fs, "/file");

    // Decode the degraded file once so its complete contents are cached.
    assert_eq!(fs.read_file("/file", false).unwrap(), vec![4u8; 8192]);
    let before = fs.metadata_snapshot().txid;
    {
        let mut meta = fs.meta.write();
        meta.disks.get_mut(&disk_id).unwrap().offline_since = Some(now_f64() - 120.0);
    }

    let (report, _) = fs.scrub_limited(1, None);
    assert_eq!(report.files_checked, 1, "{report:?}");
    assert_eq!(report.damaged_files, 1, "{report:?}");
    assert_eq!(report.repaired_files, 1, "{report:?}");
    assert!(report.missing_shards >= 1, "{report:?}");
    assert!(fs.metadata_snapshot().txid > before);
    assert_eq!(fs.read_file("/file", false).unwrap(), vec![4u8; 8192]);
    std::env::remove_var("ARGOSFS_OFFLINE_GRACE_MS");
}

#[test]
fn scrub_repairs_a_removed_member() {
    let (_dir, fs) = redundant_host_volume();
    fs.write_file("/file", &vec![9u8; 8192], 0o600).unwrap();
    let (disk_id, relpath) = {
        let ino = fs.resolve_path("/file", true).unwrap();
        let meta = fs.metadata_snapshot();
        let shard = &meta.inodes[&ino].blocks[0].shards[0];
        (shard.disk_id.clone(), shard.relpath.clone())
    };
    let shard_path = fs.shard_path_locked(&fs.metadata_snapshot(), &disk_id, &relpath);
    fs.mark_disk(&disk_id, DiskStatus::Removed).unwrap();
    std::fs::remove_file(shard_path).unwrap();

    let report = fs.scrub().unwrap();

    assert_eq!(report.unrecoverable_files, 0, "{report:?}");
    assert!(report.damaged_files >= 1, "{report:?}");
    assert!(report.repaired_files >= 1, "{report:?}");
    assert_eq!(report.checksum_errors, 0, "{report:?}");
    let report_after = fs.scrub().unwrap();
    assert_eq!(report_after.damaged_files, 0, "{report_after:?}");
    assert_eq!(report_after.repaired_files, 0, "{report_after:?}");
    assert_eq!(
        fs.read_file("/file", false).unwrap(),
        vec![9u8; 8192],
        "the file must still read correctly after the repair"
    );
}

#[test]
fn briefly_missing_loop_member_returns_without_rewriting_stripes() {
    let _guard = env_lock();
    std::env::set_var("ARGOSFS_OFFLINE_GRACE_MS", "60000");
    let dir = tempdir().unwrap();
    let images = (0..4)
        .map(|index| dir.path().join(format!("reconnect-{index}.img")))
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
        "reconnect",
        false,
    )
    .unwrap();
    for index in 0..3 {
        fs.write_file(&format!("/file-{index}"), &vec![index as u8; 16384], 0o600)
            .unwrap();
    }
    let original = fs.metadata_snapshot();
    let stripes = original
        .inodes
        .iter()
        .filter(|(_, inode)| inode.kind == NodeKind::File)
        .map(|(ino, inode)| (*ino, serde_json::to_value(&inode.blocks).unwrap()))
        .collect::<Vec<_>>();
    let missing_id = original
        .inodes
        .values()
        .find(|inode| inode.kind == NodeKind::File)
        .unwrap()
        .blocks[0]
        .shards[0]
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
    let degraded = ArgosFs::open_loop(&present_images, true).unwrap();
    let full = degraded.scrub().unwrap();
    let (incremental, _) = degraded.scrub_limited(10, None);
    for report in [&full, &incremental] {
        assert_eq!(report.damaged_files, 0, "{report:?}");
        assert_eq!(report.repaired_files, 0, "{report:?}");
        assert_eq!(report.unrecoverable_files, 0, "{report:?}");
        assert!(report.missing_shards > 0, "{report:?}");
    }
    for (ino, blocks) in &stripes {
        assert_eq!(
            serde_json::to_value(&degraded.metadata_snapshot().inodes[ino].blocks).unwrap(),
            *blocks
        );
    }
    degraded.mark_clean_unmount().unwrap();
    drop(degraded);

    let restored = ArgosFs::open_loop(&images, true).unwrap();
    assert_eq!(
        restored.metadata_snapshot().disks[&missing_id].status,
        DiskStatus::Online
    );
    let after_return = restored.scrub().unwrap();
    assert_eq!(after_return.repaired_files, 0, "{after_return:?}");
    assert_eq!(after_return.missing_shards, 0, "{after_return:?}");
    for (ino, blocks) in &stripes {
        assert_eq!(
            serde_json::to_value(&restored.metadata_snapshot().inodes[ino].blocks).unwrap(),
            *blocks
        );
    }
    for index in 0..3 {
        assert_eq!(
            restored
                .read_file(&format!("/file-{index}"), false)
                .unwrap(),
            vec![index as u8; 16384]
        );
    }
    std::env::remove_var("ARGOSFS_OFFLINE_GRACE_MS");
}

#[test]
fn runtime_usb_quarantine_waits_for_grace_before_incremental_repair() {
    let _guard = env_lock();
    std::env::set_var("ARGOSFS_OFFLINE_GRACE_MS", "60000");
    let dir = tempdir().unwrap();
    let images = (0..4)
        .map(|index| dir.path().join(format!("hotplug-{index}.img")))
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
        "hotplug",
        false,
    )
    .unwrap();
    fs.write_file("/file", &vec![5u8; 8192], 0o600).unwrap();
    let ino = fs.resolve_path("/file", true).unwrap();
    let disk_id = fs.metadata_snapshot().inodes[&ino].blocks[0].shards[0]
        .disk_id
        .clone();

    // A fatal I/O failure is quarantined while this mount is still open.
    // It must behave like a briefly missing scan member.
    fs.quarantine_device(&disk_id);
    assert_eq!(fs.read_file("/file", true).unwrap(), vec![5u8; 8192]);
    let (recent, _) = fs.scrub_limited(1, None);
    assert_eq!(recent.damaged_files, 0, "{recent:?}");
    assert_eq!(recent.repaired_files, 0, "{recent:?}");
    assert!(recent.missing_shards > 0, "{recent:?}");

    fs.quarantine_since
        .lock()
        .insert(disk_id.clone(), now_f64() - 120.0);
    let (expired, _) = fs.scrub_limited(1, None);
    assert_eq!(expired.damaged_files, 1, "{expired:?}");
    assert_eq!(expired.repaired_files, 1, "{expired:?}");
    assert_eq!(fs.read_file("/file", false).unwrap(), vec![5u8; 8192]);
    std::env::remove_var("ARGOSFS_OFFLINE_GRACE_MS");
}

#[test]
fn scrub_rebuilds_logically_retired_members_even_if_shards_are_readable() {
    for status in [DiskStatus::Failed, DiskStatus::Removed] {
        let (_dir, fs) = redundant_host_volume();
        fs.write_file("/file", &vec![6u8; 8192], 0o600).unwrap();
        let ino = fs.resolve_path("/file", true).unwrap();
        let disk_id = fs.metadata_snapshot().inodes[&ino].blocks[0].shards[0]
            .disk_id
            .clone();
        fs.mark_disk(&disk_id, status).unwrap();

        let report = fs.scrub().unwrap();
        assert_eq!(report.damaged_files, 1, "{status:?}: {report:?}");
        assert_eq!(report.repaired_files, 1, "{status:?}: {report:?}");
        assert_eq!(report.unrecoverable_files, 0, "{status:?}: {report:?}");
        assert!(fs.metadata_snapshot().inodes[&ino]
            .blocks
            .iter()
            .flat_map(|block| &block.shards)
            .all(|shard| shard.disk_id != disk_id));
        assert_eq!(fs.read_file("/file", false).unwrap(), vec![6u8; 8192]);
    }
}

#[test]
fn io_policy_and_encryption_cover_empty_repeat_and_wrong_passphrases() {
    let _guard = env_lock();
    let (_dir, fs) = host_volume(1);
    fs.set_io_policy(IoMode::Direct, true, false, false)
        .unwrap();
    let policy = fs.io_policy();
    assert_eq!(policy.io_mode, IoMode::Direct);
    assert!(policy.direct_io);
    assert!(!policy.zero_copy);
    assert!(!policy.numa_aware);

    assert!(matches!(
        fs.enable_encryption(""),
        Err(ArgosError::Invalid(_))
    ));
    fs.enable_encryption("correct").unwrap();
    fs.enable_encryption("correct").unwrap();
    assert!(matches!(
        fs.enable_encryption("wrong"),
        Err(ArgosError::PermissionDenied(_))
    ));
    assert!(fs.health_report().encryption_enabled);
}

#[test]
fn add_disk_clamps_weight_tracks_capacity_and_rejects_duplicate_paths() {
    let (dir, fs) = host_volume(1);
    let path = dir.path().join("extra");
    let id = fs
        .add_disk(
            Some(path.clone()),
            Some(StorageTier::Cold),
            Some(0.0),
            Some(12345),
            false,
        )
        .unwrap();
    let meta = fs.metadata_snapshot();
    let disk = &meta.disks[&id];
    assert_eq!(disk.tier, StorageTier::Cold);
    assert_eq!(disk.weight, 0.01);
    assert_eq!(disk.capacity_bytes, 12345);
    assert_eq!(disk.capacity_source, CapacitySource::UserOverride);
    assert!(path.join("argosfs-disk.json").exists());
    assert!(matches!(
        fs.add_disk(Some(path), None, None, None, false),
        Err(ArgosError::AlreadyExists(_))
    ));

    let automatic = fs.add_disk(None, None, None, None, true).unwrap();
    assert!(fs.metadata_snapshot().disks.contains_key(&automatic));
}

#[test]
fn host_block_device_and_missing_disk_operations_report_expected_errors() {
    let (dir, fs) = host_volume(1);
    assert!(matches!(
        fs.add_block_device(dir.path().join("device.img"), 32 * 1024 * 1024, false),
        Err(ArgosError::Unsupported(_))
    ));
    assert!(matches!(
        fs.mark_disk("missing", DiskStatus::Offline),
        Err(ArgosError::NotFound(_))
    ));
    assert!(matches!(
        fs.set_disk_health("missing", HealthCounters::default()),
        Err(ArgosError::NotFound(_))
    ));
    assert!(matches!(
        fs.refresh_disk_probe(Some("missing")),
        Err(ArgosError::NotFound(_))
    ));
    assert!(matches!(
        fs.refresh_smart_health(Some("missing")),
        Err(ArgosError::NotFound(_))
    ));
    assert!(matches!(
        fs.drain_disk("missing"),
        Err(ArgosError::NotFound(_))
    ));
}

#[test]
fn health_probe_and_smart_refresh_cover_observation_and_all_failed_paths() {
    let (_dir, fs) = host_volume(1);
    let disk_id = fs.metadata_snapshot().disks.keys().next().unwrap().clone();
    fs.set_disk_health(
        &disk_id,
        HealthCounters {
            latency_ms: 12.5,
            temperature_c: 40.0,
            ..HealthCounters::default()
        },
    )
    .unwrap();
    let disk = &fs.metadata_snapshot().disks[&disk_id];
    assert_eq!(disk.read_latency_ewma_ms, 12.5);
    assert_eq!(disk.write_latency_ewma_ms, 12.5);
    assert_eq!(disk.health.temperature_c, 40.0);

    let before = fs.metadata_snapshot().disks[&disk_id].clone();
    let observed = fs.refresh_disk_probe_observations(Some(&disk_id)).unwrap();
    assert_eq!(observed.len(), 1);
    let after_observation = fs.metadata_snapshot().disks[&disk_id].clone();
    assert_eq!(after_observation.tier, before.tier);
    assert_eq!(after_observation.weight, before.weight);
    assert!(after_observation.io_samples > before.io_samples);
    assert_eq!(fs.refresh_disk_probe(None).unwrap().len(), 1);
    match fs.refresh_smart_health(Some(&disk_id)) {
        Ok(refreshed) => assert_eq!(refreshed.len(), 1),
        Err(ArgosError::Unsupported(_)) => {}
        Err(error) => panic!("unexpected SMART refresh error: {error}"),
    }
}

#[test]
fn explicit_health_replacement_can_clear_recent_smart_evidence() {
    let (_dir, fs) = host_volume(1);
    let disk_id = fs.metadata_snapshot().disks.keys().next().unwrap().clone();
    fs.set_disk_health(
        &disk_id,
        HealthCounters {
            pending_sectors: 8,
            ..HealthCounters::default()
        },
    )
    .unwrap();
    fs.set_disk_health(&disk_id, HealthCounters::default())
        .unwrap();
    let cleared = fs.metadata_snapshot().disks[&disk_id].health.clone();
    assert_eq!(cleared.pending_sectors, 0);
    assert_eq!(cleared.smart_evidence_score, 0.0);
    assert!(cleared.smart_evidence_updated_at > 0.0);
}

#[test]
fn partial_health_override_preserves_unobserved_recent_deltas() {
    let (_dir, fs) = host_volume(1);
    let disk_id = fs.metadata_snapshot().disks.keys().next().unwrap().clone();
    fs.set_disk_health(
        &disk_id,
        HealthCounters {
            io_errors: 40,
            smart_status_failed: true,
            ..HealthCounters::default()
        },
    )
    .unwrap();
    let before = fs.metadata_snapshot().disks[&disk_id].health.clone();
    assert_eq!(before.recent_io_error_delta, 40);

    let mut override_values = before.clone();
    override_values.latency_ms = 10.0;
    override_values.smart_status_failed = false;
    fs.set_disk_health_overrides(
        &disk_id,
        override_values,
        SmartCounterObservations::default(),
    )
    .unwrap();

    let after = fs.metadata_snapshot().disks[&disk_id].health.clone();
    assert_eq!(after.recent_io_error_delta, before.recent_io_error_delta);
    assert_eq!(after.smart_evidence_score, before.smart_evidence_score);
    assert_eq!(
        after.smart_evidence_updated_at,
        before.smart_evidence_updated_at
    );
    assert_eq!(after.latency_ms, 10.0);
    assert!(!after.smart_status_failed);
}

#[test]
fn drain_remove_rebalance_and_zero_budget_paths_cover_capacity_rules() {
    let (dir, fs) = host_volume(1);
    let original = fs.metadata_snapshot().disks.keys().next().unwrap().clone();
    assert!(matches!(
        fs.drain_disk(&original),
        Err(ArgosError::NotEnoughDisks { .. })
    ));
    let extra = fs
        .add_disk(
            Some(dir.path().join("extra-disk")),
            None,
            None,
            Some(64 * 1024 * 1024),
            false,
        )
        .unwrap();
    assert_eq!(fs.drain_disk(&extra).unwrap(), 0);
    assert_eq!(fs.remove_disk(&extra).unwrap(), 0);
    assert_eq!(
        fs.metadata_snapshot().disks[&extra].status,
        DiskStatus::Removed
    );

    assert_eq!(fs.rebalance_limited(0, Some(123)).unwrap(), (0, Some(123)));
    let (scrub, cursor) = fs.scrub_limited(0, Some(456));
    assert_eq!(scrub.files_checked, 0);
    assert_eq!(cursor, Some(456));
    assert_eq!(fs.rebalance().unwrap(), 0);
}

#[test]
fn reshape_validates_targets_completes_empty_layouts_and_resumes_matching_state() {
    let (dir, fs) = host_volume(1);
    assert!(matches!(
        fs.reshape_layout(0, 1, None),
        Err(ArgosError::Invalid(_))
    ));
    assert!(matches!(
        fs.reshape_layout(2, 1, None),
        Err(ArgosError::NotEnoughDisks { .. })
    ));
    let complete = fs.reshape_layout(1, 0, Some(0)).unwrap();
    assert!(complete.complete);
    assert_eq!(complete.remaining_files, 0);

    fs.write_file("/file", &vec![7u8; 8192], 0o600).unwrap();
    let extra1 = fs
        .add_disk(
            Some(dir.path().join("reshape-1")),
            None,
            None,
            Some(64 * 1024 * 1024),
            false,
        )
        .unwrap();
    let extra2 = fs
        .add_disk(
            Some(dir.path().join("reshape-2")),
            None,
            None,
            Some(64 * 1024 * 1024),
            false,
        )
        .unwrap();
    assert!(fs.metadata_snapshot().disks.contains_key(&extra1));
    assert!(fs.metadata_snapshot().disks.contains_key(&extra2));
    let partial = fs.reshape_layout(2, 1, Some(0)).unwrap();
    assert!(!partial.complete);
    assert!(partial.remaining_files > 0);
    let resumed = fs.reshape_layout(2, 1, Some(1)).unwrap();
    assert_eq!(resumed.reshape_id, partial.reshape_id);
    assert!(resumed.rewritten_files >= 1);
}

#[test]
fn fsck_reports_missing_directory_children_orphans_and_repairs_usage() {
    let (dir, fs) = host_volume(1);
    fs.write_file("/file", &vec![1u8; 8192], 0o600).unwrap();
    let disk_id = fs.metadata_snapshot().disks.keys().next().unwrap().clone();
    let disk_root = {
        let meta = fs.metadata_snapshot();
        relative_or_absolute(dir.path(), &meta.disks[&disk_id].path)
    };
    let orphan = disk_root.join("shards/orphan.blk");
    ensure_private_dir(orphan.parent().unwrap()).unwrap();
    std::fs::write(&orphan, b"orphan").unwrap();
    {
        let mut meta = fs.meta.write();
        meta.inodes
            .get_mut(&ROOT_INO)
            .unwrap()
            .entries
            .insert("missing".to_string(), 9999);
        meta.disks.get_mut(&disk_id).unwrap().used_bytes = u64::MAX;
    }
    let report = fs.fsck(false, false).unwrap();
    assert!(report
        .errors
        .iter()
        .any(|error| error.contains("missing inode")));
    assert_eq!(report.orphan_shards, 1);
    assert!(orphan.exists());

    let repaired = fs.fsck(true, true).unwrap();
    assert_eq!(repaired.removed_orphans, 1);
    assert!(!orphan.exists());
    assert_ne!(fs.metadata_snapshot().disks[&disk_id].used_bytes, u64::MAX);
    let scrubbed = fs.scrub().unwrap();
    assert!(scrubbed.files_checked >= 1);
}

#[test]
fn scrub_reports_shard_read_retries() {
    let _guard = env_lock();
    std::env::set_var("ARGOSFS_READ_RETRIES", "1");
    let (_dir, fs) = host_volume(1);
    fs.write_file("/file", &vec![9u8; 8192], 0o600).unwrap();
    let (disk_id, relpath) = {
        let meta = fs.metadata_snapshot();
        let ino = fs.resolve_path("/file", true).unwrap();
        let shard = &meta.inodes[&ino].blocks[0].shards[0];
        (shard.disk_id.clone(), shard.relpath.clone())
    };
    let path = fs.shard_path_locked(&fs.metadata_snapshot(), &disk_id, &relpath);
    let mut data = std::fs::read(&path).unwrap();
    data[0] ^= 0xff;
    std::fs::write(&path, &data).unwrap();

    let report = fs.scrub().unwrap();
    std::env::remove_var("ARGOSFS_READ_RETRIES");

    assert!(report.checksum_errors >= 1);
    assert!(report.shard_read_retries >= 1);
    assert_eq!(report.shard_read_recovered, 0);
}

#[test]
fn health_report_counts_all_inode_kinds_and_delay_parser_is_best_effort() {
    let _guard = env_lock();
    let (_dir, fs) = host_volume(1);
    fs.write_file("/file", b"data", 0o600).unwrap();
    fs.mkdir("/dir", 0o755).unwrap();
    fs.symlink_path("/file", "/link").unwrap();
    fs.mknod_path("/fifo", libc::S_IFIFO | 0o600, 0).unwrap();
    let report = fs.health_report();
    assert_eq!(report.files, 1);
    assert_eq!(report.directories, 2);
    assert_eq!(report.symlinks, 1);
    assert_eq!(report.specials, 1);
    assert_eq!(report.disks.len(), 1);

    for value in ["invalid", "0"] {
        std::env::set_var("ARGOSFS_TEST_MAINTENANCE_AFTER_READ_DELAY_MS", value);
        maintenance_after_read_delay();
    }
    std::env::remove_var("ARGOSFS_TEST_MAINTENANCE_AFTER_READ_DELAY_MS");
}

#[test]
fn fsck_accepts_healthy_file_above_io_window_limit() {
    let (_dir, fs) = host_volume(1);
    let payload = vec![0x5a; 8192];
    fs.write_file("/large", &payload, 0o600).unwrap();
    let ino = fs.resolve_path("/large", true).unwrap();
    let large_size = MAX_IN_MEMORY_IO_BYTES as u64 + 4096;
    // A sparse extension gives a real >256 MiB inode without consuming
    // hundreds of megabytes or writing synthetic data for the regression.
    fs.truncate_inode(ino, large_size).unwrap();
    let before = fs.metadata_snapshot().inodes[&ino].clone();

    for repair in [false, true] {
        let report = fs.fsck(repair, false).unwrap();
        assert_eq!(report.files_checked, 1, "{report:?}");
        assert_eq!(report.damaged_files, 0, "{report:?}");
        assert_eq!(report.unrecoverable_files, 0, "{report:?}");
        assert!(report.errors.is_empty(), "{report:?}");
    }
    let after = fs.metadata_snapshot().inodes[&ino].clone();
    assert_eq!(after.size, large_size);
    assert_eq!(after.mtime, before.mtime);
    assert_eq!(after.blocks.len(), before.blocks.len());
    assert_eq!(
        fs.read_inode(ino, 0, payload.len(), false).unwrap(),
        payload
    );
    assert_eq!(
        fs.read_inode(ino, large_size - 16, 16, false).unwrap(),
        [0; 16]
    );
}

#[test]
fn fsck_repairs_only_damaged_stripes_in_large_file() {
    let (_dir, fs) = redundant_host_volume();
    let payload = vec![0x36; 16_384];
    fs.write_file("/large", &payload, 0o600).unwrap();
    let ino = fs.resolve_path("/large", true).unwrap();
    let large_size = MAX_IN_MEMORY_IO_BYTES as u64 + 4096;
    fs.truncate_inode(ino, large_size).unwrap();
    let before = fs.metadata_snapshot().inodes[&ino].clone();
    assert_eq!(before.blocks.len(), 2);
    let shard = &before.blocks[0].shards[0];
    let path = fs.shard_path_locked(&fs.metadata_snapshot(), &shard.disk_id, &shard.relpath);
    std::fs::write(&path, b"corrupted shard").unwrap();

    let checked = fs.fsck(false, false).unwrap();
    assert_eq!(checked.files_checked, 1, "{checked:?}");
    assert_eq!(checked.damaged_files, 1, "{checked:?}");
    assert_eq!(checked.unrecoverable_files, 0, "{checked:?}");
    assert!(checked.checksum_errors > 0, "{checked:?}");
    assert_eq!(checked.repaired_files, 0, "{checked:?}");

    let repaired = fs.fsck(true, false).unwrap();
    assert_eq!(repaired.damaged_files, 1, "{repaired:?}");
    assert_eq!(repaired.repaired_files, 1, "{repaired:?}");
    assert_eq!(repaired.unrecoverable_files, 0, "{repaired:?}");
    let after = fs.metadata_snapshot().inodes[&ino].clone();
    assert_eq!(after.size, large_size);
    assert_eq!(after.mtime, before.mtime, "fsck must preserve file mtime");
    assert_eq!(after.blocks.len(), 2);
    assert_ne!(after.blocks[0].stripe_id, before.blocks[0].stripe_id);
    assert_eq!(after.blocks[1].stripe_id, before.blocks[1].stripe_id);
    assert_eq!(
        fs.read_inode(ino, 0, payload.len(), false).unwrap(),
        payload
    );
    assert_eq!(
        fs.read_inode(ino, large_size - 16, 16, false).unwrap(),
        [0; 16]
    );
    let clean = fs.fsck(false, false).unwrap();
    assert_eq!(clean.damaged_files, 0, "{clean:?}");
    assert_eq!(clean.unrecoverable_files, 0, "{clean:?}");
}

#[test]
fn fsck_reports_genuinely_unrecoverable_large_file_without_replacing_data() {
    let (_dir, fs) = host_volume(1);
    fs.write_file("/large", &[0x3a; 8192], 0o600).unwrap();
    let ino = fs.resolve_path("/large", true).unwrap();
    fs.truncate_inode(ino, MAX_IN_MEMORY_IO_BYTES as u64 + 1)
        .unwrap();
    let before = fs.metadata_snapshot().inodes[&ino].clone();
    let shard = &before.blocks[0].shards[0];
    let path = fs.shard_path_locked(&fs.metadata_snapshot(), &shard.disk_id, &shard.relpath);
    std::fs::write(&path, b"corrupted shard").unwrap();

    let report = fs.fsck(true, false).unwrap();
    assert_eq!(report.damaged_files, 1, "{report:?}");
    assert_eq!(report.unrecoverable_files, 1, "{report:?}");
    assert_eq!(report.repaired_files, 0, "{report:?}");
    assert!(report.errors.iter().any(|error| error.contains("inode")));
    let after = fs.metadata_snapshot().inodes[&ino].clone();
    assert_eq!(after.blocks[0].stripe_id, before.blocks[0].stripe_id);
    assert_eq!(after.size, before.size);
}

#[test]
fn fsck_uses_current_inode_after_initial_id_scan() {
    let (_dir, fs) = redundant_host_volume();
    fs.write_file("/file", &[0x45; 8192], 0o600).unwrap();
    let ino = fs.resolve_path("/file", true).unwrap();
    // A completed writer can extend the file after fsck collects its IDs.
    let inode_ids = fs.metadata_snapshot().inodes.keys().copied().collect();
    let extension = b"written after the inode ID scan";
    fs.write_inode_range(ino, 8192, extension).unwrap();
    let current = fs.metadata_snapshot().inodes[&ino].clone();
    let shard = &current.blocks[0].shards[0];
    let path = fs.shard_path_locked(&fs.metadata_snapshot(), &shard.disk_id, &shard.relpath);
    std::fs::write(&path, b"corrupt").unwrap();

    let report = fs.fsck_inodes(inode_ids, true, false).unwrap();
    assert_eq!(report.damaged_files, 1, "{report:?}");
    assert_eq!(report.repaired_files, 1, "{report:?}");
    assert_eq!(report.unrecoverable_files, 0, "{report:?}");
    let after = fs.metadata_snapshot().inodes[&ino].clone();
    assert_eq!(after.size, current.size);
    assert_eq!(after.blocks[1].stripe_id, current.blocks[1].stripe_id);
    assert_eq!(
        fs.read_inode(ino, 8192, extension.len(), false).unwrap(),
        extension
    );
}

#[test]
fn fsck_rejects_invalid_inline_checksum_and_length() {
    for corrupt_length in [false, true] {
        let (_dir, fs) = host_volume(1);
        fs.write_file("/small", b"inline-data", 0o600).unwrap();
        let ino = fs.resolve_path("/small", true).unwrap();
        {
            let mut meta = fs.meta.write();
            let inode = meta.inodes.get_mut(&ino).unwrap();
            inode.blocks.clear();
            inode.inline_data = Some(b"inline-data".to_vec());
            inode.inline_sha256 = content_hash_hex(b"inline-data");
            if corrupt_length {
                inode.size += 1;
            } else {
                inode.inline_sha256 = "bad checksum".to_string();
            }
        }
        for repair in [false, true] {
            let report = fs.fsck(repair, false).unwrap();
            assert_eq!(report.files_checked, 1, "{report:?}");
            assert_eq!(report.damaged_files, 1, "{report:?}");
            assert_eq!(report.unrecoverable_files, 1, "{report:?}");
            assert_eq!(report.repaired_files, 0, "{report:?}");
            assert!(report
                .errors
                .iter()
                .any(|error| error.contains("inline inode")));
        }
    }
}

#[test]
fn fsck_reports_unreadable_stripe_during_offline_grace_without_repair() {
    let _guard = env_lock();
    std::env::set_var("ARGOSFS_OFFLINE_GRACE_MS", "600000");
    let (_dir, fs) = redundant_host_volume();
    fs.write_file("/file", &[0x42; 8192], 0o600).unwrap();
    let ino = fs.resolve_path("/file", true).unwrap();
    let before = fs.metadata_snapshot().inodes[&ino].clone();
    let offline_disks = before.blocks[0]
        .shards
        .iter()
        .take(2)
        .map(|shard| shard.disk_id.clone())
        .collect::<Vec<_>>();
    assert_eq!(offline_disks.len(), 2);
    {
        let mut meta = fs.meta.write();
        for disk_id in offline_disks {
            let disk = meta.disks.get_mut(&disk_id).unwrap();
            disk.status = DiskStatus::Offline;
            disk.offline_since = Some(now_f64());
        }
    }
    for repair in [false, true] {
        let report = fs.fsck(repair, false).unwrap();
        assert_eq!(report.files_checked, 1, "{report:?}");
        assert_eq!(report.damaged_files, 1, "{report:?}");
        assert_eq!(report.missing_shards, 2, "{report:?}");
        assert_eq!(report.unrecoverable_files, 1, "{report:?}");
        assert_eq!(report.repaired_files, 0, "{report:?}");
        assert!(report
            .errors
            .iter()
            .any(|error| error.contains("verified shards available")));
    }
    let after = fs.metadata_snapshot().inodes[&ino].clone();
    assert_eq!(after.blocks[0].stripe_id, before.blocks[0].stripe_id);
    std::env::remove_var("ARGOSFS_OFFLINE_GRACE_MS");
}
