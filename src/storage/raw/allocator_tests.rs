use super::*;

#[test]
fn allocator_reuses_freed_extents_and_reports_enospc() {
    let mut state = init_allocator(4096, 16 * 4096, 4096);
    let first = allocate(&mut state, "disk-0000", 1000, 1).unwrap();
    let second = allocate(&mut state, "disk-0000", 1000, 1).unwrap();
    assert_ne!(first.offset, second.offset);
    free(&mut state, &first).unwrap();
    let reused = allocate(&mut state, "disk-0000", 1000, 2).unwrap();
    assert_eq!(reused.offset, first.offset);
    assert!(matches!(
        allocate(&mut state, "disk-0000", 128 * 4096, 1).unwrap_err(),
        ArgosError::DiskFull { .. }
    ));
}

#[test]
fn allocator_detects_overlaps() {
    let state = init_allocator(4096, 16 * 4096, 4096);
    let a = PhysicalExtent {
        disk_id: "disk-0000".to_string(),
        offset: 4096,
        length: 8192,
        generation: 1,
        flags: 0,
    };
    let b = PhysicalExtent {
        offset: 8192,
        ..a.clone()
    };
    assert!(matches!(
        validate_allocations(&state, vec![a, b]).unwrap_err(),
        ArgosError::CorruptedMetadata(_)
    ));
}

#[test]
fn allocator_detects_free_list_overlapping_allocated_extent() {
    let mut state = init_allocator(4096, 16 * 4096, 4096);
    let allocated = allocate(&mut state, "disk-0000", 4096, 1).unwrap();
    state.free_extents.push(RawFreeExtent {
        offset: allocated.offset,
        length: allocated.length,
    });
    assert!(matches!(
        validate_allocations(&state, vec![allocated]).unwrap_err(),
        ArgosError::CorruptedMetadata(_)
    ));
}

#[test]
fn allocator_free_and_validation_reject_all_out_of_region_cases() {
    let mut state = init_allocator(4096, 4 * 4096, 4096);
    let before = PhysicalExtent {
        disk_id: "disk-0000".to_string(),
        offset: 0,
        length: 4096,
        generation: 1,
        flags: 0,
    };
    let after = PhysicalExtent {
        offset: state.data_end,
        ..before.clone()
    };
    assert!(free(&mut state, &before).is_err());
    assert!(free(&mut state, &after).is_err());
    let zero = PhysicalExtent {
        offset: state.data_start,
        length: 0,
        ..before.clone()
    };
    free(&mut state, &zero).unwrap();
    assert!(state.free_extents.is_empty());

    assert!(validate_allocations(&state, vec![before.clone()]).is_err());
    assert!(validate_allocations(&state, vec![after.clone()]).is_err());

    state.free_extents = vec![RawFreeExtent {
        offset: 0,
        length: 4096,
    }];
    assert!(validate_allocations(&state, Vec::new()).is_err());
    state.free_extents = vec![RawFreeExtent {
        offset: state.data_end,
        length: 4096,
    }];
    assert!(validate_allocations(&state, Vec::new()).is_err());
    state.free_extents = vec![
        RawFreeExtent {
            offset: 4096,
            length: 8192,
        },
        RawFreeExtent {
            offset: 8192,
            length: 4096,
        },
    ];
    assert!(validate_allocations(&state, Vec::new()).is_err());
}

#[test]
fn allocator_coalesces_adjacent_overlapping_and_partial_free_extents() {
    let mut state = init_allocator(4096, 8 * 4096, 4096);
    let full = allocate(&mut state, "disk", 3 * 4096, 1).unwrap();
    let middle = PhysicalExtent {
        offset: full.offset + 4096,
        length: 4096,
        ..full.clone()
    };
    let first = PhysicalExtent {
        length: 4096,
        ..full.clone()
    };
    let last = PhysicalExtent {
        offset: full.offset + 2 * 4096,
        length: 4096,
        ..full
    };
    free(&mut state, &middle).unwrap();
    free(&mut state, &first).unwrap();
    free(&mut state, &last).unwrap();
    assert_eq!(state.free_extents.len(), 1);
    assert_eq!(state.free_extents[0].offset, 4096);
    assert_eq!(state.free_extents[0].length, 3 * 4096);

    let partial = allocate(&mut state, "disk", 4096, 2).unwrap();
    assert_eq!(partial.offset, 4096);
    assert_eq!(state.free_extents[0].offset, 8192);
    assert_eq!(state.free_extents[0].length, 2 * 4096);
}

#[test]
fn allocator_validates_large_fragmented_namespace_in_linear_scan() {
    // Vincecate's 294k-inode pool had 270,879 free extents. The old nested
    // validation loop required >70 billion interval comparisons per member.
    const COUNT: u64 = 270_879;
    let mut state = init_allocator(4096, (COUNT * 2 + 2) * 4096, 4096);
    state.free_extents = (0..COUNT)
        .map(|i| RawFreeExtent {
            offset: (2 * i + 2) * 4096,
            length: 4096,
        })
        .collect();
    let allocated = (0..COUNT)
        .map(|i| PhysicalExtent {
            disk_id: "disk-0000".into(),
            offset: (2 * i + 1) * 4096,
            length: 4096,
            generation: 1,
            flags: 0,
        })
        .collect::<Vec<_>>();
    assert!(validate_allocations(&state, allocated.clone()).is_ok());
    // An overlap near the end must still be caught after scanning almost
    // all extents (not just the first few entries).
    let mut damaged = allocated;
    damaged[COUNT as usize - 1].offset += 4096;
    assert!(matches!(
        validate_allocations(&state, damaged),
        Err(ArgosError::CorruptedMetadata(_))
    ));
}

#[test]
fn allocator_linear_sweep_preserves_overlapping_range_detection() {
    let mut state = init_allocator(4096, 64 * 4096, 4096);
    state.free_extents = vec![
        RawFreeExtent {
            offset: 40 * 4096,
            length: 3 * 4096,
        },
        RawFreeExtent {
            offset: 6 * 4096,
            length: 2 * 4096,
        },
    ];
    let example = |offset: u64, length: u64| PhysicalExtent {
        disk_id: "disk-0000".into(),
        offset,
        length,
        generation: 1,
        flags: 0,
    };
    assert!(validate_allocations(
        &state,
        vec![example(4 * 4096, 4096), example(10 * 4096, 4096)]
    )
    .is_ok());
    for (offset, length) in [
        (5 * 4096, 2 * 4096),  // intersects first free region at right edge
        (7 * 4096, 4096),      // entirely inside free region
        (39 * 4096, 2 * 4096), // intersects second free region at left edge
        (42 * 4096, 4096),     // final page of second free region
    ] {
        assert!(matches!(
            validate_allocations(&state, vec![example(offset, length)]),
            Err(ArgosError::CorruptedMetadata(_))
        ));
    }
}

#[test]
fn allocator_sweep_matches_reference_overlap_check_at_boundaries() {
    let mut state = init_allocator(4096, 64 * 4096, 4096);
    state.free_extents = vec![
        RawFreeExtent {
            offset: 11 * 4096,
            length: 2 * 4096,
        },
        RawFreeExtent {
            offset: 37 * 4096,
            length: 5 * 4096,
        },
        RawFreeExtent {
            offset: 24 * 4096,
            length: 4096,
        },
    ];
    for offset in (4096..(64 * 4096)).step_by(1024) {
        for length in [0, 1, 1024, 4096, 8192, 65536] {
            if offset + length > state.data_end {
                continue;
            }
            let original_overlap = state.free_extents.iter().any(|free| {
                offset < free.offset.saturating_add(free.length)
                    && free.offset < offset.saturating_add(length)
            });
            let candidate = PhysicalExtent {
                disk_id: "disk-0000".into(),
                offset,
                length,
                generation: 1,
                flags: 0,
            };
            assert_eq!(
                validate_allocations(&state, vec![candidate]).is_err(),
                original_overlap,
                "offset={offset}, length={length}"
            );
        }
    }
}

#[test]
fn local_free_coalescing_matches_full_sort_and_merge() {
    let mut state = init_allocator(4096, 4096 * 8192, 4096);
    state.free_extents = (0..2048)
        .map(|i| RawFreeExtent {
            offset: (i * 3 + 1) * 4096,
            length: 4096,
        })
        .collect();
    let mut reference = state.clone();
    // Exercise insertions, bridges, double frees, and ranges covering many
    // neighbors, with deterministic pseudo-random placement.
    let mut rng = 0x1234_5678_u64;
    for i in 0..512 {
        rng = rng.wrapping_mul(6364136223846793005).wrapping_add(1);
        let extent = PhysicalExtent {
            disk_id: "disk".into(),
            offset: ((rng >> 32) % 6000 + 1) * 4096,
            length: (i % 17 + 1) * 4096,
            generation: 1,
            flags: 0,
        };
        if i == 200 {
            // Legacy unsorted input takes the full normalization fallback.
            state.free_extents.reverse();
            reference.free_extents.reverse();
        }
        free(&mut state, &extent).unwrap();
        reference.free_extents.push(RawFreeExtent {
            offset: extent.offset,
            length: extent.length,
        });
        coalesce(&mut reference);
        assert_eq!(state.free_extents, reference.free_extents, "step {i}");
    }
}

#[test]
fn fragmented_free_reuses_existing_vector_storage() {
    let mut state = init_allocator(4096, 4096 * 150_000, 4096);
    state.free_extents = (0..47_000)
        .map(|i| RawFreeExtent {
            offset: (i * 3 + 1) * 4096,
            length: 4096,
        })
        .collect();
    state.free_extents.reserve(1);
    let ptr = state.free_extents.as_ptr();
    free(
        &mut state,
        &PhysicalExtent {
            disk_id: "disk".into(),
            offset: 2 * 4096,
            length: 2 * 4096,
            generation: 1,
            flags: 0,
        },
    )
    .unwrap();
    assert_eq!(state.free_extents.as_ptr(), ptr);
    assert_eq!(state.free_extents.len(), 46_999);
    assert_eq!(state.free_extents[0].length, 4 * 4096);
}
