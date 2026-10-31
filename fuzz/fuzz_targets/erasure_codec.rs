#![no_main]

mod common;

use argosfs::erasure::RsCodec;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let k = 1 + (common::byte(data, 0) as usize % 8);
    let m = common::byte(data, 1) as usize % 5;
    let shard_size = 1 + common::u64_at(data, 2) as usize % 2048;
    let codec = RsCodec::new(k, m).expect("bounded erasure parameters must be valid");

    let mut original = Vec::with_capacity(k);
    for shard_index in 0..k {
        let shard = (0..shard_size)
            .map(|offset| common::byte(data, 16 + shard_index * 17 + offset))
            .collect::<Vec<_>>();
        original.push(shard);
    }

    let encoded = codec
        .encode(&original)
        .expect("equal-sized non-empty shards must encode");

    // Keep invalid-shape rejection on the coverage map without treating the
    // documented error path as a fuzz finding.
    if k > 1 {
        let mut uneven = original.clone();
        uneven[0].push(common::byte(data, 15));
        assert!(codec.encode(&uneven).is_err());
    }
    assert_eq!(encoded.len(), k + m);
    let mut missing = encoded.iter().cloned().map(Some).collect::<Vec<_>>();

    let remove_count = if m == 0 {
        0
    } else {
        common::byte(data, 3) as usize % (m + 1)
    };
    let mut removed = 0usize;
    let mut cursor = 4usize;
    while removed < remove_count {
        let index = common::byte(data, cursor) as usize % missing.len();
        cursor += 1;
        if missing[index].take().is_some() {
            removed += 1;
        }
        if cursor > data.len().saturating_add(32) {
            break;
        }
    }

    let reconstructed = codec
        .reconstruct(missing)
        .expect("no more than parity-count missing shards must reconstruct");
    assert_eq!(&reconstructed[..k], original.as_slice());

    if m > 0 {
        let mut too_many = encoded.into_iter().map(Some).collect::<Vec<_>>();
        for slot in too_many.iter_mut().take((m + 1).min(k + m)) {
            *slot = None;
        }
        let _ = codec.reconstruct(too_many);
    }
});
