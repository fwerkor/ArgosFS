#![no_main]

mod common;

use argosfs::journal;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut previous = common::metadata_from(data, 0);
    common::add_small_inode_tree(&mut previous, data, 128);
    common::prepare(&mut previous);

    let mut next = previous.clone();
    next.txid = previous
        .txid
        .saturating_add(1 + common::u64_at(data, 512) % 8);
    next.updated_at = previous.updated_at + (common::u64_at(data, 520) % 1000) as f64;
    next.next_stripe = common::u64_at(data, 528);
    next.raw_pool.pool_name = common::text_at(data, 536, 40, "delta-pool");
    next.config.k = 1 + (common::byte(data, 584) as usize % 6);
    next.config.m = common::byte(data, 585) as usize % 4;
    next.config.chunk_size = 4096usize << (common::byte(data, 586) as usize % 7);
    next.config.fsname = common::text_at(data, 588, 32, "delta-fs");
    next.config.direct_io = common::byte(data, 624) & 1 != 0;
    next.config.zero_copy = common::byte(data, 625) & 1 != 0;
    next.config.defer_metadata_commit = common::byte(data, 626) & 1 != 0;
    next.integrity = Default::default();
    journal::prepare_metadata_integrity_with_previous(
        &mut next,
        previous.integrity.meta_hash.clone(),
    )
    .expect("generated next metadata must be hashable");

    let delta = journal::metadata_delta(&previous, &next).expect("metadata diff must succeed");
    let replayed = journal::apply_metadata_delta(&previous, &delta)
        .expect("delta generated from valid metadata must replay");
    assert_eq!(
        serde_json::to_value(&next).unwrap(),
        serde_json::to_value(&replayed).unwrap()
    );

    let hash1 = journal::canonical_metadata_hash(&next).unwrap();
    let hash2 = journal::canonical_metadata_hash(&replayed).unwrap();
    assert_eq!(hash1, hash2);
});
