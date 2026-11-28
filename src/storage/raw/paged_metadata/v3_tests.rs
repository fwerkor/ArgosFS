use super::*;

#[test]
fn v3_compound_keys_fast_reader_and_bounded_range() {
    use super::typed::{decode_record, encode_record, Domain};
    let (_dir, backend, arenas) = fixture(3, 8192);
    let faults = Arc::new(FaultBackend::new(backend));
    let mut tree = TypedMetadata::format(faults.clone(), POOL, arenas.clone()).unwrap();
    let mut changes = TypedChanges::new();
    for ino in 1u64..=20_000 {
        changes.insert(
            Domain::Inode.inode_prefix(ino),
            Some(encode_record(&ino).unwrap()),
        );
    }
    for name in [b"a".as_slice(), b"a\xff", b"b", b"\xff"] {
        changes.insert(
            Domain::Dirent.inode_name(1, name).unwrap(),
            Some(encode_record(&name.to_vec()).unwrap()),
        );
    }
    tree.commit(&changes).unwrap();
    let key = Domain::Inode.inode_prefix(10_001);
    let edit = tree
        .commit(&TypedChanges::from([(
            key.clone(),
            Some(encode_record(&99u64).unwrap()),
        )]))
        .unwrap();
    assert!(edit.members.values().all(|io| io.pages_written < 16));
    let digest = tree.root_hash();
    drop(tree);
    assert!(PagedMetadata::open(faults.clone(), POOL, arenas.clone()).is_err());
    faults.reads.lock().clear();
    faults
        .read_calls
        .store(0, std::sync::atomic::Ordering::Relaxed);
    let reader = CertifiedReader::<TypedKey>::open(faults.clone(), POOL, arenas.clone()).unwrap();
    let open_reads = faults.read_calls.load(std::sync::atomic::Ordering::Relaxed);
    assert!(open_reads <= 18, "open scanned metadata: {open_reads}");
    assert_eq!(reader.root_hash(), digest);
    assert_eq!(
        decode_record::<u64>(&reader.get(key).unwrap().unwrap()).unwrap(),
        99
    );
    let prefix = Domain::Dirent.inode_prefix(1);
    let mut query = ScanWindow {
        start: Bound::Included(prefix.clone()),
        end: Bound::Excluded(prefix.prefix_end().unwrap()),
        max_records: 2,
        max_bytes: 4096,
        order: ScanOrder::Ascending,
    };
    let first = reader.scan(&query).unwrap();
    assert!(first.more);
    assert_eq!(first.records.len(), 2);
    query.start = Bound::Excluded(first.records.last().unwrap().0.clone());
    let second = reader.scan(&query).unwrap();
    assert!(!second.more);
    assert_eq!(second.records.len(), 2);
    assert_eq!(
        decode_record::<Vec<u8>>(&second.records[1].1).unwrap(),
        b"\xff"
    );
    query.max_bytes = 1;
    assert!(reader.scan(&query).is_err());
    let total_reads = faults.read_calls.load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        total_reads < 80,
        "point/range queries scanned the image: {total_reads}"
    );
    eprintln!("V3 20k inodes / 3 members: certified reader opens in {open_reads} read calls; open + point/range queries {total_reads}");
    drop(reader);
    let tree = TypedMetadata::open(faults, POOL, arenas).unwrap();
    assert_eq!(tree.root_hash(), digest);
}

#[test]
fn v3_certified_reader_does_not_fall_back_after_certified_corruption() {
    let (_dir, backend, arenas) = fixture(3, 128);
    let key = TypedKey::new(1, b"test").unwrap();
    let mut tree = TypedMetadata::format(backend.clone(), POOL, arenas.clone()).unwrap();
    tree.commit(&TypedChanges::from([(key.clone(), Some(b"old".to_vec()))]))
        .unwrap();
    tree.commit(&TypedChanges::from([(key.clone(), Some(b"new".to_vec()))]))
        .unwrap();
    // Damage the current root on all replicas while preserving certificates.
    for member in &tree.members {
        let root = member.roots[member.root_slot(tree.current).unwrap()]
            .unwrap()
            .tree
            .unwrap();
        backend
            .write_at(
                &member.arena.disk_id,
                root.page * PAGE_SIZE as u64 + 20,
                &[0xff],
            )
            .unwrap();
        backend.flush_device(&member.arena.disk_id).unwrap();
    }
    drop(tree);
    assert!(matches!(
        CertifiedReader::<TypedKey>::open(backend, POOL, arenas),
        Err(ArgosError::CorruptedMetadata(_))
    ));
}

#[test]
fn v3_certified_reader_requires_recovery_for_new_prepared_quorum() {
    let (_dir, backend, arenas) = fixture(3, 128);
    let faults = Arc::new(FaultBackend::new(backend));
    let key = TypedKey::new(1, b"test").unwrap();
    let mut tree = TypedMetadata::format(faults.clone(), POOL, arenas.clone()).unwrap();
    tree.commit(&TypedChanges::from([(key.clone(), Some(b"old".to_vec()))]))
        .unwrap();
    *faults.failures.lock() = arenas
        .iter()
        .map(|arena| (arena.disk_id.clone(), Phase::Certificate))
        .collect();
    assert!(matches!(
        tree.commit(&TypedChanges::from([(key.clone(), Some(b"new".to_vec()))])),
        Err(ArgosError::IndeterminateCommit(_))
    ));
    drop(tree);
    faults.failures.lock().clear();
    assert!(matches!(
        CertifiedReader::<TypedKey>::open(faults.clone(), POOL, arenas.clone()),
        Err(ArgosError::JournalReplayRequired(_))
    ));
    let tree = TypedMetadata::open(faults, POOL, arenas).unwrap();
    assert_eq!(tree.get(key).unwrap().unwrap(), b"new");
}

#[test]
fn v3_variable_length_keys_survive_splits_merges_and_reopen() {
    let (_dir, backend, arenas) = fixture(1, 4096);
    let mut tree = TypedMetadata::format(backend.clone(), POOL, arenas.clone()).unwrap();
    let mut expected = BTreeMap::new();
    for i in 0..600u64 {
        let mut name = i.to_be_bytes().to_vec();
        name.extend(vec![i as u8; if i % 3 == 0 { 1000 } else { 2 }]);
        let key = TypedKey::new(7, &name).unwrap();
        expected.insert(key.clone(), vec![i as u8; if i % 2 == 0 { 768 } else { 3 }]);
    }
    tree.commit(
        &expected
            .iter()
            .map(|(k, v)| (k.clone(), Some(v.clone())))
            .collect(),
    )
    .unwrap();
    for _ in 0..3 {
        let changes = expected
            .keys()
            .step_by(3)
            .map(|key| (key.clone(), None))
            .collect::<TypedChanges>();
        for key in changes.keys() {
            expected.remove(key);
        }
        tree.commit(&changes).unwrap();
    }
    drop(tree);
    let tree = TypedMetadata::open(backend, POOL, arenas).unwrap();
    assert_eq!(tree.records().unwrap(), expected);
}

#[test]
fn v3_record_envelope_and_atomic_parent_index() {
    use super::typed::{decode_record, encode_record, Domain, RecordTransaction};
    let (_dir, backend, arenas) = fixture(1, 128);
    let mut tree = TypedMetadata::format(backend, POOL, arenas).unwrap();
    let mut tx = RecordTransaction::begin(&tree);
    tx.set_dirent(1, b"file", None, Some(2)).unwrap();
    let stale = RecordTransaction::begin(&tree);
    tx.commit(&mut tree).unwrap();
    assert!(stale.commit(&mut tree).is_err());
    let mut tx = RecordTransaction::begin(&tree);
    tx.set_dirent(1, b"file", Some(2), Some(3)).unwrap();
    tx.commit(&mut tree).unwrap();
    assert!(tree
        .get(Domain::parent(2, 1, b"file").unwrap())
        .unwrap()
        .is_none());
    assert!(tree
        .get(Domain::parent(3, 1, b"file").unwrap())
        .unwrap()
        .is_some());
    let entry = tree
        .get(Domain::Dirent.inode_name(1, b"file").unwrap())
        .unwrap()
        .unwrap();
    assert_eq!(decode_record::<u64>(&entry).unwrap(), 3);
    let mut record = encode_record(&vec![42; 4096]).unwrap();
    assert_eq!(decode_record::<Vec<i32>>(&record).unwrap(), vec![42; 4096]);
    record[4] = 255;
    assert!(matches!(
        decode_record::<Vec<i32>>(&record),
        Err(ArgosError::IncompatibleFormat(_))
    ));
}

#[test]
fn v3_writable_cold_open_is_bounded_and_allocation_audit_matches() {
    use super::typed::Domain;
    let (_dir, backend, arenas) = fixture(3, 8192);
    let faults = Arc::new(FaultBackend::new(backend));
    let mut tree = TypedMetadata::format(faults.clone(), POOL, arenas.clone()).unwrap();
    let changes = (1..=50_000u64)
        .map(|ino| {
            (
                Domain::Inode.inode_prefix(ino),
                Some(vec![(ino % 251) as u8; 64]),
            )
        })
        .collect();
    tree.commit(&changes).unwrap();
    let key = Domain::Inode.inode_prefix(17_000);
    tree.commit(&TypedChanges::from([(
        key.clone(),
        Some(b"first".to_vec()),
    )]))
    .unwrap();
    assert!(tree.audit_allocators().values().all(Result::is_ok));
    drop(tree);
    faults
        .read_calls
        .store(0, std::sync::atomic::Ordering::Relaxed);
    let mut tree = TypedMetadata::open(faults.clone(), POOL, arenas.clone()).unwrap();
    let open_reads = faults.read_calls.load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        open_reads <= 24,
        "writable mount read {open_reads} requests"
    );
    let report = tree
        .commit(&TypedChanges::from([(
            key.clone(),
            Some(b"second".to_vec()),
        )]))
        .unwrap();
    let total_reads = faults.read_calls.load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        total_reads - open_reads < 250,
        "writable edit scanned the image"
    );
    assert!(report.members.values().all(|io| io.pages_written <= 24));
    assert!(tree.audit_allocators().values().all(Result::is_ok));
    eprintln!("V3 50k records, 3 members: writable open {open_reads} requests, edit {} requests, per-member IO {:?}",total_reads - open_reads, report.members);
    drop(tree);
    let tree = TypedMetadata::open(faults, POOL, arenas).unwrap();
    assert_eq!(tree.get(key).unwrap().unwrap(), b"second");
}

#[test]
fn v3_reference_counts_reclaim_after_random_edits_and_reopens() {
    let (_dir, backend, arenas) = fixture(1, 192);
    let mut tree = TypedMetadata::format(backend.clone(), POOL, arenas.clone()).unwrap();
    let mut oracle = BTreeMap::new();
    let mut rng = 11u64;
    for round in 0..120 {
        let mut changes = TypedChanges::new();
        for _ in 0..4 {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
            let key = TypedKey::new(4, &((rng >> 20) % 100).to_be_bytes()).unwrap();
            let val = if rng.is_multiple_of(3) {
                None
            } else {
                Some(vec![
                    round as u8;
                    if rng.is_multiple_of(13) { 7000 } else { 96 }
                ])
            };
            changes.insert(key, val);
        }
        for (key, value) in &changes {
            if let Some(value) = value {
                oracle.insert(key.clone(), value.clone());
            } else {
                oracle.remove(key);
            }
        }
        tree.commit(&changes).unwrap();
        assert!(
            tree.audit_allocators().values().all(Result::is_ok),
            "round {round}"
        );
        if round % 7 == 0 {
            drop(tree);
            tree = TypedMetadata::open(backend.clone(), POOL, arenas.clone()).unwrap();
            assert_eq!(tree.records().unwrap(), oracle);
        }
    }
    assert_eq!(tree.records().unwrap(), oracle);
    tree.commit(&oracle.keys().map(|key| (key.clone(), None)).collect())
        .unwrap();
    tree.commit(&TypedChanges::new()).unwrap();
    assert!(tree.audit_allocators().values().all(Result::is_ok));
    assert!(tree.records().unwrap().is_empty());
}

#[test]
fn v3_crash_boundaries_recover_exact_allocator_and_repair_returned_member() {
    let (_dir, backend, arenas) = fixture(3, 192);
    let faults = Arc::new(FaultBackend::new(backend));
    let key = TypedKey::new(1, b"data").unwrap();
    let mut tree = TypedMetadata::format(faults.clone(), POOL, arenas.clone()).unwrap();
    tree.commit(&TypedChanges::from([(key.clone(), Some(vec![1; 12_000]))]))
        .unwrap();
    for (phase, expected) in [(Phase::Body, 1), (Phase::Root, 1), (Phase::Certificate, 2)] {
        *faults.failures.lock() =
            BTreeMap::from([("disk-0001".into(), phase), ("disk-0002".into(), phase)]);
        assert!(tree
            .commit(&TypedChanges::from([(key.clone(), Some(vec![2; 12_000]))]))
            .is_err());
        drop(tree);
        faults.failures.lock().clear();
        tree = TypedMetadata::open(faults.clone(), POOL, arenas.clone()).unwrap();
        assert_eq!(
            tree.get(key.clone()).unwrap().unwrap(),
            vec![expected; 12_000]
        );
        // The torn alternate slot must not cause reuse of any live page.
        tree.commit(&TypedChanges::from([(key.clone(), Some(vec![1; 12_000]))]))
            .unwrap();
        tree.repair_stale().unwrap();
        // Two more publications age out any damaged/stale alternate roots.
        tree.commit(&TypedChanges::new()).unwrap();
        tree.commit(&TypedChanges::new()).unwrap();
        let audit = tree.audit_allocators();
        assert!(audit.values().all(Result::is_ok), "{phase:?}: {audit:?}");
    }
    faults.offline.lock().insert("disk-0002".into());
    tree.commit(&TypedChanges::from([(key.clone(), Some(vec![3; 12_000]))]))
        .unwrap();
    drop(tree);
    faults.offline.lock().clear();
    tree = TypedMetadata::open(faults, POOL, arenas).unwrap();
    let repaired = tree.repair_stale().unwrap();
    assert!(
        repaired.values().all(|result| result.is_ok()),
        "{repaired:?}"
    );
    assert_eq!(tree.get(key).unwrap().unwrap(), vec![3; 12_000]);
    assert!(tree.audit_allocators().values().all(Result::is_ok));
}

#[test]
fn v3_corrupt_allocator_does_not_permit_unsafe_writes() {
    let (_dir, backend, arenas) = fixture(3, 128);
    let key = TypedKey::new(1, b"data").unwrap();
    let mut tree = TypedMetadata::format(backend.clone(), POOL, arenas.clone()).unwrap();
    tree.commit(&TypedChanges::from([(key.clone(), Some(vec![1; 4096]))]))
        .unwrap();
    drop(tree);
    for arena in &arenas {
        let (data, _) = arena.views::<TypedKey>(&*backend).unwrap();
        // Destroy the reserved physical allocator area, not the logical tree.
        backend
            .write_at(
                &arena.disk_id,
                arena.offset + data.length,
                &vec![0; (arena.length - data.length) as usize],
            )
            .unwrap();
        backend.flush_device(&arena.disk_id).unwrap();
    }
    let mut tree = TypedMetadata::open(backend, POOL, arenas).unwrap();
    assert_eq!(tree.get(key.clone()).unwrap().unwrap(), vec![1; 4096]);
    assert!(tree
        .commit(&TypedChanges::from([(key.clone(), Some(vec![2; 4096]))]))
        .is_err());
    assert_eq!(tree.get(key).unwrap().unwrap(), vec![1; 4096]);
}

#[test]
fn v3_feature_negotiation_is_bound_to_latest_certified_root() {
    for (ro, incompatible) in [(true, false), (false, true)] {
        let (_dir, backend, arenas) = fixture(3, 128);
        let key = TypedKey::new(1, b"key").unwrap();
        let mut tree = TypedMetadata::format(backend.clone(), POOL, arenas.clone()).unwrap();
        tree.commit(&TypedChanges::from([(key.clone(), Some(b"old".to_vec()))]))
            .unwrap();
        tree.commit(&TypedChanges::from([(key.clone(), Some(b"new".to_vec()))]))
            .unwrap();
        for member in &tree.members {
            let slot = member.root_slot(tree.current).unwrap();
            let mut root = member.roots[slot].unwrap();
            let features = &mut root.physical.as_mut().unwrap().features;
            if ro {
                features.ro_compat |= 1 << 40;
            }
            if incompatible {
                features.incompat |= 1 << 40;
            }
            let identity = member_identity(tree.namespace, &member.arena.disk_id);
            for (index, certificate) in [(slot, false), (slot + 2, true)] {
                member
                    .arena
                    .write(
                        &*backend,
                        index as u64,
                        &root.encode::<TypedKey>(identity, member.arena.pages(), certificate),
                    )
                    .unwrap();
            }
            backend.flush_device(&member.arena.disk_id).unwrap();
        }
        drop(tree);
        if ro {
            assert!(matches!(
                TypedMetadata::open(backend.clone(), POOL, arenas.clone()),
                Err(ArgosError::ReadonlyRequired(_))
            ));
            let reader = CertifiedReader::<TypedKey>::open(backend, POOL, arenas).unwrap();
            assert_eq!(reader.get(key).unwrap().unwrap(), b"new");
        } else {
            assert!(matches!(
                TypedMetadata::open(backend.clone(), POOL, arenas.clone()),
                Err(ArgosError::IncompatibleFormat(_))
            ));
            assert!(matches!(
                CertifiedReader::<TypedKey>::open(backend, POOL, arenas),
                Err(ArgosError::IncompatibleFormat(_))
            ));
        }
    }
}

#[test]
fn v3_import_and_typed_access_preserve_non_utf8_names_and_sparse_extent_lookup() {
    use super::typed::{import_image, Domain, RecordTransaction, RecordView};
    use std::os::unix::ffi::OsStrExt;
    let host = tempfile::tempdir().unwrap();
    let fs = crate::ArgosFs::create(
        host.path(),
        crate::VolumeConfig {
            k: 1,
            m: 0,
            chunk_size: 4096,
            compression: crate::types::Compression::None,
            ..Default::default()
        },
        1,
        false,
    )
    .unwrap();
    fs.write_file("/payload", &vec![9; 32_768], 0o600).unwrap();
    let name = std::ffi::OsStr::from_bytes(b"binary-\xff");
    fs.create_file_at(1, name, 0o600).unwrap();
    let meta = fs.metadata_snapshot();
    let payload = fs.resolve_path("/payload", true).unwrap();
    let expected_binary = fs.lookup(1, name).unwrap().ino;
    let (_dir, backend, arenas) = fixture(1, 4096);
    let mut tree = TypedMetadata::format(backend.clone(), POOL, arenas.clone()).unwrap();
    tree.commit(&import_image(&meta).unwrap()).unwrap();
    {
        let view = RecordView::new(&tree);
        assert_eq!(view.inode(payload).unwrap().unwrap().size, 32_768);
        assert_eq!(
            view.lookup(1, b"binary-\xff").unwrap(),
            Some(expected_binary)
        );
        let (first, more) = view.directory_page(1, None, 1, 4096).unwrap();
        assert!(more);
        let (second, more) = view.directory_page(1, Some(&first[0].0), 10, 4096).unwrap();
        assert!(!more);
        assert!(first
            .iter()
            .chain(second.iter())
            .any(|(name, ino)| name == b"binary-\xff" && *ino == expected_binary));
        for offset in [0, 4095, 4096, 15_000, 32_767] {
            let actual = view.block_at(payload, offset).unwrap().unwrap();
            assert!(
                actual.raw_offset <= offset && offset < actual.raw_offset + actual.raw_size as u64
            );
        }
        assert!(view.block_at(payload, 32_768).unwrap().is_none());
    }
    let mut tx = RecordTransaction::begin(&tree);
    tx.set_dirent(1, b"payload", Some(expected_binary), None)
        .unwrap();
    assert!(matches!(tx.commit(&mut tree), Err(ArgosError::Conflict(_))));
    assert_eq!(
        RecordView::new(&tree).lookup(1, b"payload").unwrap(),
        Some(payload)
    );
    assert!(tree
        .get(Domain::parent(payload, 1, b"payload").unwrap())
        .unwrap()
        .is_some());
    drop(tree);
    let reader = CertifiedReader::<TypedKey>::open(backend, POOL, arenas).unwrap();
    assert_eq!(
        RecordView::new(&reader).lookup(1, b"binary-\xff").unwrap(),
        Some(expected_binary)
    );
}

#[test]
fn v3_record_cache_is_bounded_and_reuses_positive_and_negative_lookups() {
    use super::typed::RecordRead;
    let (_dir, backend, arenas) = fixture(1, 128);
    let faults = Arc::new(FaultBackend::new(backend));
    let mut tree = TypedMetadata::format(faults.clone(), POOL, arenas.clone()).unwrap();
    let key = TypedKey::new(1, b"hot").unwrap();
    let absent = TypedKey::new(1, b"missing").unwrap();
    tree.commit(&TypedChanges::from([(key.clone(), Some(vec![7; 512]))]))
        .unwrap();
    let cache = RecordCache::new(&tree, 4096);
    assert_eq!(
        cache.get_record(key.clone()).unwrap().unwrap(),
        vec![7; 512]
    );
    assert!(cache.get_record(absent.clone()).unwrap().is_none());
    let reads = faults.read_calls.load(std::sync::atomic::Ordering::Relaxed);
    for _ in 0..50 {
        assert!(cache.get_record(key.clone()).unwrap().is_some());
        assert!(cache.get_record(absent.clone()).unwrap().is_none());
    }
    assert_eq!(
        faults.read_calls.load(std::sync::atomic::Ordering::Relaxed),
        reads
    );
    for i in 0..100u64 {
        assert!(cache
            .get_record(TypedKey::new(2, &i.to_be_bytes()).unwrap())
            .unwrap()
            .is_none());
    }
    let info = cache.info();
    assert!(info.charged_bytes <= 4096 && info.entries < 32 && info.hits == 100);
}

#[test]
#[ignore = "large V3 standalone metadata engine cold-open regression"]
fn v3_294k_records_writable_mount_is_path_bounded() {
    use super::typed::Domain;
    let (_dir, backend, arenas) = fixture(1, 32768);
    let faults = Arc::new(FaultBackend::new(backend));
    let mut tree = TypedMetadata::format(faults.clone(), POOL, arenas.clone()).unwrap();
    let changes = (1..=294_000u64)
        .map(|ino| {
            (
                Domain::Inode.inode_prefix(ino),
                Some(vec![(ino % 251) as u8; 128]),
            )
        })
        .collect();
    let start = std::time::Instant::now();
    let initial = tree.commit(&changes).unwrap();
    let build = start.elapsed();
    drop(changes);
    drop(tree);
    faults
        .read_calls
        .store(0, std::sync::atomic::Ordering::Relaxed);
    let start = std::time::Instant::now();
    let mut tree = TypedMetadata::open(faults.clone(), POOL, arenas).unwrap();
    let elapsed = start.elapsed();
    let reads = faults.read_calls.load(std::sync::atomic::Ordering::Relaxed);
    let key = Domain::Inode.inode_prefix(147_000);
    let edited = tree
        .commit(&TypedChanges::from([(
            key.clone(),
            Some(b"changed".to_vec()),
        )]))
        .unwrap();
    assert_eq!(tree.get(key).unwrap().unwrap(), b"changed");
    assert!(reads <= 8);
    assert!(edited.members["disk-0000"].pages_written <= 24);
    assert!(tree.audit_allocators().values().all(Result::is_ok));
    eprintln!("V3 294k records: initial {} pages ({build:?}), writable open {reads} read requests ({elapsed:?}), edit {:?}",initial.members["disk-0000"].pages_written,edited.members["disk-0000"]);
}
