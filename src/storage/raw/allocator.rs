use crate::error::{ArgosError, Result};
use crate::raw_format::align_up;
use crate::types::{PhysicalExtent, RawAllocatorState, RawFreeExtent};

pub fn init_allocator(data_start: u64, data_length: u64, block_size: u64) -> RawAllocatorState {
    RawAllocatorState {
        block_size,
        data_start,
        data_end: data_start.saturating_add(data_length),
        next_offset: data_start,
        free_extents: Default::default(),
    }
}

pub fn allocate(
    state: &mut RawAllocatorState,
    disk_id: &str,
    length: u64,
    generation: u64,
) -> Result<PhysicalExtent> {
    let block_size = state.block_size.max(4096);
    let length = align_up(length.max(1), block_size);
    let candidate = if state
        .free_extents
        .known_length_upper_bound()
        .is_some_and(|bound| bound < length)
    {
        None
    } else if let Some(found) = state.free_extents.indexed_fit(length, block_size) {
        if found.is_none() {
            if let Some(index) = state.free_extents.take_fit_index() {
                state.free_extents.restore_fit_index(index);
            }
        }
        found
    } else {
        let mut max_seen = 0;
        let found = state
            .free_extents
            .iter()
            .enumerate()
            .find(|(_, free)| {
                max_seen = max_seen.max(free.length);
                free.length >= length && free.offset % block_size == 0
            })
            .map(|(index, free)| (index, free.clone()));
        if found.is_none() {
            // No match means the entire list was visited; max_seen is exact.
            state.free_extents.mark_length_upper_bound(max_seen);
        }
        found
    };
    if let Some((index, free)) = candidate {
        let offset = free.offset;
        let preserve_proof = state.free_extents.known_normalized();
        let old_bound = state.free_extents.known_length_upper_bound();
        let mut offset_index = state.free_extents.take_offset_index();
        if let Some(index) = &mut offset_index {
            index.remove(&free.offset);
            if free.length > length {
                let next = RawFreeExtent {
                    offset: free.offset + length,
                    length: free.length - length,
                };
                index.insert(next.offset, next);
            }
        }
        let mut fit_index = state.free_extents.take_fit_index();
        if let Some(index) = &mut fit_index {
            index.remove(&free);
            if free.length > length {
                index.insert(&RawFreeExtent {
                    offset: free.offset.saturating_add(length),
                    length: free.length - length,
                });
            }
        }
        if free.length == length {
            state.free_extents.remove(index);
        } else if let Some(slot) = state.free_extents.get_mut(index) {
            slot.offset = slot.offset.saturating_add(length);
            slot.length = slot.length.saturating_sub(length);
        }
        if preserve_proof {
            state.free_extents.mark_normalized();
        }
        // Allocation cannot enlarge an existing free extent.
        if let Some(bound) = old_bound {
            state.free_extents.mark_length_upper_bound(bound);
        }
        if let Some(index) = fit_index {
            state.free_extents.restore_fit_index(index);
        }
        if let Some(index) = offset_index {
            state.free_extents.restore_offset_index(index);
        }
        return Ok(PhysicalExtent {
            disk_id: disk_id.to_string(),
            offset,
            length,
            generation,
            flags: 0,
        });
    }

    let offset = align_up(state.next_offset, block_size);
    let end = offset
        .checked_add(length)
        .ok_or_else(|| ArgosError::Invalid("extent allocation overflow".to_string()))?;
    if end > state.data_end {
        return Err(ArgosError::DiskFull {
            disk_id: disk_id.to_string(),
            required: length,
            available: state.data_end.saturating_sub(offset),
        });
    }
    state.next_offset = end;
    Ok(PhysicalExtent {
        disk_id: disk_id.to_string(),
        offset,
        length,
        generation,
        flags: 0,
    })
}

pub fn free(state: &mut RawAllocatorState, extent: &PhysicalExtent) -> Result<()> {
    if extent.offset < state.data_start
        || extent.offset.saturating_add(extent.length) > state.data_end
    {
        return Err(ArgosError::Invalid(format!(
            "extent {}+{} outside allocator region {}+{}",
            extent.offset,
            extent.length,
            state.data_start,
            state.data_end.saturating_sub(state.data_start)
        )));
    }
    if extent.length == 0 {
        return Ok(());
    }
    let mut start = extent.offset;
    let mut end = extent.offset.saturating_add(extent.length);
    let free = &mut state.free_extents;
    let old_bound = free.known_length_upper_bound();
    // Existing pools can contain unsorted or uncoalesced lists. Keep the
    // old normalization semantics for these exceptional inputs; generated
    // free lists are sorted and strictly separated after every mutation.
    if !free.is_normalized() {
        free.push_back(RawFreeExtent {
            offset: start,
            length: extent.length,
        });
        coalesce(state);
        return Ok(());
    }
    // Only neighbors intersecting or touching the new range need merging.
    // Reuse the existing allocation instead of sorting and rebuilding a
    // potentially hundreds-of-thousands-entry free list on each reclamation.
    let mut low = 0;
    let mut high = free.len();
    while low < high {
        let middle = low + (high - low) / 2;
        let entry = &free[middle];
        if entry.offset.saturating_add(entry.length) < start {
            low = middle + 1;
        } else {
            high = middle;
        }
    }
    let first = low;
    let mut last = first;
    while let Some(entry) = free.get(last) {
        if entry.offset > end {
            break;
        }
        start = start.min(entry.offset);
        end = end.max(entry.offset.saturating_add(entry.length));
        last += 1;
    }
    let replacement = RawFreeExtent {
        offset: start,
        length: end.saturating_sub(start),
    };
    let merged_length = replacement.length;
    let mut offset_index = free.take_offset_index();
    if let Some(index) = &mut offset_index {
        for i in first..last {
            index.remove(&free[i].offset);
        }
        index.insert(replacement.offset, replacement.clone());
    }
    let mut fit_index = free.take_fit_index();
    if let Some(index) = &mut fit_index {
        for i in first..last {
            index.remove(&free[i]);
        }
        index.insert(&replacement);
    }
    if last == first + 1 {
        free[first] = replacement;
    } else if first == last {
        free.insert(first, replacement);
    } else {
        let right = free.split_off(last);
        free.truncate(first);
        free.push_back(replacement);
        free.append(right);
    }
    free.mark_normalized();
    if let Some(bound) = old_bound {
        free.mark_length_upper_bound(bound.max(merged_length));
    }
    if let Some(index) = fit_index {
        free.restore_fit_index(index);
    }
    if let Some(index) = offset_index {
        free.restore_offset_index(index);
    }
    Ok(())
}

pub fn validate_allocations(
    state: &RawAllocatorState,
    mut extents: Vec<PhysicalExtent>,
) -> Result<()> {
    extents.sort_by_key(|extent| extent.offset);
    let mut previous_end = state.data_start;
    for extent in &extents {
        if extent.offset < state.data_start
            || extent.offset.saturating_add(extent.length) > state.data_end
        {
            return Err(ArgosError::CorruptedMetadata(format!(
                "allocated extent outside data region on {}: {}+{}",
                extent.disk_id, extent.offset, extent.length
            )));
        }
        if extent.offset < previous_end {
            return Err(ArgosError::CorruptedMetadata(format!(
                "overlapping allocated extent on {} at {}",
                extent.disk_id, extent.offset
            )));
        }
        previous_end = extent.offset.saturating_add(extent.length);
    }
    let mut free_extents = state.free_extents.iter().cloned().collect::<Vec<_>>();
    free_extents.sort_by_key(|extent| extent.offset);
    let mut previous_free_end = state.data_start;
    for free in &free_extents {
        if free.offset < state.data_start
            || free.offset.saturating_add(free.length) > state.data_end
        {
            return Err(ArgosError::CorruptedMetadata(format!(
                "free extent outside data region: {}+{}",
                free.offset, free.length
            )));
        }
        if free.offset < previous_free_end {
            return Err(ArgosError::CorruptedMetadata(format!(
                "overlapping free extent at {}",
                free.offset
            )));
        }
        previous_free_end = free.offset.saturating_add(free.length);
    }
    // Both lists are sorted, and each was verified non-overlapping above.
    // Advance a single cursor rather than comparing every allocated extent
    // against every free extent. On large pools the latter is quadratic in
    // hundreds of thousands of allocations and free-list entries.
    let mut free_cursor = 0;
    for allocated in &extents {
        while let Some(free) = free_extents.get(free_cursor) {
            if free.offset.saturating_add(free.length) > allocated.offset {
                break;
            }
            free_cursor += 1;
        }
        if let Some(free) = free_extents.get(free_cursor) {
            let allocated_end = allocated.offset.saturating_add(allocated.length);
            let free_end = free.offset.saturating_add(free.length);
            if allocated.offset < free_end && free.offset < allocated_end {
                return Err(ArgosError::CorruptedMetadata(format!(
                    "allocated extent on {} overlaps allocator free list at {}+{}",
                    allocated.disk_id, free.offset, free.length
                )));
            }
        }
    }
    Ok(())
}

/// Plan recovery of allocated-but-unreferenced bytes below the high-water
/// mark. Never turn an overlap or malformed allocation into free space.
pub(crate) fn orphan_reclaim_plan(
    state: &RawAllocatorState,
    mut live: Vec<PhysicalExtent>,
) -> Result<(u64, Option<RawAllocatorState>)> {
    validate_allocations(state, live.clone())?;
    let block = state.block_size.max(4096);
    let valid_range = |offset: u64, length: u64| {
        length > 0
            && offset >= state.data_start
            && offset.is_multiple_of(block)
            && length.is_multiple_of(block)
            && offset
                .checked_add(length)
                .is_some_and(|end| end <= state.next_offset)
    };
    if state.next_offset < state.data_start
        || state.next_offset > state.data_end
        || !state.next_offset.is_multiple_of(block)
        || !state.data_start.is_multiple_of(block)
        || live.iter().any(|e| !valid_range(e.offset, e.length))
        || state
            .free_extents
            .iter()
            .any(|e| !valid_range(e.offset, e.length))
    {
        return Err(ArgosError::CorruptedMetadata(
            "invalid allocator high-water mark or extent alignment".into(),
        ));
    }
    live.sort_by_key(|extent| extent.offset);
    let mut cursor = state.data_start;
    let mut free = Vec::new();
    for extent in live {
        if extent.offset > cursor {
            free.push(RawFreeExtent {
                offset: cursor,
                length: extent.offset - cursor,
            });
        }
        cursor = extent.offset + extent.length;
    }
    if cursor < state.next_offset {
        free.push(RawFreeExtent {
            offset: cursor,
            length: state.next_offset - cursor,
        });
    }
    let bytes: u64 = free.iter().map(|e| e.length).sum();
    let known: u64 = state.free_extents.iter().map(|e| e.length).sum();
    let orphan_bytes = bytes.checked_sub(known).ok_or_else(|| {
        ArgosError::CorruptedMetadata(
            "allocator free space exceeds unreferenced extent space".into(),
        )
    })?;
    if orphan_bytes == 0 {
        return Ok((0, None));
    }
    let mut repaired = state.clone();
    repaired.free_extents = free.into_iter().collect();
    repaired.free_extents.mark_normalized();
    Ok((orphan_bytes, Some(repaired)))
}

fn coalesce(state: &mut RawAllocatorState) {
    let mut extents = std::mem::take(&mut state.free_extents)
        .into_iter()
        .collect::<Vec<_>>();
    extents.sort_by_key(|extent| extent.offset);
    let mut merged: Vec<RawFreeExtent> = Vec::new();
    for extent in extents {
        if let Some(last) = merged.last_mut() {
            let last_end = last.offset.saturating_add(last.length);
            if extent.offset <= last_end {
                let new_end = last_end.max(extent.offset.saturating_add(extent.length));
                last.length = new_end.saturating_sub(last.offset);
                continue;
            }
        }
        merged.push(extent);
    }
    let max_length = merged.iter().map(|extent| extent.length).max().unwrap_or(0);
    state.free_extents = merged.into_iter().collect();
    state.free_extents.mark_normalized();
    state.free_extents.mark_length_upper_bound(max_length);
}

#[cfg(test)]
#[path = "allocator_tests.rs"]
mod tests;
