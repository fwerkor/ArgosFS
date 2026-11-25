use super::*;
use serde_json::{json, Value};

fn wrap_at(path: &str, value: Value) -> Value {
    let mut wrapped = value;
    for token in path.trim_start_matches('/').split('/').rev() {
        wrapped = json!({ token: wrapped });
    }
    wrapped
}

fn extract_at<'a>(mut value: &'a Value, path: &str) -> &'a Value {
    for token in path.trim_start_matches('/').split('/') {
        value = &value[token];
    }
    value
}

fn round_trip_arrays_at(path: &str, before: Value, after: Value) -> Vec<MetadataDeltaOp> {
    let mut ops = Vec::new();
    diff_json(path, &before, &after, &mut ops);
    let mut full = wrap_at(path, before);
    for op in &ops {
        apply_delta_op(&mut full, op).unwrap();
    }
    let replayed = extract_at(&full, path);
    assert_eq!(replayed, &after);
    assert_eq!(
        serde_json::to_vec(replayed).unwrap(),
        serde_json::to_vec(&after).unwrap()
    );
    ops
}

fn round_trip_arrays(before: Value, after: Value) -> Vec<MetadataDeltaOp> {
    round_trip_arrays_at("/extents", before, after)
}

fn round_trip_arrays_splice(path: &str, before: Value, after: Value) -> Vec<MetadataDeltaOp> {
    let _guard = allow_array_splice(true);
    round_trip_arrays_at(path, before, after)
}

#[test]
fn array_deltas_handle_append_remove_middle_and_nested_mutations() {
    for (before, after) in [
        (json!([1, 2, 3]), json!([1, 2, 3, 4, 5])),
        (json!([1, 2, 3, 4]), json!([1, 4])),
        (json!([1, 2, 3, 4]), json!([1, 2])),
        (json!([1, 2, 3, 4]), json!([1, 5, 3, 4])),
        (json!([1, 2, 3]), json!([1, 9, 2, 3])),
        (json!([1, 2, 3, 4]), json!([1, 9, 8, 3, 4])),
        (json!([1, 2, 3]), json!([])),
        (json!([]), json!([1, 2])),
        (
            json!([{"a":1, "b":2}, {"a":3}]),
            json!([{"a":1, "b":4}, {"a":3}]),
        ),
    ] {
        round_trip_arrays(before, after);
    }
}

#[test]
fn massive_allocator_delta_edits_one_entry_instead_of_rewriting_full_list() {
    // Mirrors the 270k+ free extents reported in the 294k-inode Borg fixture.
    let before = Value::Array(
        (0_u64..270_879)
            .map(|i| json!({"offset": i * 8192, "length":4096}))
            .collect(),
    );
    let mut changed = before.clone();
    changed[10]["length"] = json!(8192);
    let ops = round_trip_arrays(before.clone(), changed);
    assert_eq!(ops.len(), 1);
    assert!(serde_json::to_vec(&ops).unwrap().len() < 200);

    // Removing a consumed extent near the front must not serialize the
    // 270,878 unaffected extents which shift their array index.
    let mut removed = before.clone();
    removed.as_array_mut().unwrap().remove(10);
    let ops = round_trip_arrays(before, removed);
    assert_eq!(ops.len(), 1);
    assert!(serde_json::to_vec(&ops).unwrap().len() < 200);
}

#[test]
fn insertion_and_bulk_deletion_in_large_array_stay_sparse() {
    // The Borg pool's free lists reached 47,289 entries. A single freed
    // extent inserted near the end used to reserialize the entire list.
    let path = "/raw_pool/allocators/disk-0007/free_extents";
    let entries = |count: u64| {
        Value::Array(
            (0..count)
                .map(|i| json!({"offset": i * 8192, "length": 4096}))
                .collect(),
        )
    };
    let before = entries(47_289);

    let mut inserted = before.clone();
    inserted
        .as_array_mut()
        .unwrap()
        .insert(47_286, json!({"offset": 387_250_176, "length": 4096}));
    let ops = round_trip_arrays_at(path, before.clone(), inserted);
    assert!(
        !ops.iter()
            .any(|op| matches!(op, MetadataDeltaOp::Replace { path: p, .. } if p == path)),
        "a near-end insertion must not rewrite the full free list"
    );
    assert!(
        serde_json::to_vec(&ops).unwrap().len() < 2048,
        "a near-end insertion should stay within a few shifted entries"
    );

    // An insertion near the front would need to shift almost the whole list.
    // That is legitimately larger than one replacement, and the early bound
    // must choose it without materializing tens of thousands of operations.
    let mut front = before.clone();
    front
        .as_array_mut()
        .unwrap()
        .insert(0, json!({"offset": 0, "length": 4096}));
    let ops = round_trip_arrays_at(path, before.clone(), front);
    assert_eq!(ops.len(), 1);
    assert!(matches!(
        &ops[0],
        MetadataDeltaOp::Replace { path: p, .. } if p == path
    ));

    // Allocating 500 contiguous extents removes 500 sorted entries. Sparse
    // removals are still far smaller than the whole list.
    let mut deleted = before.clone();
    deleted.as_array_mut().unwrap().drain(20_000..20_500);
    let ops = round_trip_arrays_at(path, before, deleted);
    assert!(
        !ops.iter()
            .any(|op| matches!(op, MetadataDeltaOp::Replace { path: p, .. } if p == path)),
        "a bulk deletion must not rewrite the full free list"
    );
    assert!(serde_json::to_vec(&ops).unwrap().len() < 100_000);
}

#[test]
fn dense_array_mutations_prefer_one_replacement_over_per_element_paths() {
    // When most of a large array changes, one pointer op per element repeats
    // each path and can exceed the whole-array replacement. The 2026-11-23
    // Borg run paid that overhead on nearly every group commit.
    let before = Value::Array(
        (0_u64..4096)
            .map(|i| json!({"offset": i * 8192, "length": 4096}))
            .collect(),
    );
    let mut after = before.clone();
    for entry in after.as_array_mut().unwrap() {
        entry["length"] = json!(8192);
    }
    let path = "/raw_pool/allocators/disk-0000/free_extents";
    let ops = round_trip_arrays_at(path, before, after.clone());
    assert_eq!(ops.len(), 1, "dense array must collapse to one replacement");
    assert!(matches!(
        &ops[0],
        MetadataDeltaOp::Replace { path, .. } if path == "/raw_pool/allocators/disk-0000/free_extents"
    ));
    assert!(
        serde_json::to_vec(&ops).unwrap().len() <= serde_json::to_vec(&after).unwrap().len() + 100,
        "replacement must not exceed the array body plus its envelope"
    );
}

#[test]
fn sparse_array_mutations_keep_the_small_encoding() {
    // The guard must not undo the sparse win that motivated array deltas.
    let before = Value::Array(
        (0_u64..4096)
            .map(|i| json!({"offset": i * 8192, "length": 4096}))
            .collect(),
    );
    let mut after = before.clone();
    for index in [7, 100, 4000] {
        after[index]["length"] = json!(8192);
    }
    let ops = round_trip_arrays(before.clone(), after);
    assert_eq!(ops.len(), 3);
    assert!(
        serde_json::to_vec(&ops).unwrap().len() < serde_json::to_vec(&before).unwrap().len() / 100,
        "three changed extents must stay far below a full-list replacement"
    );
}

#[test]
fn splice_encodes_insertions_removals_and_dense_edits_as_one_range_op() {
    let path = "/raw_pool/allocators/disk-0007/free_extents";
    let before = Value::Array(
        (0_u64..47_289)
            .map(|i| json!({"offset": i * 8192, "length": 4096}))
            .collect(),
    );

    // A middle insertion would previously shift or rewrite ~47k entries.
    let mut inserted = before.clone();
    inserted
        .as_array_mut()
        .unwrap()
        .insert(23_000, json!({"offset": 123_456, "length": 8192}));
    let ops = round_trip_arrays_splice(path, before.clone(), inserted);
    assert_eq!(ops.len(), 1);
    assert!(matches!(
        &ops[0],
        MetadataDeltaOp::Splice { start: 23_000, delete: 0, insert, .. } if insert.len() == 1
    ));
    assert!(serde_json::to_vec(&ops).unwrap().len() < 512);

    // A contiguous allocation run is one deletion range, no shifted suffix.
    let mut deleted = before.clone();
    deleted.as_array_mut().unwrap().drain(20_000..20_500);
    let ops = round_trip_arrays_splice(path, before.clone(), deleted);
    assert_eq!(ops.len(), 1);
    assert!(matches!(
        &ops[0],
        MetadataDeltaOp::Splice {
            start: 20_000,
            delete: 500,
            insert,
            ..
        } if insert.is_empty()
    ));

    // A commit that frees extents throughout a sorted list changes many
    // scattered runs. The encoder must keep the untouched stretches in place
    // instead of replacing the whole middle from the first to the last change.
    let mut scattered = before.clone();
    for index in (500..47_000).step_by(997) {
        scattered.as_array_mut().unwrap().insert(
            index,
            json!({"offset": 400_000_000 + index as u64 * 8192, "length": 4096}),
        );
    }
    let added = scattered.as_array().unwrap().len() - before.as_array().unwrap().len();
    let ops = round_trip_arrays_splice(path, before.clone(), scattered);
    assert_eq!(ops.len(), added);
    assert!(
        serde_json::to_vec(&ops).unwrap().len() < 30_000,
        "scattered insertions must not reserialize the untouched list"
    );

    // Dense changes remain one bounded range operation.
    let mut dense = before.clone();
    for entry in dense.as_array_mut().unwrap() {
        entry["length"] = json!(8192);
    }
    let ops = round_trip_arrays_splice(path, before, dense);
    assert_eq!(ops.len(), 1);
    assert!(matches!(&ops[0], MetadataDeltaOp::Splice { start: 0, .. }));
}

#[test]
fn randomized_splice_edits_round_trip() {
    let path = "/raw_pool/allocators/disk-0003/free_extents";
    let mut seed = 0x517c_c1b7_2722_0a95_u64;
    let mut base = Value::Array(
        (0_u64..96)
            .map(|i| json!({"offset": i * 8192, "length": 4096}))
            .collect(),
    );
    for _ in 0..256 {
        let mut next = base.clone();
        let items = next.as_array_mut().unwrap();
        for _ in 0..(seed % 5) + 1 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let kind = seed % 3;
            let index = (seed as usize / 3) % (items.len() + 1);
            match kind {
                0 => items.insert(index, json!({"offset": seed, "length": 8192})),
                1 if index < items.len() => {
                    items.remove(index);
                }
                _ if index < items.len() => {
                    items[index]["length"] = json!(seed);
                }
                _ => {}
            }
        }
        round_trip_arrays_splice(path, base.clone(), next.clone());
        base = next;
    }
}

#[test]
fn randomized_array_edits_round_trip_within_the_replacement_bound() {
    let path = "/raw_pool/allocators/disk-0003/free_extents";
    let mut seed = 0x9e37_79b9_7f4a_7c15_u64;
    let mut base = Value::Array(
        (0_u64..96)
            .map(|i| json!({"offset": i * 8192, "length": 4096}))
            .collect(),
    );
    for _ in 0..256 {
        let mut next = base.clone();
        let items = next.as_array_mut().unwrap();
        for _ in 0..(seed % 4) + 1 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let kind = seed % 3;
            let index = (seed as usize / 3) % (items.len() + 1);
            match kind {
                0 => items.insert(index, json!({"offset": seed, "length": 8192})),
                1 if index < items.len() => {
                    items.remove(index);
                }
                _ if index < items.len() => {
                    items[index]["length"] = json!(seed);
                }
                _ => {}
            }
        }
        let ops = round_trip_arrays_at(path, base.clone(), next.clone());
        let serialized = serde_json::to_vec(&ops).unwrap();
        let replacement_bound = serde_json::to_vec(&next).unwrap().len() + path.len() + 64;
        assert!(
            serialized.len() <= replacement_bound,
            "delta {} exceeded replacement bound {}",
            serialized.len(),
            replacement_bound
        );
        base = next;
    }
}

#[test]
fn signed_zero_in_array_remains_distinct() {
    let ops = round_trip_arrays(json!([0.0]), json!([-0.0]));
    assert_eq!(ops.len(), 1);
    assert!(matches!(ops[0], MetadataDeltaOp::Replace { .. }));
    let nested = round_trip_arrays(json!([{"number": 0.0}]), json!([{"number": -0.0}]));
    assert_eq!(nested.len(), 1, "nested signed zero must not be omitted");
}

#[test]
fn allocator_delta_round_trips_through_authenticated_v2_journal() {
    use crate::types::{RawAllocatorState, RawFreeExtent, VolumeConfig};
    use crate::volume::ArgosFs;
    use tempfile::tempdir;

    let root = tempdir().unwrap();
    let fs = ArgosFs::create(
        root.path(),
        VolumeConfig {
            k: 1,
            m: 0,
            ..VolumeConfig::default()
        },
        1,
        false,
    )
    .unwrap();
    let mut previous = fs.metadata_snapshot();
    previous.raw_pool.allocators.insert(
        "disk-0000".into(),
        RawAllocatorState {
            block_size: 4096,
            data_start: 4096,
            data_end: 100_000_000,
            next_offset: 64_000_000,
            free_extents: (0_u64..10_000)
                .map(|i| RawFreeExtent {
                    offset: 8192 + i * 4096,
                    length: 4096,
                })
                .collect(),
        },
    );
    prepare_metadata_integrity(&mut previous, String::new()).unwrap();
    previous.inodes.mark_committed();

    let mut next = previous.clone();
    next.txid += 1;
    next.raw_pool
        .allocators
        .get_mut("disk-0000")
        .unwrap()
        .free_extents[200]
        .length = 8192;
    let prepared = prepare_delta_integrity_with_delta(&mut next, &previous).unwrap();
    assert!(serde_json::to_vec(&prepared).unwrap().len() < 1024);
    assert!(prepared.iter().any(|op| matches!(
        op, MetadataDeltaOp::Replace { path, .. }
        if path.ends_with("/free_extents/200/length")
    )));
    let replayed = apply_metadata_delta(&previous, &prepared).unwrap();
    assert_eq!(replayed.integrity, next.integrity);
    assert_eq!(
        serde_json::to_vec(&replayed.raw_pool.allocators).unwrap(),
        serde_json::to_vec(&next.raw_pool.allocators).unwrap()
    );
    let mut corrupt = prepared.clone();
    let entry = corrupt.iter_mut().find(|op| matches!(
        op, MetadataDeltaOp::Replace { path, .. } if path.ends_with("/free_extents/200/length")
    )).unwrap();
    if let MetadataDeltaOp::Replace { value, .. } = entry {
        *value = json!(12288);
    }
    assert!(apply_metadata_delta(&previous, &corrupt).is_err());
}

#[test]
fn splice_delta_round_trips_through_authenticated_v2_journal() {
    use crate::types::{RawAllocatorState, RawFreeExtent, VolumeConfig};
    use crate::volume::ArgosFs;
    use tempfile::tempdir;

    let root = tempdir().unwrap();
    let fs = ArgosFs::create(
        root.path(),
        VolumeConfig {
            k: 1,
            m: 0,
            ..VolumeConfig::default()
        },
        1,
        false,
    )
    .unwrap();
    let mut previous = fs.metadata_snapshot();
    previous.raw_pool.allocators.insert(
        "disk-0000".into(),
        RawAllocatorState {
            block_size: 4096,
            data_start: 4096,
            data_end: 100_000_000,
            next_offset: 64_000_000,
            free_extents: (0_u64..10_000)
                .map(|i| RawFreeExtent {
                    offset: 8192 + i * 4096,
                    length: 4096,
                })
                .collect(),
        },
    );
    prepare_metadata_integrity(&mut previous, String::new()).unwrap();
    previous.inodes.mark_committed();

    let mut next = previous.clone();
    next.txid += 1;
    next.raw_pool
        .allocators
        .get_mut("disk-0000")
        .unwrap()
        .free_extents
        .insert(
            5_000,
            RawFreeExtent {
                offset: 50_000_000,
                length: 8192,
            },
        );
    let prepared = {
        let _guard = allow_array_splice(true);
        prepare_delta_integrity_with_delta(&mut next, &previous).unwrap()
    };
    assert_eq!(
        prepared
            .iter()
            .filter(|op| matches!(op, MetadataDeltaOp::Splice { .. }))
            .count(),
        1,
        "the free list must use one splice op"
    );
    assert!(prepared.iter().any(|op| matches!(
        op,
        MetadataDeltaOp::Splice {
            start: 5_000,
            delete: 0,
            ..
        }
    )));
    assert!(serde_json::to_vec(&prepared).unwrap().len() < 1024);
    let replayed = apply_metadata_delta(&previous, &prepared).unwrap();
    assert_eq!(replayed.integrity, next.integrity);
    assert_eq!(
        serde_json::to_vec(&replayed.raw_pool.allocators).unwrap(),
        serde_json::to_vec(&next.raw_pool.allocators).unwrap()
    );

    let mut corrupt = prepared.clone();
    if let MetadataDeltaOp::Splice { start, .. } = corrupt
        .iter_mut()
        .find(|op| matches!(op, MetadataDeltaOp::Splice { .. }))
        .unwrap()
    {
        *start = 9_999;
    }
    assert!(apply_metadata_delta(&previous, &corrupt).is_err());
}
