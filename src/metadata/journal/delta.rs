use super::*;

/// Build a JSON-compatible journal delta without materializing the full
/// previous/next metadata trees. Existing journal records remain readable.
pub fn metadata_delta(previous: &Metadata, next: &Metadata) -> Result<Vec<MetadataDeltaOp>> {
    let mut ops = Vec::new();
    // Keep large persistent allocator arrays out of the general header JSON.
    let mut previous_header = MetadataHeaderPage::from(previous);
    let mut next_header = MetadataHeaderPage::from(next);
    previous_header.raw_pool.allocators.clear();
    next_header.raw_pool.allocators.clear();
    diff_json(
        "",
        &serde_json::to_value(previous_header)?,
        &serde_json::to_value(next_header)?,
        &mut ops,
    );
    allocator_delta::diff(
        &previous.raw_pool.allocators,
        &next.raw_pool.allocators,
        &mut ops,
    )?;

    diff_metadata_map("/disks", &previous.disks, &next.disks, &mut ops)?;
    diff_inodes(&previous.inodes, &next.inodes, &mut ops)?;
    Ok(ops)
}

/// Committed baselines and mutation tracking bound normal group commits to
/// modified inode keys; independent images fall back to exhaustive scans. The
/// operation order is independent of the full-tree implementation but replay
/// produces the identical JSON metadata.
pub(super) fn diff_inodes(
    previous: &crate::model::inodes::TrackedInodes,
    next: &crate::model::inodes::TrackedInodes,
    ops: &mut Vec<MetadataDeltaOp>,
) -> Result<()> {
    for ino in previous.changed_candidates(next) {
        let path = push_json_pointer("/inodes", &ino.to_string());
        match (previous.get(&ino), next.get(&ino)) {
            (None, Some(inode)) => ops.push(MetadataDeltaOp::Add {
                path,
                value: serde_json::to_value(inode)?,
            }),
            (Some(_), None) => ops.push(MetadataDeltaOp::Remove { path }),
            (Some(old), Some(updated)) => {
                let old_value = serde_json::to_value(old)?;
                let new_value = serde_json::to_value(updated)?;
                diff_json(&path, &old_value, &new_value, ops);
            }
            (None, None) => {}
        }
    }
    Ok(())
}

/// Iterating B-tree keys is O(total inodes); transient JSON memory is limited
/// to a single changed inode rather than two copies of the entire namespace.
/// Full changed-record tracking can later avoid scanning unchanged keys.
pub(super) fn diff_metadata_map<K, V>(
    prefix: &str,
    previous: &BTreeMap<K, V>,
    next: &BTreeMap<K, V>,
    ops: &mut Vec<MetadataDeltaOp>,
) -> Result<()>
where
    K: Ord + ToString,
    V: Serialize,
{
    for key in previous.keys() {
        if !next.contains_key(key) {
            ops.push(MetadataDeltaOp::Remove {
                path: push_json_pointer(prefix, &key.to_string()),
            });
        }
    }
    for (key, next_item) in next {
        let path = push_json_pointer(prefix, &key.to_string());
        if let Some(previous_item) = previous.get(key) {
            // Serialize a single inode before allocating its JSON tree. This
            // also avoids adding any new PartialEq requirements to metadata.
            if serde_json::to_vec(previous_item)? != serde_json::to_vec(next_item)? {
                let old = serde_json::to_value(previous_item)?;
                let new = serde_json::to_value(next_item)?;
                diff_json(&path, &old, &new, ops);
            }
        } else {
            ops.push(MetadataDeltaOp::Add {
                path,
                value: serde_json::to_value(next_item)?,
            });
        }
    }
    Ok(())
}

pub fn apply_metadata_delta(previous: &Metadata, delta: &[MetadataDeltaOp]) -> Result<Metadata> {
    // An entire-map replacement (or a root replacement) is legal in the v1
    // journal protocol. Preserve its old semantics; ordinary journal diffs
    // operate on individual inode/disk keys and never need this fallback.
    if delta.iter().any(|op| {
        let path = delta_path(op);
        path.is_empty() || path == "/inodes" || path == "/disks"
    }) {
        return apply_metadata_delta_legacy(previous, delta);
    }

    // Clone the persistent HAMT, not its inode values. The previous JSON
    // implementation expanded every inode into serde_json::Value, mutated a
    // handful of entries, then built an entirely new HAMT on every record.
    let mut metadata = previous.clone();
    let typed_allocators = allocator_delta::replay(&previous.raw_pool.allocators, delta);
    let mut header: Option<serde_json::Value> = None;
    // The delta writer groups changes to a given inode. Keep at most one
    // expanded inode (and disk) while applying successive nested operations;
    // caching every changed inode would reintroduce unbounded replay memory.
    let mut pending_inode: Option<(crate::types::InodeId, serde_json::Value)> = None;
    let mut pending_disk: Option<(String, serde_json::Value)> = None;
    for op in delta {
        let tokens = json_pointer_tokens(delta_path(op))?;
        if typed_allocators.is_some() && allocator_delta::is_allocator_path(&tokens) {
            continue;
        }
        match tokens.as_slice() {
            [root, key, tail @ ..] if root == "inodes" => {
                flush_pending_disk(&mut metadata, &mut pending_disk)?;
                let ino = key.parse::<crate::types::InodeId>().map_err(|_| {
                    ArgosError::Invalid(format!("invalid inode key in metadata delta: {key}"))
                })?;
                if tail.is_empty() {
                    flush_pending_inode(&mut metadata, &mut pending_inode)?;
                    match op {
                        MetadataDeltaOp::Add { value, .. } => {
                            metadata
                                .inodes
                                .insert(ino, serde_json::from_value(value.clone())?);
                        }
                        MetadataDeltaOp::Replace { value, .. } => {
                            if !metadata.inodes.contains_key(&ino) {
                                return Err(ArgosError::Invalid(format!(
                                    "metadata delta replace target does not exist: {}",
                                    delta_path(op)
                                )));
                            }
                            metadata
                                .inodes
                                .insert(ino, serde_json::from_value(value.clone())?);
                        }
                        MetadataDeltaOp::Remove { .. } => {
                            if metadata.inodes.remove(&ino).is_none() {
                                return Err(ArgosError::Invalid(format!(
                                    "metadata delta remove target does not exist: {}",
                                    delta_path(op)
                                )));
                            }
                        }
                        MetadataDeltaOp::Splice { .. } => {
                            return Err(ArgosError::Invalid(format!(
                                "metadata delta splice cannot target an inode object: {}",
                                delta_path(op)
                            )));
                        }
                    }
                } else {
                    if pending_inode.as_ref().is_some_and(|(id, _)| *id != ino) {
                        flush_pending_inode(&mut metadata, &mut pending_inode)?;
                    }
                    if pending_inode.is_none() {
                        let inode = metadata.inodes.get(&ino).ok_or_else(|| {
                            ArgosError::Invalid(format!(
                                "metadata delta parent does not exist: /inodes/{key}"
                            ))
                        })?;
                        pending_inode = Some((ino, serde_json::to_value(inode)?));
                    }
                    apply_delta_op(
                        &mut pending_inode.as_mut().expect("pending inode exists").1,
                        &scoped_delta(op, tail),
                    )?;
                }
            }
            [root, key, tail @ ..] if root == "disks" => {
                flush_pending_inode(&mut metadata, &mut pending_inode)?;
                if tail.is_empty() {
                    flush_pending_disk(&mut metadata, &mut pending_disk)?;
                    match op {
                        MetadataDeltaOp::Add { value, .. } => {
                            metadata
                                .disks
                                .insert(key.clone(), serde_json::from_value(value.clone())?);
                        }
                        MetadataDeltaOp::Replace { value, .. } => {
                            if !metadata.disks.contains_key(key) {
                                return Err(ArgosError::Invalid(format!(
                                    "metadata delta replace target does not exist: {}",
                                    delta_path(op)
                                )));
                            }
                            metadata
                                .disks
                                .insert(key.clone(), serde_json::from_value(value.clone())?);
                        }
                        MetadataDeltaOp::Remove { .. } => {
                            if metadata.disks.remove(key).is_none() {
                                return Err(ArgosError::Invalid(format!(
                                    "metadata delta remove target does not exist: {}",
                                    delta_path(op)
                                )));
                            }
                        }
                        MetadataDeltaOp::Splice { .. } => {
                            return Err(ArgosError::Invalid(format!(
                                "metadata delta splice cannot target a disk object: {}",
                                delta_path(op)
                            )));
                        }
                    }
                } else {
                    if pending_disk.as_ref().is_some_and(|(id, _)| id != key) {
                        flush_pending_disk(&mut metadata, &mut pending_disk)?;
                    }
                    if pending_disk.is_none() {
                        let disk = metadata.disks.get(key).ok_or_else(|| {
                            ArgosError::Invalid(format!(
                                "metadata delta parent does not exist: /disks/{key}"
                            ))
                        })?;
                        pending_disk = Some((key.clone(), serde_json::to_value(disk)?));
                    }
                    apply_delta_op(
                        &mut pending_disk.as_mut().expect("pending disk exists").1,
                        &scoped_delta(op, tail),
                    )?;
                }
            }
            _ => {
                flush_pending_inode(&mut metadata, &mut pending_inode)?;
                flush_pending_disk(&mut metadata, &mut pending_disk)?;
                let value = match header.as_mut() {
                    Some(value) => value,
                    None => {
                        let mut page = MetadataHeaderPage::from(&metadata);
                        if typed_allocators.is_some() {
                            page.raw_pool.allocators.clear();
                        }
                        header.insert(serde_json::to_value(page)?)
                    }
                };
                apply_delta_op(value, op)?;
            }
        }
    }
    flush_pending_inode(&mut metadata, &mut pending_inode)?;
    flush_pending_disk(&mut metadata, &mut pending_disk)?;

    if let Some(value) = header {
        // HeaderPage contains all Metadata fields except disks/inodes. The
        // explicit assignment ensures additions to Metadata cannot silently
        // turn recovery into a partial/invalid reconstruction.
        let h: MetadataHeaderPage = serde_json::from_value(value)?;
        metadata.format = h.format;
        metadata.uuid = h.uuid;
        metadata.backend = h.backend;
        metadata.raw_pool = h.raw_pool;
        metadata.created_at = h.created_at;
        metadata.updated_at = h.updated_at;
        metadata.txid = h.txid;
        metadata.next_inode = h.next_inode;
        metadata.next_stripe = h.next_stripe;
        metadata.config = h.config;
        metadata.layouts = h.layouts;
        metadata.current_write_layout = h.current_write_layout;
        metadata.reshape = h.reshape;
        metadata.encryption = h.encryption;
        metadata.integrity = h.integrity;
    }
    if let Some(allocators) = typed_allocators {
        metadata.raw_pool.allocators = allocators;
    }
    if metadata.integrity.hash_scheme == Some(DELTA_INTEGRITY_SCHEME) {
        let hash = delta_integrity_hash(previous, &metadata, delta)?;
        if metadata.integrity.meta_hash != hash {
            return Err(ArgosError::Checksum(
                "raw delta integrity chain digest mismatch".to_string(),
            ));
        }
    } else if metadata.integrity.hash_scheme.is_none() {
        let hash = canonical_metadata_hash(&metadata)?;
        // Legacy v1 journal records can contain floats whose persisted JSON
        // representation changes on a second serde_json parse. Use the same
        // backwards-compatible decoded-hash check as whole-map v1 replay;
        // the fast COW delta path must not reject a valid older record.
        if validate_metadata(&mut metadata, &hash).is_err() {
            let decoded_hash = canonical_metadata_hash_decoded(&metadata)?;
            validate_metadata(&mut metadata, &decoded_hash)?;
        }
    } else {
        return Err(ArgosError::IncompatibleFormat(
            "unsupported raw delta integrity hash scheme".to_string(),
        ));
    }
    // The old JSON round-trip restored a fresh, clean tracking baseline.
    metadata.inodes.mark_committed();
    Ok(metadata)
}

pub(super) fn flush_pending_inode(
    metadata: &mut Metadata,
    pending: &mut Option<(crate::types::InodeId, serde_json::Value)>,
) -> Result<()> {
    if let Some((ino, value)) = pending.take() {
        metadata.inodes.insert(ino, serde_json::from_value(value)?);
    }
    Ok(())
}

pub(super) fn flush_pending_disk(
    metadata: &mut Metadata,
    pending: &mut Option<(String, serde_json::Value)>,
) -> Result<()> {
    if let Some((id, value)) = pending.take() {
        metadata.disks.insert(id, serde_json::from_value(value)?);
    }
    Ok(())
}

pub(super) fn delta_path(op: &MetadataDeltaOp) -> &str {
    match op {
        MetadataDeltaOp::Add { path, .. }
        | MetadataDeltaOp::Remove { path }
        | MetadataDeltaOp::Replace { path, .. }
        | MetadataDeltaOp::Splice { path, .. } => path,
    }
}

pub(super) fn scoped_delta(op: &MetadataDeltaOp, tail: &[String]) -> MetadataDeltaOp {
    let path = json_pointer_from_tokens(tail);
    match op {
        MetadataDeltaOp::Add { value, .. } => MetadataDeltaOp::Add {
            path,
            value: value.clone(),
        },
        MetadataDeltaOp::Remove { .. } => MetadataDeltaOp::Remove { path },
        MetadataDeltaOp::Replace { value, .. } => MetadataDeltaOp::Replace {
            path,
            value: value.clone(),
        },
        MetadataDeltaOp::Splice {
            start,
            delete,
            insert,
            ..
        } => MetadataDeltaOp::Splice {
            path,
            start: *start,
            delete: *delete,
            insert: insert.clone(),
        },
    }
}

pub(super) fn apply_metadata_delta_legacy(
    previous: &Metadata,
    delta: &[MetadataDeltaOp],
) -> Result<Metadata> {
    let mut value = serde_json::to_value(previous)?;
    for op in delta {
        apply_delta_op(&mut value, op)?;
    }
    let mut metadata = serde_json::from_value::<Metadata>(value)?;
    if metadata.integrity.hash_scheme.is_some() {
        return Err(ArgosError::IncompatibleFormat(
            "whole-map raw delta is unsupported with versioned integrity".to_string(),
        ));
    }
    let hash = canonical_metadata_hash(&metadata)?;
    if validate_metadata(&mut metadata, &hash).is_err() {
        let decoded_hash = canonical_metadata_hash_decoded(&metadata)?;
        validate_metadata(&mut metadata, &decoded_hash)?;
    }
    Ok(metadata)
}

pub(super) fn diff_json(
    path: &str,
    previous: &serde_json::Value,
    next: &serde_json::Value,
    ops: &mut Vec<MetadataDeltaOp>,
) {
    match (previous, next) {
        (serde_json::Value::Object(left), serde_json::Value::Object(right)) => {
            for key in left.keys().filter(|key| !right.contains_key(*key)) {
                ops.push(MetadataDeltaOp::Remove {
                    path: push_json_pointer(path, key),
                });
            }
            for (key, right_value) in right {
                let child = push_json_pointer(path, key);
                if let Some(left_value) = left.get(key) {
                    diff_json(&child, left_value, right_value, ops);
                } else {
                    ops.push(MetadataDeltaOp::Add {
                        path: child,
                        value: right_value.clone(),
                    });
                }
            }
        }
        // The raw free-extent list (and file block lists) can contain
        // hundreds of thousands of entries. A middle insertion or bulk
        // deletion cannot be expressed compactly with legacy JSON pointer
        // operations, so pools that advertise the array-splice feature use a
        // single range operation. Other pools keep the legacy encoding.
        (serde_json::Value::Array(left), serde_json::Value::Array(right)) => {
            if array_splice_allowed() {
                push_array_splice(path, left, right, ops);
                return;
            }
            let mut array_ops = Vec::new();
            if !diff_json_array(path, left, right, next, &mut array_ops) {
                ops.push(MetadataDeltaOp::Replace {
                    path: path.to_string(),
                    value: next.clone(),
                });
                return;
            }
            if array_ops.is_empty() {
                return;
            }
            // A sparse delta wins when only a few elements change. Do not
            // assume sparsity: a dense mutation repeats the path and the
            // operation envelope for every element, and a whole-array
            // fallback reserializes every entry. The 2026-11-23 Borg write
            // hit both shapes and regressed from 12 to 90 journal rollovers.
            // Pick the smaller encoding.
            if sparse_array_delta_is_not_smaller(path, next, &array_ops) {
                ops.push(MetadataDeltaOp::Replace {
                    path: path.to_string(),
                    value: next.clone(),
                });
            } else {
                ops.extend(array_ops);
            }
        }
        // Value::PartialEq treats -0.0 and +0.0 as equal, although those
        // serialize to distinct bytes and therefore change the metadata hash.
        _ if !json_values_identical(previous, next) => ops.push(MetadataDeltaOp::Replace {
            path: path.to_string(),
            value: next.clone(),
        }),
        _ => {}
    }
}

/// Collapse one changed array into a single range splice. The unchanged
/// prefix and suffix stay in place, and replay splices the new middle into the
/// old array, so a middle insertion never shifts a large suffix through
/// per-element pointer operations.
pub(super) fn push_array_splice(
    path: &str,
    left: &[serde_json::Value],
    right: &[serde_json::Value],
    ops: &mut Vec<MetadataDeltaOp>,
) {
    // The previous single prefix/suffix splice replaced everything between
    // the first and last change. Sorted allocator free lists change in many
    // scattered runs, so that spanned almost the whole 47k-entry array. Walk
    // the two sequences and keep every unchanged stretch in place.
    const LOOKAHEAD: usize = 32;
    const MAX_NET_WINDOW: usize = 4_096;
    const MAX_RUNS: usize = 8_192;
    // A net deletion or insertion can exceed the fixed lookahead. Extend the
    // matching window by the length difference so a contiguous allocation
    // run is still recognised as one deletion/insertion instead of hundreds
    // of element replacements. Bound the extension to avoid quadratic scan
    // cost on arrays that differ completely.
    let delete_window = LOOKAHEAD + left.len().saturating_sub(right.len()).min(MAX_NET_WINDOW);
    let insert_window = LOOKAHEAD + right.len().saturating_sub(left.len()).min(MAX_NET_WINDOW);
    // (old start, old count, new start, new end)
    let mut runs: Vec<(usize, usize, usize, usize)> = Vec::new();
    let (mut i, mut j) = (0usize, 0usize);
    while i < left.len() || j < right.len() {
        if i < left.len() && j < right.len() && json_values_identical(&left[i], &right[j]) {
            i += 1;
            j += 1;
            continue;
        }
        let old_start = i;
        let new_start = j;
        while i < left.len() || j < right.len() {
            if i < left.len() && j < right.len() && json_values_identical(&left[i], &right[j]) {
                break;
            }
            let insert = (1..=insert_window).find(|offset| {
                i < left.len()
                    && j + offset < right.len()
                    && json_values_identical(&left[i], &right[j + offset])
            });
            let delete = (1..=delete_window).find(|offset| {
                j < right.len()
                    && i + offset < left.len()
                    && json_values_identical(&left[i + offset], &right[j])
            });
            match (insert, delete) {
                (Some(insert), Some(delete)) if insert <= delete => j += insert,
                (Some(_), Some(delete)) => i += delete,
                (Some(insert), None) => j += insert,
                (None, Some(delete)) => i += delete,
                (None, None) => {
                    if i < left.len() {
                        i += 1;
                    }
                    if j < right.len() {
                        j += 1;
                    }
                }
            }
        }
        runs.push((old_start, i - old_start, new_start, j));
        if runs.len() > MAX_RUNS {
            // A genuinely dense rewrite has more changed runs than a
            // replacement. Emit one full-range splice instead of unbounded
            // per-run overhead.
            ops.push(MetadataDeltaOp::Splice {
                path: path.to_string(),
                start: 0,
                delete: left.len() as u64,
                insert: right.to_vec(),
            });
            return;
        }
    }
    let mut shift: isize = 0;
    for (old_start, old_count, new_start, new_end) in runs {
        let insert = right[new_start..new_end].to_vec();
        let start = old_start as isize + shift;
        debug_assert!(start >= 0);
        ops.push(MetadataDeltaOp::Splice {
            path: path.to_string(),
            start: start as u64,
            delete: old_count as u64,
            insert,
        });
        shift += (new_end - new_start) as isize - old_count as isize;
    }
}

/// Emit only the operations needed to rebuild one changed array. Returns
/// `false` when the caller should replace the whole array instead; the caller
/// may also collapse a returned sparse form when it serializes larger.
pub(super) fn diff_json_array(
    path: &str,
    left: &[serde_json::Value],
    right: &[serde_json::Value],
    next: &serde_json::Value,
    ops: &mut Vec<MetadataDeltaOp>,
) -> bool {
    let prefix = left
        .iter()
        .zip(right)
        .take_while(|(a, b)| json_values_identical(a, b))
        .count();
    if prefix == left.len() && prefix == right.len() {
        return true;
    }
    let suffix = left
        .len()
        .saturating_sub(prefix)
        .min(right.len().saturating_sub(prefix));
    let suffix = (0..suffix)
        .take_while(|i| {
            json_values_identical(&left[left.len() - 1 - i], &right[right.len() - 1 - i])
        })
        .count();
    let old_end = left.len() - suffix;
    let new_end = right.len() - suffix;
    let old_middle = old_end - prefix;
    let new_middle = new_end - prefix;
    let planned_ops = if new_middle > old_middle {
        right.len().saturating_sub(left.len()) + left.len().saturating_sub(prefix)
    } else {
        old_middle
    };
    // Building tens of thousands of operations only to discard them in the
    // size guard is wasteful. When the sparse form's lower bound already
    // exceeds a whole-array replacement, fall back before materializing it.
    if planned_ops > 256
        && sparse_array_shift_exceeds_replacement(
            path,
            next,
            planned_ops,
            left.len().max(right.len()),
        )
    {
        return false;
    }
    if new_middle > old_middle {
        // The array grew. Legacy JSON operations cannot insert in place, but
        // an append-and-shift is equivalent: append the final tail first, then
        // overwrite every position before that tail. This is cheap when a
        // freed extent is inserted near the end of a large sorted list, which
        // previously fell back to rewriting the entire 1.9 MB list.
        for (index, value) in right.iter().enumerate().skip(left.len()) {
            ops.push(MetadataDeltaOp::Add {
                path: push_json_pointer(path, &index.to_string()),
                value: value.clone(),
            });
        }
        for (index, value) in right
            .iter()
            .enumerate()
            .skip(prefix)
            .take(left.len().saturating_sub(prefix))
        {
            ops.push(MetadataDeltaOp::Replace {
                path: push_json_pointer(path, &index.to_string()),
                value: value.clone(),
            });
        }
        return true;
    }
    for index in prefix..(prefix + new_middle) {
        diff_json(
            &push_json_pointer(path, &index.to_string()),
            &left[index],
            &right[index],
            ops,
        );
    }
    // Remove surplus elements at one stable index; each removal shifts the
    // suffix left, preserving its exact values and byte ordering. A sparse
    // run of removals is much smaller than reserializing the whole list; the
    // caller's size guard collapses it only when it would not be.
    for _ in new_middle..old_middle {
        ops.push(MetadataDeltaOp::Remove {
            path: push_json_pointer(path, &(prefix + new_middle).to_string()),
        });
    }
    true
}

/// Conservative lower bound for `planned_ops` sparse operations against one
/// whole-array replacement. Returns false when the sparse form is provably
/// smaller without serializing the array.
pub(super) fn sparse_array_shift_exceeds_replacement(
    path: &str,
    next: &serde_json::Value,
    planned_ops: usize,
    element_count: usize,
) -> bool {
    let Ok(encoded_path) = serde_json::to_vec(path) else {
        return true;
    };
    // Remove needs 23 bytes plus the path; Add/Replace need at least as many
    // and always carry a value of at least one byte.
    let lower = planned_ops.saturating_mul(24usize.saturating_add(encoded_path.len()));
    // A JSON array is never shorter than its element count.
    if lower < element_count {
        return false;
    }
    let Ok(value) = serde_json::to_vec(next) else {
        return true;
    };
    lower
        >= 33usize
            .saturating_add(encoded_path.len())
            .saturating_add(value.len())
}

/// Compare the serialized sparse operations with one serialized
/// `Replace { path, value: next }`. The comparison is exact, but avoids
/// serializing a large array when the sparse form is already below the
/// replacement's lower bound (every JSON array element is at least one byte).
pub(super) fn sparse_array_delta_is_not_smaller(
    path: &str,
    next: &serde_json::Value,
    ops: &[MetadataDeltaOp],
) -> bool {
    let Ok(sparse) = serde_json::to_vec(ops) else {
        return true;
    };
    let Ok(encoded_path) = serde_json::to_vec(path) else {
        return true;
    };
    // {"op":"replace","path":<path>,"value":<value>}
    let envelope = 33usize.saturating_add(encoded_path.len());
    let minimum_value = next.as_array().map_or(2, |items| items.len().max(1));
    // The sparse vector's serialization includes two bytes for its own
    // brackets; the replacement operation is a single member of the parent
    // vector, so account for that difference.
    let sparse_cost = sparse.len().saturating_sub(2);
    if sparse_cost < envelope.saturating_add(minimum_value) {
        return false;
    }
    let Ok(value) = serde_json::to_vec(next) else {
        return true;
    };
    sparse_cost >= envelope.saturating_add(value.len())
}

// Value's numeric equality ignores the sign bit of zero, but a journal
// delta must reproduce exact serialized bytes to validate integrity hashes.
pub(super) fn json_values_identical(a: &serde_json::Value, b: &serde_json::Value) -> bool {
    match (a, b) {
        (serde_json::Value::Number(left), serde_json::Value::Number(right))
            if left.is_f64() && right.is_f64() =>
        {
            left.as_f64().map(f64::to_bits) == right.as_f64().map(f64::to_bits)
        }
        (serde_json::Value::Array(left), serde_json::Value::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(a, b)| json_values_identical(a, b))
        }
        (serde_json::Value::Object(left), serde_json::Value::Object(right)) => {
            left.len() == right.len()
                && left.iter().all(|(key, value)| {
                    right
                        .get(key)
                        .is_some_and(|other| json_values_identical(value, other))
                })
        }
        _ => a == b,
    }
}

pub(super) fn apply_delta_op(value: &mut serde_json::Value, op: &MetadataDeltaOp) -> Result<()> {
    match op {
        MetadataDeltaOp::Add { path, value: next } => set_json_pointer(value, path, next.clone()),
        MetadataDeltaOp::Replace { path, value: next } => {
            if path.is_empty() {
                *value = next.clone();
                Ok(())
            } else if get_json_pointer_mut(value, path).is_some() {
                set_json_pointer(value, path, next.clone())
            } else {
                Err(ArgosError::Invalid(format!(
                    "metadata delta replace target does not exist: {path}"
                )))
            }
        }
        MetadataDeltaOp::Remove { path } => remove_json_pointer(value, path),
        MetadataDeltaOp::Splice {
            path,
            start,
            delete,
            insert,
        } => splice_json_array(value, path, *start, *delete, insert),
    }
}

pub(super) fn splice_json_array(
    value: &mut serde_json::Value,
    path: &str,
    start: u64,
    delete: u64,
    insert: &[serde_json::Value],
) -> Result<()> {
    let target = if path.is_empty() {
        value
    } else {
        get_json_pointer_mut(value, path).ok_or_else(|| {
            ArgosError::Invalid(format!(
                "metadata delta splice target does not exist: {path}"
            ))
        })?
    };
    let items = target.as_array_mut().ok_or_else(|| {
        ArgosError::Invalid(format!(
            "metadata delta splice target is not an array: {path}"
        ))
    })?;
    let start = usize::try_from(start).map_err(|_| {
        ArgosError::Invalid(format!("metadata delta splice start overflows: {path}"))
    })?;
    let delete = usize::try_from(delete).map_err(|_| {
        ArgosError::Invalid(format!("metadata delta splice delete overflows: {path}"))
    })?;
    let end = start.checked_add(delete).ok_or_else(|| {
        ArgosError::Invalid(format!("metadata delta splice range overflows: {path}"))
    })?;
    if start > items.len() || end > items.len() {
        return Err(ArgosError::Invalid(format!(
            "metadata delta splice range {start}..{end} outside array length {}: {path}",
            items.len()
        )));
    }
    items.splice(start..end, insert.iter().cloned());
    Ok(())
}

pub(super) fn set_json_pointer(
    value: &mut serde_json::Value,
    path: &str,
    next: serde_json::Value,
) -> Result<()> {
    if path.is_empty() {
        *value = next;
        return Ok(());
    }
    let mut parts = json_pointer_tokens(path)?;
    let key = parts
        .pop()
        .ok_or_else(|| ArgosError::Invalid("empty metadata delta path".to_string()))?;
    let parent_path = json_pointer_from_tokens(&parts);
    let parent = get_json_pointer_mut(value, &parent_path).ok_or_else(|| {
        ArgosError::Invalid(format!(
            "metadata delta parent does not exist: {parent_path}"
        ))
    })?;
    match parent {
        serde_json::Value::Object(map) => {
            map.insert(key, next);
            Ok(())
        }
        serde_json::Value::Array(items) => {
            let index = key.parse::<usize>().map_err(|_| {
                ArgosError::Invalid(format!("metadata delta array index is invalid: {key}"))
            })?;
            if index > items.len() {
                return Err(ArgosError::Invalid(format!(
                    "metadata delta array index out of bounds: {index}"
                )));
            }
            if index == items.len() {
                items.push(next);
            } else {
                items[index] = next;
            }
            Ok(())
        }
        _ => Err(ArgosError::Invalid(format!(
            "metadata delta parent is not a container: {parent_path}"
        ))),
    }
}

pub(super) fn remove_json_pointer(value: &mut serde_json::Value, path: &str) -> Result<()> {
    if path.is_empty() {
        *value = serde_json::Value::Null;
        return Ok(());
    }
    let mut parts = json_pointer_tokens(path)?;
    let key = parts
        .pop()
        .ok_or_else(|| ArgosError::Invalid("empty metadata delta path".to_string()))?;
    let parent_path = json_pointer_from_tokens(&parts);
    let parent = get_json_pointer_mut(value, &parent_path).ok_or_else(|| {
        ArgosError::Invalid(format!(
            "metadata delta parent does not exist: {parent_path}"
        ))
    })?;
    match parent {
        serde_json::Value::Object(map) => map.remove(&key).map(|_| ()).ok_or_else(|| {
            ArgosError::Invalid(format!(
                "metadata delta remove target does not exist: {path}"
            ))
        }),
        serde_json::Value::Array(items) => {
            let index = key.parse::<usize>().map_err(|_| {
                ArgosError::Invalid(format!("metadata delta array index is invalid: {key}"))
            })?;
            if index >= items.len() {
                return Err(ArgosError::Invalid(format!(
                    "metadata delta array index out of bounds: {index}"
                )));
            }
            items.remove(index);
            Ok(())
        }
        _ => Err(ArgosError::Invalid(format!(
            "metadata delta parent is not a container: {parent_path}"
        ))),
    }
}

pub(super) fn get_json_pointer_mut<'a>(
    value: &'a mut serde_json::Value,
    path: &str,
) -> Option<&'a mut serde_json::Value> {
    if path.is_empty() {
        return Some(value);
    }
    value.pointer_mut(path)
}

pub(super) fn push_json_pointer(path: &str, key: &str) -> String {
    let escaped = key.replace('~', "~0").replace('/', "~1");
    if path.is_empty() {
        format!("/{escaped}")
    } else {
        format!("{path}/{escaped}")
    }
}

pub(super) fn json_pointer_tokens(path: &str) -> Result<Vec<String>> {
    if path.is_empty() {
        return Ok(Vec::new());
    }
    if !path.starts_with('/') {
        return Err(ArgosError::Invalid(format!(
            "metadata delta path is not a JSON pointer: {path}"
        )));
    }
    path.split('/')
        .skip(1)
        .map(|part| Ok(part.replace("~1", "/").replace("~0", "~")))
        .collect()
}

pub(super) fn json_pointer_from_tokens(tokens: &[String]) -> String {
    tokens
        .iter()
        .fold(String::new(), |path, token| push_json_pointer(&path, token))
}
