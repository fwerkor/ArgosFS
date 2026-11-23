use super::*;
use serde_json::{json, Value};

fn round_trip_arrays(before: Value, after: Value) -> Vec<MetadataDeltaOp> {
    let mut ops = Vec::new();
    diff_json("/extents", &before, &after, &mut ops);
    let mut full = json!({"extents": before});
    for op in &ops {
        apply_delta_op(&mut full, op).unwrap();
    }
    assert_eq!(full["extents"], after);
    assert_eq!(
        serde_json::to_vec(&full["extents"]).unwrap(),
        serde_json::to_vec(&after).unwrap()
    );
    ops
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
