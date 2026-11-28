use super::*;

fn key(i: u64) -> TypedKey {
    TypedKey::new(2, &i.to_be_bytes()).unwrap()
}

fn point(i: u64) -> ScanWindow<TypedKey> {
    ScanWindow {
        start: Bound::Included(key(i)),
        end: Bound::Included(key(i)),
        max_records: 1,
        max_bytes: 32 * 1024,
        order: ScanOrder::Ascending,
    }
}

#[test]
fn snapshot_cow_shared_graph_survives_reopen_and_reclamation() {
    let (_dir, backend, arenas) = fixture(3, 1024);
    let faults = Arc::new(FaultBackend::new(backend));
    let mut tree = TypedMetadata::format(faults.clone(), POOL, arenas.clone()).unwrap();
    tree.commit(
        &(0..4096)
            .map(|i| (key(i), Some(vec![i as u8; 128])))
            .collect(),
    )
    .unwrap();
    let original = tree.txid();
    let before = tree.root_hash();
    faults
        .read_calls
        .store(0, std::sync::atomic::Ordering::Relaxed);
    let a = tree.create_snapshot("first").unwrap();
    assert_eq!(a.txid, original);
    assert_ne!(before, tree.root_hash());
    assert!(faults.read_calls.load(std::sync::atomic::Ordering::Relaxed) < 200);
    assert!(tree.audit_allocators().values().all(Result::is_ok));
    tree.commit(&TypedChanges::from([
        (key(100), Some(vec![11; 8192])),
        (key(200), None),
    ]))
    .unwrap();
    let b = tree.create_snapshot("second").unwrap();
    assert_ne!(a.root_hash, b.root_hash);
    assert!(tree.create_snapshot("first").is_err());
    assert!(tree
        .commit(&TypedChanges::from([(
            TypedKey::new(SNAPSHOT_DOMAIN, b"fake").unwrap(),
            Some(vec![0; 40])
        )]))
        .is_err());
    assert!(tree.audit_allocators().values().all(Result::is_ok));
    tree.commit(&TypedChanges::from([(key(100), Some(vec![33; 256]))]))
        .unwrap();
    let (names, more) = tree.snapshot_page(None, 1, 4096).unwrap();
    assert!(more);
    assert_eq!(names, vec![("first".into(), a)]);
    assert_eq!(
        tree.snapshot_page(Some("first"), 2, 4096).unwrap(),
        (vec![("second".into(), b)], false)
    );
    drop(tree);
    let mut tree = TypedMetadata::open(faults, POOL, arenas).unwrap();
    assert_eq!(
        tree.scan_snapshot("first", &point(100)).unwrap().records[0].1,
        vec![100; 128]
    );
    assert_eq!(
        tree.scan_snapshot("first", &point(200)).unwrap().records[0].1,
        vec![200; 128]
    );
    assert_eq!(
        tree.scan_snapshot("second", &point(100)).unwrap().records[0].1,
        vec![11; 8192]
    );
    assert!(tree
        .scan_snapshot("second", &point(200))
        .unwrap()
        .records
        .is_empty());
    assert_eq!(tree.get(key(100)).unwrap().unwrap(), vec![33; 256]);
    tree.delete_snapshot("first").unwrap();
    tree.commit(&TypedChanges::new()).unwrap();
    assert!(tree.snapshot_info("first").unwrap().is_none());
    assert!(tree.scan_snapshot("first", &point(100)).is_err());
    assert_eq!(
        tree.scan_snapshot("second", &point(100)).unwrap().records[0].1,
        vec![11; 8192]
    );
    assert!(tree.audit_allocators().values().all(Result::is_ok));
    tree.delete_snapshot("second").unwrap();
    tree.commit(&TypedChanges::new()).unwrap();
    assert!(tree.snapshot_page(None, 5, 4096).unwrap().0.is_empty());
    assert!(tree.audit_allocators().values().all(Result::is_ok));
}

#[test]
fn snapshot_creation_certificate_loss_and_stale_member_repair() {
    let (_dir, backend, arenas) = fixture(3, 256);
    let faults = Arc::new(FaultBackend::new(backend));
    let mut tree = TypedMetadata::format(faults.clone(), POOL, arenas.clone()).unwrap();
    tree.commit(&TypedChanges::from([(key(1), Some(vec![1; 8192]))]))
        .unwrap();
    tree.create_snapshot("old").unwrap();
    tree.commit(&TypedChanges::from([(key(1), Some(vec![2; 8192]))]))
        .unwrap();
    *faults.failures.lock() = arenas
        .iter()
        .map(|a| (a.disk_id.clone(), Phase::Certificate))
        .collect();
    assert!(matches!(
        tree.create_snapshot("new"),
        Err(ArgosError::IndeterminateCommit(_))
    ));
    drop(tree);
    faults.failures.lock().clear();
    let mut tree = TypedMetadata::open(faults.clone(), POOL, arenas.clone()).unwrap();
    assert_eq!(
        tree.scan_snapshot("new", &point(1)).unwrap().records[0].1,
        vec![2; 8192]
    );
    faults.offline.lock().insert("disk-0002".into());
    tree.commit(&TypedChanges::from([(key(1), Some(vec![3; 8192]))]))
        .unwrap();
    drop(tree);
    faults.offline.lock().clear();
    let mut tree = TypedMetadata::open(faults, POOL, arenas).unwrap();
    assert!(tree
        .repair_stale()
        .unwrap()
        .values()
        .all(std::result::Result::is_ok));
    assert!(tree.audit_allocators().values().all(Result::is_ok));
    assert_eq!(
        tree.scan_snapshot("old", &point(1)).unwrap().records[0].1,
        vec![1; 8192]
    );
    assert_eq!(
        tree.scan_snapshot("new", &point(1)).unwrap().records[0].1,
        vec![2; 8192]
    );
    assert_eq!(tree.get(key(1)).unwrap().unwrap(), vec![3; 8192]);
}
