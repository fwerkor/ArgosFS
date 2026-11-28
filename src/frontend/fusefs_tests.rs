use super::*;
use crate::types::{DiskStatus, VolumeConfig};
use crate::volume::capacity::{
    fallback_statfs_capacity, max_stripe_units, statfs_bytes, statvfs_capacity,
};

#[test]
fn kernel_killpriv_only_clears_setid_bits() {
    assert!(is_setid_clear_only(0o104777, 0o100777));
    assert!(is_setid_clear_only(0o102777, 0o100777));
    assert!(is_setid_clear_only(0o106777, 0o100777));
    assert!(!is_setid_clear_only(0o100777, 0o100777));
    assert!(!is_setid_clear_only(0o104777, 0o100755));
    assert!(!is_setid_clear_only(0o104777, 0o104755));
}

#[test]
fn explicit_subsecond_timestamps_round_trip_at_microsecond_precision() {
    for (seconds, nanos) in [
        (100_000_000, 100_000_000),
        (200_000_000, 200_000_000),
        (1_700_000_000, 999_999_000),
    ] {
        let time = UNIX_EPOCH + Duration::new(seconds, nanos);
        let encoded = system_time_to_f64(time);
        let decoded = f64_to_system_time(encoded)
            .duration_since(UNIX_EPOCH)
            .unwrap();
        assert_eq!(decoded.as_secs(), seconds);
        assert_eq!(decoded.subsec_nanos(), nanos);
    }
}

#[test]
fn allow_other_sets_fuser_session_acl() {
    let config = mount_config(vec![
        MountOption::FSName("argosfs".to_string()),
        MountOption::Subtype("argosfs".to_string()),
        MountOption::CUSTOM("allow_other".to_string()),
        MountOption::CUSTOM("default_permissions".to_string()),
    ]);

    assert_eq!(config.acl, SessionACL::All);
    assert!(config
        .mount_options
        .contains(&MountOption::DefaultPermissions));
    assert!(!config
        .mount_options
        .contains(&MountOption::CUSTOM("allow_other".to_string())));
}

#[test]
fn allow_other_wins_over_allow_root() {
    let config = mount_config(vec![
        MountOption::CUSTOM("allow_root".to_string()),
        MountOption::CUSTOM("allow_other".to_string()),
    ]);

    assert_eq!(config.acl, SessionACL::All);
}

#[test]
fn mounts_dispatch_multiple_requests_with_independent_linux_fuse_channels() {
    let config = mount_config(vec![MountOption::CUSTOM("ro".to_string())]);
    #[cfg(target_os = "linux")]
    {
        assert_eq!(
            config.n_threads,
            Some(fuse_thread_count("ARGOSFS_FUSE_REQUEST_THREADS", 2, 8))
        );
        assert!(config.n_threads.unwrap() >= 2);
        assert!(config.clone_fd);
    }
    #[cfg(not(target_os = "linux"))]
    {
        assert_eq!(config.n_threads, None);
        assert!(!config.clone_fd);
    }
    assert!(config.mount_options.contains(&MountOption::RO));
}

#[test]
fn proc_status_group_parser_reads_supplementary_groups() {
    let status =
        "Name:\ttest\nUid:\t1000 1000 1000 1000\nGid:\t2000 2000 2000 2000\nGroups:\t10 20 30 \n";
    let groups = parse_status_groups_for_identity(status, 1000, 2000).unwrap();
    assert_eq!(groups, vec![10, 20, 30]);
    assert!(parse_status_groups_for_identity(status, 1001, 2000).is_none());
    assert!(parse_status_groups_for_identity(status, 1000, 2001).is_none());
}

#[test]
fn missing_xattr_maps_to_enodata_and_reports_absent() {
    let err = ArgosError::NotFound("xattr security.selinux".to_string());
    assert_eq!(xattr_errno(&err).code(), Errno::NO_XATTR.code());
    assert!(!xattr_exists(Err(err)).unwrap());
}

#[test]
fn missing_inode_keeps_enoent_for_xattr_operations() {
    let err = ArgosError::NotFound("inode 42".to_string());
    assert_eq!(xattr_errno(&err).code(), Errno::ENOENT.code());
    assert!(matches!(
        xattr_exists(Err(err)),
        Err(ArgosError::NotFound(_))
    ));
}

#[test]
fn dirty_extent_merges_adjacent_and_overlapping_writes() {
    let mut dirty = DirtyExtent {
        gaps: Vec::new(),
        offset: 10,
        data: b"abcd".to_vec(),
        written_at: 1.0,
        seen_ctime: None,
    };

    assert!(dirty.try_merge(14, b"ef", 16));
    assert_eq!(dirty.offset, 10);
    assert_eq!(dirty.data, b"abcdef");

    assert!(dirty.try_merge(12, b"XY", 16));
    assert_eq!(dirty.offset, 10);
    assert_eq!(dirty.data, b"abXYef");
}

#[test]
fn dirty_extent_reuses_reserved_capacity_across_sequential_writes() {
    let mut data = Vec::with_capacity(128 * 1024);
    data.extend_from_slice(b"abcd");
    let mut dirty = DirtyExtent {
        gaps: Vec::new(),
        offset: 64,
        data,
        written_at: 1.0,
        seen_ctime: None,
    };
    let initial_ptr = dirty.data.as_ptr();
    for index in 0..64 {
        let bytes = vec![(index % 251) as u8; 1024];
        assert!(dirty.try_merge(68 + (index * 1024) as u64, &bytes, 128 * 1024));
        assert_eq!(
            dirty.data.as_ptr(),
            initial_ptr,
            "unexpected full-buffer reallocation"
        );
    }
    assert!(dirty.try_merge(66, b"ZZ", 128 * 1024));
    assert_eq!(dirty.data.as_ptr(), initial_ptr);
    assert_eq!(&dirty.data[..4], b"abZZ");
    assert_eq!(dirty.offset, 64);
    assert_eq!(dirty.data.len(), 4 + 64 * 1024);
    assert!(!dirty.try_merge(500_000, b"gap", 128 * 1024));
}

#[test]
fn dirty_extent_prepend_and_overlapping_extension_preserve_last_write() {
    let mut dirty = DirtyExtent {
        gaps: Vec::new(),
        offset: 10,
        data: b"34567".to_vec(),
        written_at: 1.0,
        seen_ctime: None,
    };
    assert!(dirty.try_merge(8, b"1234", 20));
    assert_eq!(dirty.offset, 8);
    assert_eq!(dirty.data, b"1234567");
    assert!(dirty.try_merge(12, b"XYZ890", 20));
    assert_eq!(dirty.data, b"1234XYZ890");
}

#[test]
fn dirty_extent_rejects_gaps_and_oversized_merges() {
    let mut dirty = DirtyExtent {
        gaps: Vec::new(),
        offset: 10,
        data: b"abcd".to_vec(),
        written_at: 1.0,
        seen_ctime: None,
    };

    assert!(!dirty.try_merge(15, b"z", 16));
    assert_eq!(dirty.offset, 10);
    assert_eq!(dirty.data, b"abcd");

    assert!(!dirty.try_merge(14, b"efgh", 6));
    assert_eq!(dirty.offset, 10);
    assert_eq!(dirty.data, b"abcd");
}

#[test]
fn stripe_capacity_respects_distinct_device_constraints() {
    assert_eq!(max_stripe_units(&[100, 100, 100], 3), 100);
    assert_eq!(max_stripe_units(&[300, 10, 10], 3), 10);
    assert_eq!(max_stripe_units(&[100, 100, 100, 100, 100, 100], 3), 200);
    assert_eq!(max_stripe_units(&[100, 100], 3), 0);
}

#[test]
fn raw_statfs_uses_allocator_regions_and_free_extents() {
    let tmp = tempfile::tempdir().unwrap();
    let images = (0..3)
        .map(|index| tmp.path().join(format!("disk-{index}.img")))
        .collect::<Vec<_>>();
    let volume = ArgosFs::create_loop(
        &images,
        VolumeConfig {
            k: 2,
            m: 1,
            chunk_size: 4096,
            ..VolumeConfig::default()
        },
        32 * 1024 * 1024,
        "statfs",
        false,
    )
    .unwrap();
    let before = statfs_bytes(&volume.metadata_snapshot(), volume.root(), &BTreeSet::new());
    volume
        .write_file("/payload", &vec![7u8; 64 * 1024], 0o644)
        .unwrap();
    let after = statfs_bytes(&volume.metadata_snapshot(), volume.root(), &BTreeSet::new());
    assert!(before.0 > 0);
    assert!(before.0 < 3 * 32 * 1024 * 1024);
    assert!(after.1 < before.1);
    assert!(after.1 <= after.0);

    volume.mark_disk("disk-0000", DiskStatus::Degraded).unwrap();
    let degraded = statfs_bytes(&volume.metadata_snapshot(), volume.root(), &BTreeSet::new());
    assert_eq!(degraded.0, after.0);
    assert_eq!(degraded.1, 0);

    let quarantined = BTreeSet::from(["disk-0000".to_string()]);
    let unavailable = statfs_bytes(&volume.metadata_snapshot(), volume.root(), &quarantined);
    assert_eq!(unavailable, (0, 0));
}

#[test]
fn statfs_advances_after_deferred_reclamation_commit() {
    let tmp = tempfile::tempdir().unwrap();
    let images = (0..3)
        .map(|index| tmp.path().join(format!("disk-{index}.img")))
        .collect::<Vec<_>>();
    let volume = ArgosFs::create_loop(
        &images,
        VolumeConfig {
            k: 2,
            m: 1,
            chunk_size: 4096,
            defer_metadata_commit: true,
            deferred_commit_interval_ms: 3_600_000,
            ..VolumeConfig::default()
        },
        32 * 1024 * 1024,
        "statfs-publish",
        false,
    )
    .unwrap();
    volume
        .write_file("/payload", &vec![7u8; 64 * 1024], 0o644)
        .unwrap();
    volume.sync().unwrap();
    let before_delete = volume
        .published_capacity_snapshot()
        .expect("raw pools publish capacity");

    volume.unlink_path("/payload").unwrap();
    let before_commit = volume
        .published_capacity_snapshot()
        .expect("raw pools publish capacity");
    assert_eq!(
        before_commit.free_bytes, before_delete.free_bytes,
        "deferred reclamation must not publish space before its commit"
    );

    volume.sync().unwrap();
    let after_commit = volume
        .published_capacity_snapshot()
        .expect("raw pools publish capacity");
    assert!(
        after_commit.free_bytes > before_commit.free_bytes,
        "deleted extents must be republished after a durable commit: {} -> {}",
        before_commit.free_bytes,
        after_commit.free_bytes
    );

    // The FUSE statfs path must observe the republished value even while a
    // checkpoint holds the metadata writer lock.
    let fuse = ArgosFuse::new(volume.clone());
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        volume.test_with_metadata_write_lock(|| {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
    });
    started_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let started = std::time::Instant::now();
    let snapshot = fuse.statfs_snapshot();
    let elapsed = started.elapsed();
    release_tx.send(()).unwrap();
    writer.join().unwrap();
    assert_eq!(snapshot.free_bytes, after_commit.free_bytes);
    assert!(
        elapsed < Duration::from_millis(200),
        "statfs blocked during metadata write lock: {elapsed:?}"
    );
}

#[test]
fn host_statfs_subtracts_logical_usage_after_erasure_scaling() {
    let tmp = tempfile::tempdir().unwrap();
    let volume = ArgosFs::create(
        tmp.path(),
        VolumeConfig {
            k: 2,
            m: 2,
            compression: crate::types::Compression::None,
            ..VolumeConfig::default()
        },
        4,
        false,
    )
    .unwrap();
    let before = statfs_bytes(&volume.metadata_snapshot(), volume.root(), &BTreeSet::new());
    let payload = vec![7u8; 64 * 1024];
    volume.write_file("/payload", &payload, 0o644).unwrap();
    let after = statfs_bytes(&volume.metadata_snapshot(), volume.root(), &BTreeSet::new());

    assert_eq!(before.1.saturating_sub(after.1), payload.len() as u64);
}

#[test]
fn file_handles_track_independent_open_references() {
    let mut handles = FuseHandles::default();
    let first = handles.open(OpenFileHandle {
        ino: 42,
        flags: libc::O_RDONLY,
    });
    let second = handles.open(OpenFileHandle {
        ino: 42,
        flags: libc::O_SYNC | libc::O_WRONLY,
    });
    assert_ne!(first, second);
    assert_eq!(handles.refs(42), 2);
    assert_eq!(
        handles.get(second).unwrap().flags & libc::O_SYNC,
        libc::O_SYNC
    );
    assert!(!handles.close(first).unwrap().1);
    assert!(handles.close(second).unwrap().1);
    assert_eq!(handles.refs(42), 0);
}

#[test]
fn fuse_writeback_flushes_merged_extent_to_volume() {
    let tmp = tempfile::tempdir().unwrap();
    let volume = ArgosFs::create(
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
    let ino = volume.create_file_path("/buffered", 0o644).unwrap();
    let fuse = ArgosFuse::new(volume.clone());

    assert!(fuse.queue_writeback(ino, 0, b"hello"));
    assert!(fuse.queue_writeback(ino, 5, b" world"));
    assert_eq!(volume.read_file("/buffered", true).unwrap(), b"");

    fuse.flush_inode_writeback(ino).unwrap();

    assert_eq!(volume.read_file("/buffered", true).unwrap(), b"hello world");
    assert!(fuse.writeback.lock().dirty.is_empty());
}

#[test]
fn buffered_write_is_rejected_on_a_readonly_volume() {
    let tmp = tempfile::tempdir().unwrap();
    let images = (0..3)
        .map(|index| tmp.path().join(format!("readonly-write-{index}.img")))
        .collect::<Vec<_>>();
    let volume = ArgosFs::create_loop(
        &images,
        VolumeConfig {
            k: 2,
            m: 1,
            chunk_size: 4096,
            compression: crate::types::Compression::None,
            ..VolumeConfig::default()
        },
        32 * 1024 * 1024,
        "readonly-write",
        false,
    )
    .unwrap();
    volume.write_file("/file", b"persisted", 0o600).unwrap();
    volume.sync().unwrap();
    drop(volume);

    let readonly = ArgosFs::open_loop(&images, false).unwrap();
    let ino = readonly.resolve_path("/file", true).unwrap();
    let fuse = ArgosFuse::new(readonly.clone());
    assert!(matches!(
        fuse.queue_writeback_staged(ino, 0, b"must-not-be-acknowledged"),
        Err(ArgosError::ReadonlyRequired(_))
    ));
    assert!(fuse.writeback.lock().dirty.is_empty());

    // Closing a normally linked file must not report EROFS merely because the
    // mount is read-only.
    readonly.reap_unlinked_inode(ino).unwrap();
}

#[test]
fn fuse_writeback_uses_the_successfully_opened_handle_after_mode_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let volume = ArgosFs::create(
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
    let attr = volume
        .create_file_at_with_owner(1, OsStr::new("group-write"), 0o620, 1000, 2000)
        .unwrap();
    assert!(volume
        .check_access_inode_with_groups(attr.ino, 3000, &[3000, 2000], libc::W_OK)
        .is_ok());
    let fuse = ArgosFuse::new(volume.clone());
    assert!(fuse.queue_writeback(attr.ino, 0, b"accepted"));
    volume.chmod_inode(attr.ino, 0).unwrap();

    fuse.flush_inode_writeback(attr.ino).unwrap();

    assert_eq!(
        volume.read_inode(attr.ino, 0, 64, false).unwrap(),
        b"accepted"
    );
}

#[test]
fn truncate_uses_the_successfully_opened_writable_handle_after_mode_changes() {
    let tmp = tempfile::tempdir().unwrap();
    let volume = ArgosFs::create(
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
    let ino = volume.create_file_path("/truncate", 0o600).unwrap();
    volume.write_inode_range(ino, 0, b"payload").unwrap();
    let fuse = ArgosFuse::new(volume.clone());
    let writable = fuse.handles.lock().open(OpenFileHandle {
        ino,
        flags: libc::O_WRONLY,
    });
    let read_only = fuse.handles.lock().open(OpenFileHandle {
        ino,
        flags: libc::O_RDONLY,
    });
    volume.chmod_inode(ino, 0).unwrap();

    fuse.require_writable_handle(INodeNo(ino), writable)
        .unwrap();
    assert_eq!(
        fuse.require_writable_handle(INodeNo(ino), read_only)
            .unwrap_err()
            .errno(),
        libc::EACCES
    );
    assert_eq!(
        fuse.require_writable_handle(INodeNo(ino + 1), writable)
            .unwrap_err()
            .errno(),
        libc::EINVAL
    );

    volume.truncate_inode_as(ino, 2).unwrap();
    assert_eq!(volume.read_inode(ino, 0, 8, false).unwrap(), b"pa");
}

#[test]
fn successful_retry_reaps_an_unlinked_inode_after_writeback() {
    let tmp = tempfile::tempdir().unwrap();
    let volume = ArgosFs::create(
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
    let ino = volume.create_file_path("/orphan", 0o644).unwrap();
    let fuse = ArgosFuse::new(volume.clone());
    let handle = fuse.handles.lock().open(OpenFileHandle {
        ino,
        flags: libc::O_WRONLY,
    });
    volume
        .unlink_at_as_preserving_open(1, OsStr::new("orphan"), 0)
        .unwrap();
    assert!(fuse.queue_writeback(ino, 0, b"pending"));
    assert!(fuse.handles.lock().close(handle).unwrap().1);
    fuse.writeback.lock().reap_after_flush.insert(ino);

    fuse.flush_inode_writeback(ino).unwrap();

    assert!(matches!(
        volume.attr_inode(ino),
        Err(ArgosError::NotFound(_))
    ));
}

#[test]
fn periodic_worker_commits_idle_deferred_metadata() {
    let tmp = tempfile::tempdir().unwrap();
    let images = vec![tmp.path().join("disk.img")];
    let volume = ArgosFs::create_loop(
        &images,
        VolumeConfig {
            k: 1,
            m: 0,
            chunk_size: 1024,
            compression: crate::types::Compression::None,
            defer_journal_flush: true,
            defer_metadata_commit: true,
            defer_data_flush: true,
            deferred_commit_interval_ms: 20,
            deferred_commit_max_transactions: 1_000,
            ..VolumeConfig::default()
        },
        32 * 1024 * 1024,
        "periodic-commit",
        false,
    )
    .unwrap();
    let fuse = ArgosFuse::new(volume.clone());
    volume
        .write_file("/periodic", &vec![b'p'; 2048], 0o644)
        .unwrap();
    let expected_txid = volume.metadata_snapshot().txid;

    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        let report = volume.transaction_report().unwrap();
        if report.last_valid_txid == expected_txid {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "periodic commit timed out"
        );
        std::thread::sleep(Duration::from_millis(10));
    }

    drop(fuse);
    drop(volume);
    let reopened = ArgosFs::open_loop(&images, false).unwrap();
    assert_eq!(
        reopened.read_file("/periodic", true).unwrap(),
        vec![b'p'; 2048]
    );
}

fn node_attr(kind: NodeKind, mode: u32) -> NodeAttr {
    NodeAttr {
        ino: 7,
        kind,
        mode,
        uid: 1000,
        gid: 1001,
        nlink: 2,
        size: 123,
        rdev: 0x1234,
        atime: 10.25,
        mtime: 11.5,
        ctime: 12.75,
        blocks: 8,
        blksize: 4096,
    }
}

#[test]
fn open_file_handle_access_modes_match_posix_flags() {
    let read_only = OpenFileHandle {
        ino: 1,
        flags: libc::O_RDONLY,
    };
    let write_only = OpenFileHandle {
        ino: 1,
        flags: libc::O_WRONLY,
    };
    let read_write = OpenFileHandle {
        ino: 1,
        flags: libc::O_RDWR,
    };
    assert!(read_only.can_read());
    assert!(!read_only.can_write());
    assert!(!write_only.can_read());
    assert!(write_only.can_write());
    assert!(read_write.can_read());
    assert!(read_write.can_write());
}

#[test]
fn handle_allocator_skips_collisions_and_tolerates_missing_ref_counts() {
    let mut handles = FuseHandles {
        next: u64::MAX,
        ..FuseHandles::default()
    };
    handles.files.insert(
        1,
        OpenFileHandle {
            ino: 10,
            flags: libc::O_RDONLY,
        },
    );
    let handle = handles.open(OpenFileHandle {
        ino: 20,
        flags: libc::O_RDWR,
    });
    assert_eq!(handle, FileHandle(2));
    assert!(handles.close(FileHandle(999)).is_none());
    handles.refs.remove(&20);
    assert!(handles.close(handle).unwrap().1);
}

#[test]
fn dirty_extent_checked_arithmetic_and_queue_limits_are_enforced() {
    let mut overflowing_existing = DirtyExtent {
        gaps: Vec::new(),
        offset: u64::MAX,
        data: vec![1],
        written_at: 1.0,
        seen_ctime: None,
    };
    assert!(overflowing_existing.end().is_none());
    assert!(!overflowing_existing.try_merge(0, b"x", usize::MAX));

    let mut normal = DirtyExtent {
        gaps: Vec::new(),
        offset: 0,
        data: vec![1],
        written_at: 1.0,
        seen_ctime: None,
    };
    assert!(!normal.try_merge(u64::MAX, b"xx", usize::MAX));

    let tmp = tempfile::tempdir().unwrap();
    let volume = ArgosFs::create(
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
    let fuse = ArgosFuse::new(volume);
    assert!(!fuse.queue_writeback(2, 0, &vec![0; FUSE_WRITEBACK_MAX_BYTES + 1]));
    assert!(fuse.queue_writeback(2, 0, b"a"));
    assert!(!fuse.queue_writeback(2, 2, b"b"));
}

#[test]
fn empty_writeback_flushes_are_noops() {
    let tmp = tempfile::tempdir().unwrap();
    let volume = ArgosFs::create(
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
    let fuse = ArgosFuse::new(volume);
    fuse.flush_inode_writeback(999).unwrap();
    fuse.flush_all_writeback().unwrap();
}

#[test]
fn best_effort_writeback_does_not_poison_unrelated_inodes() {
    let tmp = tempfile::tempdir().unwrap();
    let volume = ArgosFs::create(
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
    let healthy = volume.create_file_path("/healthy", 0o644).unwrap();
    let root = volume.resolve_path("/", true).unwrap();
    let fuse = ArgosFuse::new(volume.clone());

    assert!(fuse.queue_writeback(root, 0, b"cannot-write-a-directory"));
    assert!(fuse.queue_writeback(healthy, 0, b"still-readable"));
    fuse.flush_all_writeback_best_effort();

    assert_eq!(
        volume.read_file("/healthy", true).unwrap(),
        b"still-readable"
    );
    let writeback = fuse.writeback.lock();
    assert!(writeback.dirty.contains_key(&root));
    assert!(!writeback.dirty.contains_key(&healthy));
}

#[test]
fn lookup_retries_target_writeback_without_blocking_namespace_resolution() {
    let tmp = tempfile::tempdir().unwrap();
    let volume = ArgosFs::create(
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
    let target = volume.create_file_path("/target", 0o644).unwrap();
    let bad_dir = volume.mkdir("/bad-dir", 0o755).unwrap();
    let root = volume.resolve_path("/", true).unwrap();
    let fuse = ArgosFuse::new(volume.clone());

    assert!(fuse.queue_writeback(bad_dir, 0, b"cannot-write-a-directory"));
    assert_eq!(
        fuse.lookup_after_access(root, OsStr::new("target"))
            .unwrap()
            .ino,
        target
    );
    assert!(fuse.writeback.lock().dirty.contains_key(&bad_dir));

    assert_eq!(
        fuse.lookup_after_access(root, OsStr::new("bad-dir"))
            .unwrap()
            .ino,
        bad_dir
    );
    assert!(fuse.writeback.lock().dirty.contains_key(&bad_dir));
}

#[test]
fn unlink_ignores_unrelated_failed_writeback() {
    let tmp = tempfile::tempdir().unwrap();
    let volume = ArgosFs::create(
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
    let target = volume.create_file_path("/target", 0o644).unwrap();
    let root = volume.resolve_path("/", true).unwrap();
    let fuse = ArgosFuse::new(volume.clone());
    assert!(fuse.queue_writeback(root, 0, b"cannot-write-a-directory"));

    fuse.unlink_after_access(root, OsStr::new("target"), unsafe { libc::geteuid() })
        .unwrap();

    assert!(matches!(
        volume.lookup(root, OsStr::new("target")),
        Err(ArgosError::NotFound(_))
    ));
    assert!(matches!(
        volume.attr_inode(target),
        Err(ArgosError::NotFound(_))
    ));
    assert!(fuse.writeback.lock().dirty.contains_key(&root));
}

#[test]
fn rejected_unlink_retains_failed_target_writeback() {
    let tmp = tempfile::tempdir().unwrap();
    let volume = ArgosFs::create(
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
    let dir = volume.mkdir("/dir", 0o755).unwrap();
    let root = volume.resolve_path("/", true).unwrap();
    let fuse = ArgosFuse::new(volume.clone());
    assert!(fuse.queue_writeback(dir, 0, b"invalid-directory-writeback"));

    assert!(fuse
        .unlink_after_access(root, OsStr::new("dir"), unsafe { libc::geteuid() })
        .is_err());

    assert!(volume.lookup(root, OsStr::new("dir")).is_ok());
    assert!(fuse.writeback.lock().dirty.contains_key(&dir));
}

#[test]
fn unlink_one_hard_link_retains_failed_target_writeback() {
    let tmp = tempfile::tempdir().unwrap();
    let volume = ArgosFs::create(
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
    let ino = volume.create_file_path("/primary", 0o644).unwrap();
    let root = volume.resolve_path("/", true).unwrap();
    volume.link_at(ino, root, OsStr::new("alias")).unwrap();
    assert_eq!(volume.attr_inode(ino).unwrap().nlink, 2);
    let fuse = ArgosFuse::new(volume.clone());

    assert!(fuse.queue_writeback(ino, u64::MAX, b"pending"));
    assert!(fuse.flush_inode_writeback(ino).is_err());
    fuse.unlink_after_access(root, OsStr::new("primary"), unsafe { libc::geteuid() })
        .unwrap();

    assert!(matches!(
        volume.lookup(root, OsStr::new("primary")),
        Err(ArgosError::NotFound(_))
    ));
    assert_eq!(volume.lookup(root, OsStr::new("alias")).unwrap().ino, ino);
    assert_eq!(volume.attr_inode(ino).unwrap().nlink, 1);
    assert!(fuse.writeback.lock().dirty.contains_key(&ino));
}

#[test]
fn unlink_preserves_inode_while_open_handle_exists() {
    let tmp = tempfile::tempdir().unwrap();
    let volume = ArgosFs::create(
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
    let ino = volume.create_file_path("/open", 0o644).unwrap();
    let root = volume.resolve_path("/", true).unwrap();
    let fuse = ArgosFuse::new(volume.clone());
    let handle = fuse.handles.lock().open(OpenFileHandle {
        ino,
        flags: libc::O_RDONLY,
    });

    fuse.unlink_after_access(root, OsStr::new("open"), unsafe { libc::geteuid() })
        .unwrap();

    assert!(matches!(
        volume.lookup(root, OsStr::new("open")),
        Err(ArgosError::NotFound(_))
    ));
    assert!(volume.attr_inode(ino).is_ok());
    assert!(fuse.handles.lock().close(handle).unwrap().1);
    volume.reap_unlinked_inode(ino).unwrap();
    assert!(matches!(
        volume.attr_inode(ino),
        Err(ArgosError::NotFound(_))
    ));
}

#[test]
fn mount_option_normalization_covers_known_and_custom_values() {
    let cases = [
        ("auto_unmount", MountOption::AutoUnmount),
        ("default_permissions", MountOption::DefaultPermissions),
        ("dev", MountOption::Dev),
        ("nodev", MountOption::NoDev),
        ("suid", MountOption::Suid),
        ("nosuid", MountOption::NoSuid),
        ("ro", MountOption::RO),
        ("rw", MountOption::RW),
        ("exec", MountOption::Exec),
        ("noexec", MountOption::NoExec),
        ("atime", MountOption::Atime),
        ("noatime", MountOption::NoAtime),
        ("dirsync", MountOption::DirSync),
        ("sync", MountOption::Sync),
        ("async", MountOption::Async),
    ];
    for (raw, expected) in cases {
        assert_eq!(
            normalize_mount_option(MountOption::CUSTOM(raw.to_string())),
            expected
        );
    }
    assert_eq!(
        normalize_mount_option(MountOption::CUSTOM("fsname=custom".to_string())),
        MountOption::FSName("custom".to_string())
    );
    assert_eq!(
        normalize_mount_option(MountOption::CUSTOM("subtype=custom".to_string())),
        MountOption::Subtype("custom".to_string())
    );
    assert_eq!(
        normalize_mount_option(MountOption::CUSTOM("unknown=value".to_string())),
        MountOption::CUSTOM("unknown=value".to_string())
    );
    assert_eq!(normalize_mount_option(MountOption::RO), MountOption::RO);

    let config = mount_config(vec![MountOption::CUSTOM("allow_root".to_string())]);
    assert_eq!(config.acl, SessionACL::RootAndOwner);
}

#[test]
fn xattr_namespace_and_name_helpers_cover_supported_forms() {
    for name in [
        acl::POSIX_ACL_ACCESS_XATTR,
        acl::POSIX_ACL_DEFAULT_XATTR,
        acl::ARGOS_POSIX_ACL_ACCESS_XATTR,
        acl::ARGOS_POSIX_ACL_DEFAULT_XATTR,
    ] {
        assert!(is_owner_managed_xattr(name));
    }
    assert!(!is_owner_managed_xattr("user.example"));
    assert_eq!(
        xattr_name(OsStr::new("user.example")).unwrap(),
        "user.example"
    );
    use std::os::unix::ffi::OsStrExt;
    let invalid = OsStr::from_bytes(b"user.\xff");
    assert!(matches!(xattr_name(invalid), Err(ArgosError::Invalid(_))));
}

#[test]
fn statvfs_fallback_deduplicates_filesystems_and_ignores_bad_paths() {
    let tmp = tempfile::tempdir().unwrap();
    let same_fs = tmp.path().join("child");
    std::fs::create_dir(&same_fs).unwrap();
    let (single_capacity, single_free) = fallback_statfs_capacity(tmp.path(), []);
    let (deduplicated_capacity, deduplicated_free) =
        fallback_statfs_capacity(tmp.path(), [same_fs, tmp.path().join("missing")]);
    assert!(single_capacity > 0);
    assert_eq!(deduplicated_capacity, single_capacity);
    assert!(deduplicated_free.abs_diff(single_free) < 1024 * 1024);

    use std::os::unix::ffi::OsStrExt;
    assert!(statvfs_capacity(Path::new(OsStr::from_bytes(b"bad\0path"))).is_none());
    assert!(statvfs_capacity(&tmp.path().join("missing")).is_none());
}

#[test]
fn file_attribute_conversion_covers_all_node_and_special_types() {
    let directory = to_file_attr(&node_attr(NodeKind::Directory, libc::S_IFDIR | 0o755));
    assert_eq!(directory.kind, FileType::Directory);
    assert_eq!(directory.ino, INodeNo(7));
    assert_eq!(directory.perm, 0o755);
    assert_eq!(directory.uid, 1000);
    assert_eq!(directory.gid, 1001);
    assert_eq!(directory.size, 123);
    assert_eq!(directory.blocks, 8);

    assert_eq!(
        file_type_from_attr(&node_attr(NodeKind::File, libc::S_IFREG | 0o644)),
        FileType::RegularFile
    );
    assert_eq!(
        file_type_from_attr(&node_attr(NodeKind::Symlink, libc::S_IFLNK | 0o777)),
        FileType::Symlink
    );
    for (mode, expected) in [
        (libc::S_IFCHR, FileType::CharDevice),
        (libc::S_IFBLK, FileType::BlockDevice),
        (libc::S_IFIFO, FileType::NamedPipe),
        (libc::S_IFSOCK, FileType::Socket),
        (0, FileType::RegularFile),
    ] {
        assert_eq!(
            file_type_from_attr(&node_attr(NodeKind::Special, mode | 0o600)),
            expected
        );
    }
}

#[test]
fn time_helpers_cover_epoch_rounding_now_and_pre_epoch() {
    assert_eq!(f64_to_system_time(-1.0), UNIX_EPOCH);
    let rounded = f64_to_system_time(1.999_999_6)
        .duration_since(UNIX_EPOCH)
        .unwrap();
    assert_eq!(rounded.as_secs(), 2);
    assert_eq!(rounded.subsec_nanos(), 0);
    assert_eq!(
        system_time_to_f64(UNIX_EPOCH.checked_sub(Duration::from_secs(1)).unwrap()),
        0.0
    );
    let specific_time = TimeOrNow::SpecificTime(UNIX_EPOCH + Duration::from_secs(3));
    assert!(is_specific_time(&Some(specific_time)));
    assert!(!is_specific_time(&Some(TimeOrNow::Now)));
    assert!(!is_specific_time(&None));
    assert_eq!(time_or_now(specific_time), 3.0);
    assert!(time_or_now(TimeOrNow::Now) > 0.0);
}

#[test]
fn proc_status_parsers_handle_missing_invalid_and_live_processes() {
    assert!(read_proc_status(std::process::id())
        .unwrap()
        .contains("Uid:"));
    assert!(read_proc_status(u32::MAX).is_none());
    assert!(parse_status_groups_for_identity("Uid:\t1\n", 1, 1).is_none());
    assert!(parse_status_identity("Uid:\tbad\n", "Uid:").is_none());
    assert!(parse_status_identity("Name:\ttest\n", "Uid:").is_none());
    assert!(parse_status_groups("Name:\ttest\n").is_empty());
    assert_eq!(parse_status_groups("Groups:\t1 bad 2\n"), vec![1, 2]);
}

#[test]
fn open_mask_maps_access_and_truncate_flags() {
    assert_eq!(open_mask(OpenFlags(libc::O_RDONLY)), libc::R_OK);
    assert_eq!(open_mask(OpenFlags(libc::O_WRONLY)), libc::W_OK);
    assert_eq!(open_mask(OpenFlags(libc::O_RDWR)), libc::R_OK | libc::W_OK);
    assert_eq!(
        open_mask(OpenFlags(libc::O_RDONLY | libc::O_TRUNC)),
        libc::R_OK | libc::W_OK
    );
    assert_eq!(open_mask(OpenFlags(3)), libc::R_OK);
}

#[test]
fn errno_helpers_preserve_os_errors_and_map_xattrs() {
    assert_eq!(
        errno(&ArgosError::PermissionDenied("x".into())).code(),
        libc::EACCES
    );
    assert_eq!(
        errno(&ArgosError::Io(std::io::Error::from_raw_os_error(
            libc::ENOSPC
        )))
        .code(),
        libc::ENOSPC
    );
    assert_eq!(
        xattr_errno(&ArgosError::Invalid("x".into())).code(),
        libc::EINVAL
    );
}

#[test]
fn open_reply_flags_follow_direct_io_and_writeback_policy() {
    let tmp = tempfile::tempdir().unwrap();
    let volume = ArgosFs::create(
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
    let fuse = ArgosFuse::new(volume.clone());
    assert_eq!(fuse.open_reply_flags(false), FopenFlags::FOPEN_KEEP_CACHE);
    assert_eq!(fuse.open_reply_flags(true), FopenFlags::FOPEN_NOFLUSH);

    volume
        .set_io_policy(crate::types::IoMode::Direct, true, true, true)
        .unwrap();
    assert_eq!(fuse.open_reply_flags(false), FopenFlags::FOPEN_DIRECT_IO);
    assert_eq!(
        fuse.open_reply_flags(true),
        FopenFlags::FOPEN_DIRECT_IO | FopenFlags::FOPEN_NOFLUSH
    );
}

#[test]
fn regular_file_and_handle_validation_report_expected_errors() {
    let tmp = tempfile::tempdir().unwrap();
    let volume = ArgosFs::create(
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
    let ino = volume.create_file_path("/file", 0o600).unwrap();
    let dir = volume.mkdir("/dir", 0o700).unwrap();
    let fuse = ArgosFuse::new(volume);
    fuse.require_regular_file(INodeNo(ino)).unwrap();
    assert!(matches!(
        fuse.require_regular_file(INodeNo(dir)),
        Err(ArgosError::IsDirectory(_))
    ));
    assert!(matches!(
        fuse.require_writable_handle(INodeNo(ino), FileHandle(999)),
        Err(ArgosError::Invalid(_))
    ));
}

#[test]
fn statfs_returns_published_snapshot_during_metadata_write_lock() {
    let dir = tempfile::tempdir().unwrap();
    let volume = ArgosFs::create(
        dir.path(),
        VolumeConfig {
            k: 1,
            m: 0,
            ..VolumeConfig::default()
        },
        1,
        false,
    )
    .unwrap();
    let fuse = ArgosFuse::new(volume.clone());
    let original = fuse.statfs_snapshot();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        volume.test_with_metadata_write_lock(|| {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
        });
    });
    started_rx.recv_timeout(Duration::from_secs(3)).unwrap();
    let started = std::time::Instant::now();
    let cached = fuse.statfs_snapshot();
    let elapsed = started.elapsed();
    release_tx.send(()).unwrap();
    writer.join().unwrap();
    assert_eq!(cached.usable_bytes, original.usable_bytes);
    assert_eq!(cached.free_bytes, original.free_bytes);
    assert_eq!(cached.inode_count, original.inode_count);
    assert!(
        elapsed < Duration::from_millis(200),
        "statfs blocked: {elapsed:?}"
    );
}

#[test]
fn sequential_writeback_merges_four_mib_without_cross_inode_growth() {
    let dir = tempfile::tempdir().unwrap();
    let volume = ArgosFs::create(
        dir.path(),
        VolumeConfig {
            k: 1,
            m: 0,
            ..VolumeConfig::default()
        },
        1,
        false,
    )
    .unwrap();
    let fuse = ArgosFuse::new(volume);
    let one_mib = vec![0x51; 1024 * 1024];
    for i in 0..4 {
        assert!(fuse.queue_writeback(17, i * one_mib.len() as u64, &one_mib));
    }
    let state = fuse.writeback.lock();
    let extent = state.dirty.get(&17).unwrap();
    assert_eq!(extent.offset, 0);
    assert_eq!(extent.data.len(), 4 * 1024 * 1024);
    assert!(extent.data.iter().all(|&b| b == 0x51));
    drop(state);
    assert!(!fuse.queue_writeback(17, 4 * one_mib.len() as u64, &one_mib));
    assert!(fuse.queue_writeback(19, 0, &one_mib));
    let state = fuse.writeback.lock();
    assert_eq!(state.dirty.get(&19).unwrap().data.len(), 1024 * 1024);
}

#[test]
fn directory_listing_reuses_large_snapshot_and_invalidates_after_create() {
    let tmp = tempfile::tempdir().unwrap();
    let volume = ArgosFs::create(
        tmp.path(),
        VolumeConfig {
            k: 1,
            m: 0,
            chunk_size: 4096,
            ..VolumeConfig::default()
        },
        1,
        false,
    )
    .unwrap();
    volume.mkdir("/bulk", 0o755).unwrap();
    for index in 0..16 {
        volume
            .write_file(&format!("/bulk/item-{index:04}"), b"data", 0o600)
            .unwrap();
    }
    let ino = volume.resolve_path("/bulk", true).unwrap();
    let fuse = ArgosFuse::new(volume.clone());
    let first = fuse.shared.directory_listing(ino).unwrap();
    assert_eq!(first.len(), 18);
    let second = fuse.shared.directory_listing(ino).unwrap();
    assert!(Arc::ptr_eq(&first, &second));
    assert!(fuse.shared.directory_cache.lock().used_bytes() > 0);

    volume
        .write_file("/bulk/new-file", b"added", 0o600)
        .unwrap();
    let third = fuse.shared.directory_listing(ino).unwrap();
    assert_eq!(third.len(), 19);
    assert!(!Arc::ptr_eq(&first, &third));
    assert!(third.iter().any(|entry| entry.name == "new-file"));

    // A write to a regular file changes its attr but not its parent listing:
    // readdirplus must fetch current attributes, while readdir reuses names.
    let file_ino = volume.resolve_path("/bulk/new-file", true).unwrap();
    let cached_attr_size = third
        .iter()
        .find(|e| e.attr.ino == file_ino)
        .unwrap()
        .attr
        .size;
    volume
        .write_file("/bulk/new-file", b"updated with a longer payload", 0o600)
        .unwrap();
    let again = fuse.shared.directory_listing(ino).unwrap();
    assert!(Arc::ptr_eq(&again, &third));
    let actual_attr = volume.attr_inode(file_ino).unwrap();
    assert!(actual_attr.size > cached_attr_size);
}

#[test]
fn blocked_inode_flush_does_not_hold_mount_writeback_map() {
    let tmp = tempfile::tempdir().unwrap();
    let volume = ArgosFs::create(
        tmp.path(),
        VolumeConfig {
            k: 1,
            m: 0,
            ..Default::default()
        },
        1,
        false,
    )
    .unwrap();
    let a = volume.create_file_path("/a", 0o600).unwrap();
    let b = volume.create_file_path("/b", 0o600).unwrap();
    assert_ne!(
        a as usize % WRITEBACK_LOCK_STRIPES,
        b as usize % WRITEBACK_LOCK_STRIPES
    );
    let fuse = ArgosFuse::new(volume.clone());
    assert!(fuse.queue_writeback(a, 0, b"pending-a"));
    assert!(fuse.queue_writeback(b, 0, b"pending-b"));
    std::thread::scope(|scope| {
        volume.test_with_metadata_write_lock(|| {
            let shared = &fuse.shared;
            scope.spawn(move || shared.flush_inode_writeback(a).unwrap());
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if shared.writeback_lock(a).try_write().is_none() {
                    break;
                }
                assert!(Instant::now() < deadline, "flush never acquired inode gate");
                std::thread::yield_now();
            }
            // The flusher cannot make progress while we hold metadata, but
            // unrelated dirty-state operations must remain available.
            let (sent, received) = std::sync::mpsc::channel();
            scope.spawn(move || {
                let state = shared.writeback.lock();
                assert_eq!(state.pending_bytes, 18);
                assert_eq!(state.dirty[&a].data, b"pending-a");
                drop(state);
                shared.discard_inode_writeback(b);
                sent.send(()).unwrap();
            });
            received
                .recv_timeout(Duration::from_secs(5))
                .expect("blocked flush held global writeback map");
            assert_eq!(shared.writeback.lock().pending_bytes, 9);
        });
    });
    assert_eq!(volume.read_file("/a", false).unwrap(), b"pending-a");
    assert!(fuse.writeback.lock().dirty.is_empty());
}

#[test]
fn optional_fuse_capabilities_preserve_read_only_directory_listing() {
    use InitFlags as F;
    assert_eq!(
        fuse_optional_capabilities(F::FUSE_PARALLEL_DIROPS),
        F::FUSE_PARALLEL_DIROPS
    );
    assert_eq!(
        fuse_optional_capabilities(F::FUSE_DO_READDIRPLUS | F::FUSE_READDIRPLUS_AUTO),
        F::empty()
    );
    let supported = F::FUSE_DO_READDIRPLUS | F::FUSE_READDIRPLUS_AUTO | F::FUSE_PARALLEL_DIROPS;
    assert_eq!(
        fuse_optional_capabilities(supported | F::FUSE_WRITEBACK_CACHE),
        F::FUSE_PARALLEL_DIROPS
    );
}

#[test]
fn fuse_io_setting_rejects_invalid_and_unbounded_values() {
    for value in [None, Some(""), Some("abc"), Some("0"), Some("257")] {
        assert_eq!(fuse_io_setting(value, 8, 256, 64), 64);
    }
    assert_eq!(fuse_io_setting(Some("8"), 8, 256, 64), 8);
    assert_eq!(fuse_io_setting(Some("256"), 8, 256, 64), 256);
}
