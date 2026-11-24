use crate::error::{ArgosError, Result};
use crate::raw_format::align_up;
use crate::types::{PhysicalExtent, RawAllocatorState, RawFreeExtent};

pub fn init_allocator(data_start: u64, data_length: u64, block_size: u64) -> RawAllocatorState {
    RawAllocatorState {
        block_size,
        data_start,
        data_end: data_start.saturating_add(data_length),
        next_offset: data_start,
        free_extents: imbl::Vector::new(),
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
    if let Some((index, free)) = state
        .free_extents
        .iter()
        .enumerate()
        .find(|(_, free)| free.length >= length && free.offset % block_size == 0)
        .map(|(index, free)| (index, free.clone()))
    {
        let offset = free.offset;
        if free.length == length {
            state.free_extents.remove(index);
        } else if let Some(slot) = state.free_extents.get_mut(index) {
            slot.offset = slot.offset.saturating_add(length);
            slot.length = slot.length.saturating_sub(length);
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
    // Existing pools can contain unsorted or uncoalesced lists. Keep the
    // old normalization semantics for these exceptional inputs; generated
    // free lists are sorted and strictly separated after every mutation.
    if !free
        .iter()
        .zip(free.iter().skip(1))
        .all(|(left, right)| left.offset.saturating_add(left.length) < right.offset)
    {
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
    state.free_extents = merged.into_iter().collect();
}

#[cfg(test)]
#[path = "allocator_tests.rs"]
mod tests;
