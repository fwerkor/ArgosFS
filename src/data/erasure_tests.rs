use super::*;

#[test]
fn codec_rejects_zero_data_shards() {
    assert!(matches!(RsCodec::new(0, 1), Err(ArgosError::Invalid(_))));
}

#[test]
fn no_parity_layout_round_trips_and_rejects_missing_shards() {
    let codec = RsCodec::new(2, 0).unwrap();
    assert_eq!(codec.total(), 2);
    let data = vec![b"aaaa".to_vec(), b"bbbb".to_vec()];
    assert_eq!(codec.encode(&data).unwrap(), data);
    assert_eq!(
        codec
            .reconstruct(vec![Some(b"aaaa".to_vec()), Some(b"bbbb".to_vec())])
            .unwrap(),
        data
    );
    assert!(matches!(
        codec.reconstruct(vec![Some(b"aaaa".to_vec()), None]),
        Err(ArgosError::Erasure(_))
    ));
}

#[test]
fn parity_layout_reconstructs_data_and_parity() {
    let codec = RsCodec::new(2, 1).unwrap();
    let original = vec![b"aaaa".to_vec(), b"bbbb".to_vec()];
    let encoded = codec.encode(&original).unwrap();
    assert_eq!(encoded.len(), 3);
    let mut missing_data = encoded.iter().cloned().map(Some).collect::<Vec<_>>();
    missing_data[0] = None;
    assert_eq!(codec.reconstruct(missing_data).unwrap(), encoded);
    let mut missing_parity = encoded.iter().cloned().map(Some).collect::<Vec<_>>();
    missing_parity[2] = None;
    assert_eq!(codec.reconstruct(missing_parity).unwrap(), encoded);
}

#[test]
fn codec_validates_counts_and_shard_sizes() {
    let codec = RsCodec::new(2, 1).unwrap();
    assert!(matches!(
        codec.encode(&[vec![1]]),
        Err(ArgosError::Invalid(_))
    ));
    assert!(matches!(
        codec.encode(&[vec![1], vec![1, 2]]),
        Err(ArgosError::Invalid(_))
    ));
    assert!(matches!(
        codec.reconstruct(vec![Some(vec![1]), Some(vec![1])]),
        Err(ArgosError::Invalid(_))
    ));
    assert!(matches!(
        codec.reconstruct(vec![Some(vec![1]), None, None]),
        Err(ArgosError::Erasure(_))
    ));
}

#[test]
fn codec_rejects_an_overflowing_shard_count() {
    assert!(matches!(
        RsCodec::new(usize::MAX, 1),
        Err(ArgosError::Invalid(_))
    ));
}

fn fill_shard(shard: usize, size: usize) -> Vec<u8> {
    (0..size)
        .map(|i| {
            let v = (shard as u64 + 7)
                .wrapping_mul(0x9e37_79b9)
                .wrapping_add((i as u64).wrapping_mul(0xa41c_89d5))
                .rotate_left((i % 31) as u32);
            (v ^ (v >> 13) ^ (v >> 29)) as u8
        })
        .collect()
}

fn for_each_missing_set(total: usize, count: usize, mut f: impl FnMut(&[usize])) {
    fn visit(
        start: usize,
        total: usize,
        remaining: usize,
        missing: &mut Vec<usize>,
        f: &mut impl FnMut(&[usize]),
    ) {
        if remaining == 0 {
            f(missing);
            return;
        }
        for i in start..=total - remaining {
            missing.push(i);
            visit(i + 1, total, remaining - 1, missing, f);
            missing.pop();
        }
    }
    visit(0, total, count, &mut Vec::new(), &mut f);
}

#[test]
fn native_codec_matches_erase_for_every_layout_and_boundary_length() {
    use erase::galois_8::ReedSolomon as LegacyCodec;

    let layouts = [(1, 1), (2, 1), (3, 2), (4, 4), (5, 5), (6, 6)];
    let lengths = [
        1, 2, 7, 15, 16, 17, 31, 32, 33, 63, 64, 65, 127, 128, 129, 255, 256, 257, 1024,
    ];
    for (k, m) in layouts {
        let codec = RsCodec::new(k, m).unwrap();
        let legacy = LegacyCodec::new(k, m).unwrap();
        for len in lengths {
            let data = (0..k).map(|i| fill_shard(i, len)).collect::<Vec<_>>();
            let expected = {
                let mut shards = data.clone();
                shards.extend((0..m).map(|_| vec![0; len]));
                legacy.encode(&mut shards).unwrap();
                shards
            };
            assert_eq!(
                codec.encode(&data).unwrap(),
                expected,
                "parity mismatch for {k}+{m} at len={len}"
            );

            // Every missing data or parity shard, and every pair of erasures.
            // For the smallest vectors also test all patterns up to m missing.
            let upper = if len == 1 || len == 17 || len == 257 {
                m
            } else {
                m.min(2)
            };
            for count in 1..=upper {
                for_each_missing_set(k + m, count, |missing| {
                    let mut old = expected.iter().cloned().map(Some).collect::<Vec<_>>();
                    for &i in missing {
                        old[i] = None;
                    }
                    let mut new = old.clone();
                    legacy.reconstruct(&mut old).unwrap();
                    assert_eq!(
                        codec.reconstruct(std::mem::take(&mut new)).unwrap(),
                        expected,
                        "new decode mismatch for {k}+{m}, len={len}, missing={missing:?}"
                    );
                    assert_eq!(
                        old.into_iter().map(Option::unwrap).collect::<Vec<_>>(),
                        expected,
                        "legacy decode mismatch for {k}+{m}, len={len}, missing={missing:?}"
                    );
                });
            }
        }
    }
}

#[test]
fn every_supported_simd_kernel_matches_scalar_and_legacy() {
    use crate::data::ec::{Codec as EngineCodec, Kernel, ShardMask};
    use erase::galois_8::ReedSolomon as LegacyCodec;

    for (k, m) in [(1, 1), (2, 1), (4, 4), (6, 6)] {
        let legacy = LegacyCodec::new(k, m).unwrap();
        for len in [1, 15, 16, 17, 31, 32, 33, 63, 64, 65, 257, 4096] {
            let data = (0..k).map(|i| fill_shard(i + 13, len)).collect::<Vec<_>>();
            let mut expected = data.clone();
            expected.extend((0..m).map(|_| vec![0; len]));
            legacy.encode(&mut expected).unwrap();

            // Kernel::supported never includes an ISA this CPU cannot run.
            let supported = Kernel::supported().collect::<Vec<_>>();
            assert!(supported.contains(&Kernel::SCALAR));
            for kernel in supported {
                let engine = EngineCodec::with_kernel(kernel, k, m).unwrap();
                let mut encoded = data.clone();
                encoded.extend((0..m).map(|_| vec![0; len]));
                engine.encode_all(&mut encoded).unwrap();
                assert_eq!(
                    encoded,
                    expected,
                    "kernel {} broke parity for {k}+{m} len={len}",
                    kernel.name()
                );

                for lost in [0, k - 1, k, k + m - 1] {
                    let mut shards = expected.clone();
                    let mut present = ShardMask::all_present(k + m);
                    present.remove(lost);
                    shards[lost].fill(0xAA);
                    engine
                        .reconstruct(&mut shards, &present, &mut engine.workspace())
                        .unwrap();
                    assert_eq!(
                        shards,
                        expected,
                        "kernel {} failed erasure #{lost} for {k}+{m}",
                        kernel.name()
                    );
                }
            }
        }
    }
}

#[test]
fn parity_layout_retains_legacy_empty_and_invalid_input_contracts() {
    use erase::galois_8::ReedSolomon as LegacyCodec;

    let codec = RsCodec::new(2, 1).unwrap();
    let legacy = LegacyCodec::new(2, 1).unwrap();
    let mut empty = vec![Vec::new(), Vec::new(), Vec::new()];
    assert!(legacy.encode(&mut empty).is_err());
    assert!(codec.encode(&[Vec::new(), Vec::new()]).is_err());
    let mut all_present = empty.into_iter().map(Some).collect::<Vec<_>>();
    assert!(legacy.reconstruct(&mut all_present).is_err());
    assert!(codec.reconstruct(vec![Some(vec![]); 3]).is_err());

    assert!(codec
        .reconstruct(vec![Some(vec![1]), Some(vec![2, 3]), None])
        .is_err());
    assert!(codec.reconstruct(vec![None, None, None]).is_err());
    assert!(codec.reconstruct(vec![Some(vec![1]), None, None]).is_err());
    assert!(codec.encode(&[vec![0], vec![0, 1]]).is_err());
    assert!(RsCodec::new(256, 1).is_err());

    // No parity is a special layout; no RS library may reject it.
    // No-parity reconstruction historically accepted uneven shard lengths.
    assert_eq!(RsCodec::new(300, 0).unwrap().total(), 300);
    let no_parity_mixed = RsCodec::new(2, 0).unwrap();
    assert_eq!(
        no_parity_mixed
            .reconstruct(vec![Some(vec![1]), Some(vec![2, 3])])
            .unwrap(),
        vec![vec![1], vec![2, 3]]
    );
    let no_parity = RsCodec::new(1, 0).unwrap();
    assert_eq!(
        no_parity.encode(&[Vec::<u8>::new()]).unwrap(),
        vec![Vec::<u8>::new()]
    );
    assert_eq!(
        no_parity.reconstruct(vec![Some(Vec::<u8>::new())]).unwrap(),
        vec![Vec::<u8>::new()]
    );
}

#[test]
fn larger_redundancy_layouts_preserve_legacy_parity_and_repair() {
    use erase::galois_8::ReedSolomon as LegacyCodec;

    for (k, m) in [(8, 4), (12, 4), (16, 8), (32, 16)] {
        let codec = RsCodec::new(k, m).unwrap();
        let legacy = LegacyCodec::new(k, m).unwrap();
        for len in [1, 17, 257] {
            let data = (0..k).map(|i| fill_shard(i + 91, len)).collect::<Vec<_>>();
            let mut expected = data.clone();
            expected.extend((0..m).map(|_| vec![0; len]));
            legacy.encode(&mut expected).unwrap();
            assert_eq!(codec.encode(&data).unwrap(), expected);
            // Include missing data shards, missing parity, and a fully
            // degraded m-erasure pattern without relying on only one layout.
            for missing in [
                vec![0],
                vec![k - 1, k],
                vec![k + m - 1],
                (0..m).collect::<Vec<_>>(),
            ] {
                let mut old = expected.iter().cloned().map(Some).collect::<Vec<_>>();
                for i in missing {
                    old[i] = None;
                }
                let mut new = old.clone();
                legacy.reconstruct(&mut old).unwrap();
                assert_eq!(
                    codec.reconstruct(std::mem::take(&mut new)).unwrap(),
                    expected
                );
            }
        }
    }
}

#[test]
fn reused_data_decoder_matches_full_reconstruction_across_layouts_and_threads() {
    let work = || {
        for (k, m) in [(4, 4), (2, 1), (4, 4), (3, 2), (2, 1)] {
            let codec = RsCodec::new(k, m).unwrap();
            let data = (0..k).map(|i| fill_shard(i, 4097)).collect::<Vec<_>>();
            let encoded = codec.encode(&data).unwrap();
            for lost in 0..k + m {
                let mut shards = encoded.iter().cloned().map(Some).collect::<Vec<_>>();
                shards[lost] = None;
                assert_eq!(
                    RsCodec::reconstruct_data_cached(k, m, shards).unwrap(),
                    data,
                    "{k}+{m} missing slot {lost}"
                );
            }
        }
    };
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(work);
        }
    });
}

#[test]
fn cached_data_decoder_keeps_full_reconstruction_errors() {
    assert!(matches!(
        RsCodec::reconstruct_data_cached(2, 1, vec![Some(vec![1]), None]),
        Err(ArgosError::Invalid(_))
    ));
    assert!(matches!(
        RsCodec::reconstruct_data_cached(2, 1, vec![Some(vec![1]), None, None]),
        Err(ArgosError::Erasure(_))
    ));
}

// Exercise cache-tile boundaries and the final partial vector with a legacy
// oracle, not only the short vector lengths used by the kernel unit tests.
#[test]
fn large_tiled_shards_match_legacy_and_recover() {
    use crate::data::ec::{Codec, Kernel, ShardMask};
    use erase::galois_8::ReedSolomon as Legacy;
    for (k, m) in [(2, 1), (4, 2), (16, 8)] {
        let tile = (256 * 1024 / (k + m)).clamp(4096, 65536) & !63;
        let legacy = Legacy::new(k, m).unwrap();
        for len in [tile - 1, tile, tile + 1, 2 * tile + 17, 65537] {
            let mut expected = (0..k + m).map(|i| fill_shard(i, len)).collect::<Vec<_>>();
            legacy.encode(&mut expected).unwrap();
            for kernel in Kernel::supported() {
                let native = Codec::with_kernel(kernel, k, m).unwrap();
                let mut actual = expected.clone();
                for shard in &mut actual[k..] {
                    shard.fill(0xa5);
                }
                native.encode_all(&mut actual).unwrap();
                assert_eq!(actual, expected, "{} {k}+{m} {len}", kernel.name());
                let mut mask = ShardMask::all_present(k + m);
                mask.remove(0);
                actual[0].fill(0xa5);
                native
                    .reconstruct(&mut actual, &mask, &mut native.workspace())
                    .unwrap();
                assert_eq!(actual, expected, "{} {k}+{m} {len}", kernel.name());
            }
        }
    }
}
