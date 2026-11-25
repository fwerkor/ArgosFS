//! Typed allocator delta paths preserve persistent extent sharing. Unusual
//! legacy JSON pointer sequences retain the generic replay implementation.
use super::*;
use crate::types::{RawAllocatorState, RawFreeExtent};

type Allocators = BTreeMap<String, RawAllocatorState>;

pub(super) fn diff(
    previous: &Allocators,
    next: &Allocators,
    ops: &mut Vec<MetadataDeltaOp>,
) -> Result<()> {
    const ROOT: &str = "/raw_pool/allocators";
    for key in previous.keys().filter(|key| !next.contains_key(*key)) {
        ops.push(MetadataDeltaOp::Remove {
            path: push_json_pointer(ROOT, key),
        });
    }
    for (key, updated) in next {
        let path = push_json_pointer(ROOT, key);
        let Some(old) = previous.get(key) else {
            ops.push(MetadataDeltaOp::Add {
                path,
                value: serde_json::to_value(updated)?,
            });
            continue;
        };
        let mut old_header = old.clone();
        let mut next_header = updated.clone();
        old_header.free_extents.clear();
        next_header.free_extents.clear();
        diff_json(
            &path,
            &serde_json::to_value(old_header)?,
            &serde_json::to_value(next_header)?,
            ops,
        );
        let left = &old.free_extents;
        let right = &updated.free_extents;
        if left.ptr_eq(right) {
            continue;
        }
        let path = push_json_pointer(&path, "free_extents");
        if left.len() == right.len() {
            // Avoid building two huge JSON arrays for sparse fixed-position
            // updates. Dense edits retain the generic array delta strategy.
            let changed = left
                .iter()
                .zip(right.iter())
                .enumerate()
                .filter(|(_, (a, b))| a != b)
                .take(65)
                .collect::<Vec<_>>();
            if changed.len() <= 64 {
                for (index, (a, b)) in changed {
                    diff_json(
                        &push_json_pointer(&path, &index.to_string()),
                        &serde_json::to_value(a)?,
                        &serde_json::to_value(b)?,
                        ops,
                    );
                }
                continue;
            }
        }
        if array_splice_allowed() && diff_sorted_extents(&path, left, right, ops)? {
            continue;
        }
        diff_json(
            &path,
            &serde_json::to_value(left)?,
            &serde_json::to_value(right)?,
            ops,
        );
    }
    Ok(())
}

/// Offset order supplies exact anchors without materializing JSON trees or
/// searching a bounded lookahead window. Only changed runs become Values.
fn diff_sorted_extents(
    path: &str,
    left: &imbl::Vector<RawFreeExtent>,
    right: &imbl::Vector<RawFreeExtent>,
    ops: &mut Vec<MetadataDeltaOp>,
) -> Result<bool> {
    const MAX_RUNS: usize = 8192;
    let ordered = |items: &imbl::Vector<RawFreeExtent>| {
        items
            .iter()
            .zip(items.iter().skip(1))
            .all(|(a, b)| a.offset < b.offset)
    };
    if !ordered(left) || !ordered(right) {
        return Ok(false);
    }
    let mut edits = Vec::new();
    let mut a = left.iter().peekable();
    let mut b = right.iter().peekable();
    let mut cursor = 0u64;
    while a.peek().is_some() || b.peek().is_some() {
        if matches!((a.peek(), b.peek()), (Some(x), Some(y)) if x == y) {
            a.next();
            b.next();
            cursor += 1;
            continue;
        }
        let mut delete = 0u64;
        let mut insert = Vec::new();
        loop {
            match (a.peek(), b.peek()) {
                (Some(x), Some(y)) if x == y => break,
                (Some(x), Some(y)) if x.offset < y.offset => {
                    a.next();
                    delete += 1;
                }
                (Some(x), Some(y)) if x.offset == y.offset => {
                    a.next();
                    delete += 1;
                    insert.push(serde_json::to_value(b.next().unwrap())?);
                }
                (_, Some(_)) => insert.push(serde_json::to_value(b.next().unwrap())?),
                (Some(_), None) => {
                    a.next();
                    delete += 1;
                }
                (None, None) => break,
            }
        }
        let count = insert.len() as u64;
        edits.push(MetadataDeltaOp::Splice {
            path: path.to_owned(),
            start: cursor,
            delete,
            insert,
        });
        cursor += count;
        if edits.len() > MAX_RUNS {
            return Ok(false);
        }
    }
    ops.extend(edits);
    Ok(true)
}

pub(super) fn is_allocator_path(tokens: &[String]) -> bool {
    matches!(tokens, [root, map, _, ..] if root == "raw_pool" && map == "allocators")
}

/// Return None for uncommon legacy paths or intermediate states that cannot
/// be represented by a typed allocator. The caller then replays the original
/// JSON sequence without any partial typed mutations.
pub(super) fn replay(previous: &Allocators, delta: &[MetadataDeltaOp]) -> Option<Allocators> {
    let mut allocators = previous.clone();
    for op in delta {
        let tokens = json_pointer_tokens(delta_path(op)).ok()?;
        if matches!(tokens.as_slice(), [root] if root == "raw_pool")
            || matches!(tokens.as_slice(), [root,map] if root == "raw_pool" && map == "allocators")
        {
            return None;
        }
        if !is_allocator_path(&tokens) {
            continue;
        }
        if json_pointer_from_tokens(&tokens) != delta_path(op) {
            return None;
        }
        let [_, _, id, tail @ ..] = tokens.as_slice() else {
            return None;
        };
        if tail.is_empty() {
            if let MetadataDeltaOp::Add { value, .. } = op {
                allocators.insert(id.clone(), serde_json::from_value(value.clone()).ok()?);
            } else if let MetadataDeltaOp::Replace { value, .. } = op {
                *allocators.get_mut(id)? = serde_json::from_value(value.clone()).ok()?;
            } else if matches!(op, MetadataDeltaOp::Remove { .. }) {
                allocators.remove(id)?;
            } else {
                return None;
            }
            continue;
        }
        let allocator = allocators.get_mut(id)?;
        if !replay_one(allocator, tail, op) {
            return None;
        }
    }
    Some(allocators)
}

fn replacement(op: &MetadataDeltaOp) -> Option<&serde_json::Value> {
    if let MetadataDeltaOp::Add { value, .. } | MetadataDeltaOp::Replace { value, .. } = op {
        Some(value)
    } else {
        None
    }
}

fn replay_one(allocator: &mut RawAllocatorState, tail: &[String], op: &MetadataDeltaOp) -> bool {
    fn edit(
        allocator: &mut RawAllocatorState,
        tail: &[String],
        op: &MetadataDeltaOp,
    ) -> Option<()> {
        match tail {
            [field] if field == "free_extents" => {
                if let MetadataDeltaOp::Splice {
                    start,
                    delete,
                    insert,
                    ..
                } = op
                {
                    let start = usize::try_from(*start).ok()?;
                    let end = start.checked_add(usize::try_from(*delete).ok()?)?;
                    let list = &mut allocator.free_extents;
                    if end > list.len() {
                        return None;
                    }
                    let inserted = insert
                        .iter()
                        .map(|value| serde_json::from_value::<RawFreeExtent>(value.clone()))
                        .collect::<std::result::Result<imbl::Vector<_>, _>>()
                        .ok()?;
                    let suffix = list.split_off(end);
                    list.truncate(start);
                    list.append(inserted);
                    list.append(suffix);
                } else {
                    allocator.free_extents =
                        serde_json::from_value(replacement(op)?.clone()).ok()?;
                }
            }
            [field] => {
                let value = replacement(op)?.as_u64()?;
                match field.as_str() {
                    "block_size" => allocator.block_size = value,
                    "data_start" => allocator.data_start = value,
                    "data_end" => allocator.data_end = value,
                    "next_offset" => allocator.next_offset = value,
                    _ => return None,
                }
            }
            [field, index, rest @ ..] if field == "free_extents" => {
                let position = index.parse::<usize>().ok()?;
                if position.to_string() != *index {
                    return None;
                }
                let list = &mut allocator.free_extents;
                if rest.is_empty() {
                    if let MetadataDeltaOp::Add { value, .. } = op {
                        let extent: RawFreeExtent = serde_json::from_value(value.clone()).ok()?;
                        if position == list.len() {
                            list.push_back(extent);
                        } else {
                            *list.get_mut(position)? = extent;
                        }
                    } else if let MetadataDeltaOp::Replace { value, .. } = op {
                        *list.get_mut(position)? = serde_json::from_value(value.clone()).ok()?;
                    } else if matches!(op, MetadataDeltaOp::Remove { .. }) {
                        if position >= list.len() {
                            return None;
                        }
                        list.remove(position);
                    } else {
                        return None;
                    }
                } else if let [name] = rest {
                    let value = replacement(op)?.as_u64()?;
                    let extent = list.get_mut(position)?;
                    match name.as_str() {
                        "offset" => extent.offset = value,
                        "length" => extent.length = value,
                        _ => return None,
                    }
                } else {
                    return None;
                }
            }
            _ => return None,
        }
        Some(())
    }
    edit(allocator, tail, op).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn fixture() -> (tempfile::TempDir, Metadata) {
        let dir = tempfile::tempdir().unwrap();
        let fs = crate::volume::ArgosFs::create(
            dir.path(),
            crate::types::VolumeConfig {
                k: 1,
                m: 0,
                ..Default::default()
            },
            1,
            false,
        )
        .unwrap();
        let mut meta = fs.metadata_snapshot();
        for i in 0..4 {
            meta.raw_pool.allocators.insert(
                format!("disk/{i}~"),
                RawAllocatorState {
                    block_size: 4096,
                    data_start: 4096,
                    data_end: 2_000_000_000,
                    next_offset: 1_500_000_000,
                    free_extents: (0..47_000)
                        .map(|n| RawFreeExtent {
                            offset: (n * 3 + 1) * 4096,
                            length: 4096,
                        })
                        .collect(),
                },
            );
        }
        prepare_metadata_integrity(&mut meta, String::new()).unwrap();
        (dir, meta)
    }

    #[test]
    fn sparse_allocator_replay_preserves_unchanged_chunks_and_exact_metadata() {
        let (_dir, base) = fixture();
        let mut next = base.clone();
        next.txid += 1;
        next.raw_pool
            .allocators
            .get_mut("disk/0~")
            .unwrap()
            .next_offset += 4096;
        next.raw_pool
            .allocators
            .get_mut("disk/1~")
            .unwrap()
            .free_extents[200]
            .length = 8192;
        let delta = prepare_delta_integrity_with_delta(&mut next, &base).unwrap();
        assert!(serde_json::to_vec(&delta).unwrap().len() < 1024);
        let replayed = apply_metadata_delta(&base, &delta).unwrap();
        assert_eq!(
            serde_json::to_vec(&replayed).unwrap(),
            serde_json::to_vec(&next).unwrap()
        );
        for member in ["disk/0~", "disk/2~", "disk/3~"] {
            assert!(base.raw_pool.allocators[member]
                .free_extents
                .ptr_eq(&replayed.raw_pool.allocators[member].free_extents));
        }
        assert!(std::ptr::eq(
            &base.raw_pool.allocators["disk/1~"].free_extents[30_000],
            &replayed.raw_pool.allocators["disk/1~"].free_extents[30_000]
        ));
        // The original v1 pointer application is also a differential oracle.
        let mut reference = serde_json::to_value(&base).unwrap();
        for op in &delta {
            apply_delta_op(&mut reference, op).unwrap();
        }
        assert_eq!(reference, serde_json::to_value(&replayed).unwrap());
    }

    #[test]
    fn typed_replay_retains_legacy_fallback_and_array_add_semantics() {
        let (_dir, base) = fixture();
        let member = "/raw_pool/allocators/disk~10~0";
        let remove = MetadataDeltaOp::Remove {
            path: format!("{member}/free_extents/10"),
        };
        let add = MetadataDeltaOp::Add {
            path: format!("{member}/free_extents/20"),
            value: json!({"offset": 77, "length": 88}),
        };
        let append = MetadataDeltaOp::Add {
            path: format!("{member}/free_extents/46999"),
            value: json!({"offset": 99, "length": 100}),
        };
        let delta = vec![remove, add, append];
        let typed = replay(&base.raw_pool.allocators, &delta).unwrap();
        let mut reference = serde_json::to_value(&base).unwrap();
        for op in &delta {
            apply_delta_op(&mut reference, op).unwrap();
        }
        assert_eq!(
            serde_json::to_value(typed).unwrap(),
            reference["raw_pool"]["allocators"]
        );
        // A remove-then-add of a required scalar must use the original JSON
        // interpreter, since the temporary object cannot be deserialized.
        let uncommon = vec![
            MetadataDeltaOp::Remove {
                path: format!("{member}/next_offset"),
            },
            MetadataDeltaOp::Add {
                path: format!("{member}/next_offset"),
                value: json!(4096),
            },
        ];
        assert!(replay(&base.raw_pool.allocators, &uncommon).is_none());
    }

    #[test]
    #[ignore = "synthetic allocator serialization benchmark"]
    fn benchmark_unchanged_allocator_delta() {
        let (_dir, base) = fixture();
        let mut next = base.clone();
        next.txid += 1;
        next.raw_pool
            .allocators
            .get_mut("disk/0~")
            .unwrap()
            .next_offset += 4096;
        let start = Instant::now();
        for _ in 0..10 {
            let previous_header = serde_json::to_value(MetadataHeaderPage::from(&base)).unwrap();
            let next_header = serde_json::to_value(MetadataHeaderPage::from(&next)).unwrap();
            let mut ops = Vec::new();
            diff_json("", &previous_header, &next_header, &mut ops);
            std::hint::black_box(ops);
        }
        let old = start.elapsed();
        let start = Instant::now();
        for _ in 0..10 {
            std::hint::black_box(metadata_delta(&base, &next).unwrap());
        }
        let new = start.elapsed();
        println!("188000 extents, 10 iterations: old_header={old:?}, typed_delta={new:?}, speedup={:.1}x", old.as_secs_f64()/new.as_secs_f64());
    }
}

#[cfg(test)]
mod splice_tests {
    use super::*;

    #[test]
    fn sorted_allocator_splices_share_chunks_and_match_generic_replay() {
        let left = RawAllocatorState {
            free_extents: (0..47_000u64)
                .map(|i| RawFreeExtent {
                    offset: (i * 4 + 1) * 4096,
                    length: 4096,
                })
                .collect(),
            ..Default::default()
        }
        .free_extents;
        let mut right = left.clone();
        // Widely separated insertions and removals, beyond generic lookahead.
        right.insert(
            400,
            RawFreeExtent {
                offset: left[399].offset + 8192,
                length: 4096,
            },
        );
        right[900].length = 8192;
        let tail = right.split_off(20_000);
        right.truncate(12_000);
        right.append(tail);
        let last_offset = right[29_999].offset + 8192;
        right.insert(
            30_000,
            RawFreeExtent {
                offset: last_offset,
                length: 4096,
            },
        );
        let path = "/free_extents";
        let mut ops = Vec::new();
        assert!(diff_sorted_extents(path, &left, &right, &mut ops).unwrap());
        assert!(ops.len() <= 4);
        assert!(serde_json::to_vec(&ops).unwrap().len() < 1024);
        let mut state = RawAllocatorState {
            free_extents: left.clone(),
            ..Default::default()
        };
        let mut reference = serde_json::to_value(&state).unwrap();
        for op in &ops {
            assert!(replay_one(&mut state, &["free_extents".into()], op));
            apply_delta_op(&mut reference, op).unwrap();
        }
        assert_eq!(state.free_extents, right);
        assert_eq!(serde_json::to_value(&state).unwrap(), reference);
        // Persistent vector splits may rebalance individual boundary chunks;
        // assert sharing across the whole surviving sequence instead.
        let original = left
            .iter()
            .map(|e| (e.offset, e as *const RawFreeExtent))
            .collect::<BTreeMap<_, _>>();
        let shared = state
            .free_extents
            .iter()
            .filter(|e| {
                original
                    .get(&e.offset)
                    .is_some_and(|ptr| std::ptr::eq(*ptr, *e))
            })
            .count();
        assert!(
            shared > state.free_extents.len() / 2,
            "only {shared} surviving entries shared"
        );
        let bad = MetadataDeltaOp::Splice {
            path: path.into(),
            start: u64::MAX,
            delete: 2,
            insert: vec![],
        };
        assert!(!replay_one(&mut state, &["free_extents".into()], &bad));
        assert!(apply_delta_op(&mut reference, &bad).is_err());
    }

    #[test]
    fn allocator_splice_gate_preserves_legacy_encoding() {
        let left = RawAllocatorState {
            free_extents: (0..1000u64)
                .map(|i| RawFreeExtent {
                    offset: i * 4096 * 4,
                    length: 4096,
                })
                .collect(),
            ..Default::default()
        }
        .free_extents;
        let mut right = left.clone();
        right.insert(
            100,
            RawFreeExtent {
                offset: left[99].offset + 8192,
                length: 4096,
            },
        );
        let previous = BTreeMap::from([(
            "a".into(),
            RawAllocatorState {
                free_extents: left,
                ..Default::default()
            },
        )]);
        let next = BTreeMap::from([(
            "a".into(),
            RawAllocatorState {
                free_extents: right,
                ..Default::default()
            },
        )]);
        for allowed in [false, true] {
            let _guard = allow_array_splice(allowed);
            let mut ops = Vec::new();
            diff(&previous, &next, &mut ops).unwrap();
            assert_eq!(
                ops.iter()
                    .any(|op| matches!(op, MetadataDeltaOp::Splice { .. })),
                allowed
            );
            assert_eq!(
                serde_json::to_value(replay(&previous, &ops).unwrap()).unwrap(),
                serde_json::to_value(&next).unwrap()
            );
        }
    }
}
