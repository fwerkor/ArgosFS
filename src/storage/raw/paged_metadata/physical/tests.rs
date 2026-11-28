use super::*;
use crate::backend::FileBlockBackend;
use std::sync::Arc;

fn fixture(pages: u64) -> (tempfile::TempDir, Arc<dyn StorageBackend>, AllocationMap) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("allocator.img");
    let bytes = AllocationMap::required_bytes(pages).unwrap();
    std::fs::File::create(&path)
        .unwrap()
        .set_len(bytes)
        .unwrap();
    let backend = Arc::new(FileBlockBackend::open_loop(&[path], true).unwrap());
    let map = AllocationMap::new(
        &*backend,
        Arena {
            disk_id: "disk-0000".into(),
            offset: 0,
            length: bytes,
        },
        pages,
    )
    .unwrap();
    (dir, backend, map)
}

#[test]
fn sparse_billion_page_allocator_cow_and_retained_roots() {
    let (_dir, backend, map) = fixture(1_000_000_000);
    let empty = AllocatorRoot::default();
    map.open_root(&*backend, empty).unwrap();
    let changes = BTreeMap::from([(0, 1), (1, 2), (999_999_999, 3)]);
    let first = map.prepare(&*backend, empty, empty, &changes).unwrap();
    assert!(first.pages_written() <= 10);
    map.write_prepared(&*backend, &first).unwrap();
    let a = first.root();
    assert_eq!(AllocatorRoot::decode(&a.encode()).unwrap(), a);
    assert_eq!(map.allocated_entries(&*backend, a).unwrap(), changes);
    assert_eq!(map.next_free(&*backend, a, 0).unwrap(), Some(2));
    assert_eq!(map.next_free(&*backend, a, 999_999_999).unwrap(), None);
    let second = map
        .prepare(&*backend, a, empty, &BTreeMap::from([(0, 0), (2, 4)]))
        .unwrap();
    map.write_prepared(&*backend, &second).unwrap();
    let b = second.root();
    let third = map
        .prepare(
            &*backend,
            b,
            a,
            &BTreeMap::from([(0, 7), (2, 0), (999_999_999, 0)]),
        )
        .unwrap();
    map.write_prepared(&*backend, &third).unwrap();
    let c = third.root();
    // Before the third root is published, both previous roots are intact.
    assert_eq!(map.allocated_entries(&*backend, a).unwrap(), changes);
    assert_eq!(map.reference_count(&*backend, b, 2).unwrap(), 4);
    assert_eq!(map.reference_count(&*backend, c, 0).unwrap(), 7);
    assert_eq!(
        map.next_free(&*backend, c, 999_999_999).unwrap(),
        Some(999_999_999)
    );
    // Simulated power loss: discard the third candidate and retry from b/a.
    let retry = map
        .prepare(&*backend, b, a, &BTreeMap::from([(2, 0), (1, 0)]))
        .unwrap();
    map.write_prepared(&*backend, &retry).unwrap();
    assert_eq!(map.allocated_entries(&*backend, a).unwrap(), changes);
    assert_eq!(
        map.allocated_entries(&*backend, retry.root()).unwrap(),
        BTreeMap::from([(999_999_999, 3)])
    );
}

#[test]
fn allocator_random_edits_exact_reference_counts_and_free_search() {
    let pages = 32_769;
    let (_dir, backend, map) = fixture(pages);
    let mut roots = [AllocatorRoot::default(); 2];
    let mut oracle = [BTreeMap::new(), BTreeMap::new()];
    let mut rng = 7u64;
    for round in 0..80 {
        let base = round % 2;
        let other = 1 - base;
        let mut changes = BTreeMap::new();
        for _ in 0..20 {
            rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
            changes.insert((rng >> 20) % pages, ((rng >> 45) % 4) as u32);
        }
        let plan = map
            .prepare(&*backend, roots[base], roots[other], &changes)
            .unwrap();
        map.write_prepared(&*backend, &plan).unwrap();
        assert_eq!(
            map.allocated_entries(&*backend, roots[base]).unwrap(),
            oracle[base]
        );
        assert_eq!(
            map.allocated_entries(&*backend, roots[other]).unwrap(),
            oracle[other]
        );
        oracle[other] = oracle[base].clone();
        for (page, refs) in changes {
            if refs == 0 {
                oracle[other].remove(&page);
            } else {
                oracle[other].insert(page, refs);
            }
        }
        roots[other] = plan.root();
        assert_eq!(
            map.allocated_entries(&*backend, roots[other]).unwrap(),
            oracle[other]
        );
        let from = (rng >> 10) % pages;
        let expected = (from..pages).find(|page| !oracle[other].contains_key(page));
        assert_eq!(
            map.next_free(&*backend, roots[other], from).unwrap(),
            expected
        );
    }
}

#[test]
fn corrupt_allocator_path_is_never_used_as_free_space() {
    let (_dir, backend, map) = fixture(2048);
    let empty = AllocatorRoot::default();
    let plan = map
        .prepare(&*backend, empty, empty, &BTreeMap::from([(0, 1)]))
        .unwrap();
    map.write_prepared(&*backend, &plan).unwrap();
    let root = plan.root();
    let address = map.address(map.top(), 0, root.0.unwrap().slot).unwrap();
    backend
        .write_at(&map.arena.disk_id, address + 100, &[0xff])
        .unwrap();
    backend.flush_device(&map.arena.disk_id).unwrap();
    assert!(map.open_root(&*backend, root).is_err());
    assert!(map.next_free(&*backend, root, 0).is_err());
    assert!(map
        .prepare(&*backend, root, empty, &BTreeMap::from([(1, 1)]))
        .is_err());
}
