//! Exhaustive and stratified erasure-pattern sweeps.
//!
//! Erasure coding fails silently: a decoder that is subtly wrong returns
//! plausible bytes and is only found out when someone actually needs their data
//! back. The round-trip property therefore gets a disproportionate share of the
//! test budget. Every case here encodes a shard set, destroys shards,
//! reconstructs, and demands the original bytes back exactly.
//!
//! # Why this is tiered rather than uniformly exhaustive
//!
//! The obvious reading of "all splits with total <= 32, every erasure pattern
//! up to the correction limit" is not a few million cases. The 16+16 split
//! alone has `sum(C(32, e) for e in 0..=16)` = 2,362,232,013 patterns, and the
//! whole `total <= 32` range runs to roughly 4 * 10^11 cases. That is not a
//! test, it is a supercomputer job.
//!
//! So the sweep is genuinely exhaustive where exhaustive is affordable and
//! explicitly, documentably sampled beyond that. The tiers:
//!
//! | tier | coverage | cases |
//! |---|---|---|
//! | 1 | every split with total 2..=18, and for each one **every** erasure pattern of weight <= parity_shards | 4,456,431 |
//! | 2 | every split with total 17..=32; every pattern of weight <= 2 exhaustively, plus structured and sampled patterns at each higher weight | 357,644 |
//! | 3 | twelve large splits, 255+1 and 1+255 among them; structured and sampled patterns at up to eight weights each | 1,180 |
//! | lengths | seven small splits, every pattern, at each of twelve shard lengths | 12,744 |
//! | parity | every subset of the parity shards erased, at eight splits | 2,366 |
//! | survivors | every data shard erased, recovery from parity alone, at fourteen splits | 122 |
//!
//! Total: 4,830,487 cases, about 3.5 seconds of `cargo test --release` on an
//! M-series laptop. Each tier asserts its own case count, so the numbers above
//! are checked by the suite rather than merely claimed by it.
//!
//! Tier 1 carried on to a total of 22 (a further 83,623,932 cases, about ninety
//! seconds) lives behind `#[ignore]`. Run it whenever the decoder changes:
//!
//! ```text
//! cargo test --release --test exhaustive -- --ignored
//! ```
//!
//! # What makes a case a real test
//!
//! Missing shards are never handed to the decoder zeroed. They are poisoned
//! first with bytes that are guaranteed non-zero *and* guaranteed to differ
//! from the truth in every position, so a decoder that silently no-ops, that
//! zero-fills, or that accumulates into stale contents cannot pass.

use reed_solomon_engine::{Codec, ShardMask};

// ---------------------------------------------------------------------------
// Deterministic randomness
// ---------------------------------------------------------------------------

/// Seeded xorshift64, so that any failure reproduces exactly from the
/// parameters printed in the assertion message.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        // Zero is a fixed point of xorshift, so it has to be excluded.
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// Returns a value in `0..n`, which must be non-empty.
    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
}

/// Deterministic shard contents. The high bits of the state are used because
/// the low bit of an xorshift stream is the weakest.
fn filled(seed: u64, len: usize) -> Vec<u8> {
    let mut rng = Rng::new(seed);
    (0..len).map(|_| (rng.next_u64() >> 33) as u8).collect()
}

// ---------------------------------------------------------------------------
// Erasure patterns
// ---------------------------------------------------------------------------

/// Builds a presence mask over `total` shards with `erased` marked missing.
fn erase(total: usize, erased: impl IntoIterator<Item = usize>) -> ShardMask {
    let mut mask = ShardMask::all_present(total);
    for index in erased {
        mask.remove(index);
    }
    mask
}

/// Builds a presence mask from a bitmap of erased shards, bit `i` for shard `i`.
fn erase_bits(total: usize, bits: u32) -> ShardMask {
    erase(total, (0..total).filter(|&i| bits >> i & 1 == 1))
}

/// The indices a mask reports missing. Used only in assertion messages.
fn missing(present: &ShardMask, total: usize) -> Vec<usize> {
    (0..total).filter(|&i| !present.is_present(i)).collect()
}

/// Erasure patterns of weight `e` chosen for their position rather than at
/// random: the placements a decoder's index arithmetic is most likely to get
/// wrong.
fn structured_patterns(total: usize, data_shards: usize, e: usize) -> [ShardMask; 4] {
    [
        // A run at the very front, which eats data before parity.
        erase(total, 0..e),
        // A run at the very back, which eats parity before data.
        erase(total, (total - e)..total),
        // Spread evenly, so present and missing shards interleave.
        erase(total, (0..e).map(|i| i * total / e)),
        // Straddling the data/parity boundary, where an off-by-one hides best.
        {
            let start = data_shards.saturating_sub(e / 2).min(total - e);
            erase(total, start..(start + e))
        },
    ]
}

/// Draws a uniformly random erasure pattern of weight `e` over `pool.len()`
/// shards.
///
/// `pool` must be a permutation of `0..total` on entry and is left a
/// permutation on exit, so successive draws stay allocation-free and
/// independent.
fn sample_pattern(rng: &mut Rng, pool: &mut [usize], e: usize) -> ShardMask {
    let total = pool.len();
    for i in 0..e {
        let j = i + rng.below(total - i);
        pool.swap(i, j);
    }
    erase(total, pool[..e].iter().copied())
}

// ---------------------------------------------------------------------------
// The round-trip harness
// ---------------------------------------------------------------------------

/// Overwrites `shard` with bytes that are non-zero everywhere and that differ
/// from `truth` in every position.
///
/// Both guarantees matter. Non-zero rules out a decoder that "passes" only
/// because the caller happened to hand it a zeroed buffer. Differing everywhere
/// rules out a decoder that leaves the buffer untouched or that accumulates
/// into stale contents.
fn poison(shard: &mut [u8], truth: &[u8], nonce: u8) {
    // Restricting the mask to 0x01..=0x7F makes both guarantees provable: it is
    // non-zero, so `t ^ mask != t`, and it is below 0x80, so the fallback below
    // is non-zero and still differs from `t`.
    let mask = (nonce | 0x01) & 0x7F;
    for (byte, &t) in shard.iter_mut().zip(truth) {
        let poisoned = t ^ mask;
        *byte = if poisoned == 0 { t ^ 0xFF } else { poisoned };
    }
}

/// Encodes one shard set and runs every erasure pattern `generate` yields
/// against it, asserting exact recovery of all `total` shards each time.
///
/// The buffers, the `&mut` view over them and the decoder are built once and
/// reused for every pattern, which is what keeps the million-case tier out of
/// the allocator.
///
/// Returns the number of cases checked.
fn sweep(
    data_shards: usize,
    parity_shards: usize,
    len: usize,
    seed: u64,
    generate: impl FnOnce(&mut dyn FnMut(&ShardMask)),
) -> usize {
    let total = data_shards + parity_shards;
    let codec = Codec::new(data_shards, parity_shards).unwrap();
    let mut workspace = codec.workspace();

    let mut original: Vec<Vec<u8>> = (0..total)
        .map(|i| {
            if i < data_shards {
                filled(seed ^ (i as u64).wrapping_mul(0x0100_0000_01B3), len)
            } else {
                vec![0; len]
            }
        })
        .collect();
    {
        let mut refs: Vec<&mut [u8]> = original.iter_mut().map(|s| &mut s[..]).collect();
        codec.encode_all(&mut refs).unwrap();
    }
    let original = original;

    let mut work = original.clone();
    let mut refs: Vec<&mut [u8]> = work.iter_mut().map(|s| &mut s[..]).collect();

    let mut cases = 0usize;
    let mut nonce = 0u8;
    {
        let mut check = |present: &ShardMask| {
            cases += 1;
            nonce = nonce.wrapping_add(0x25);

            for (i, shard) in refs.iter_mut().enumerate() {
                if present.is_present(i) {
                    shard.copy_from_slice(&original[i]);
                } else {
                    poison(shard, &original[i], nonce);
                }
            }

            if let Err(error) = codec.reconstruct(&mut refs, present, &mut workspace) {
                panic!(
                    "{data_shards}+{parity_shards} len {len}: reconstruct failed with \
                     \"{error}\" for erasures {:?}",
                    missing(present, total)
                );
            }

            for i in 0..total {
                assert_eq!(
                    &refs[i][..],
                    &original[i][..],
                    "{data_shards}+{parity_shards} len {len}: shard {i} is wrong after \
                     reconstructing erasures {:?}",
                    missing(present, total)
                );
            }
        };
        generate(&mut check);
    }
    cases
}

/// Shard lengths cycled through the exhaustive tiers, so alignment and tail
/// handling get covered without multiplying the case count by twelve.
const SMALL_LENGTHS: &[usize] = &[1, 2, 3, 4, 7, 8, 15, 16, 17, 31, 32, 33];

// ---------------------------------------------------------------------------
// Tier 1: fully exhaustive
// ---------------------------------------------------------------------------

/// Every split with `min_total <= data + parity <= max_total`, and for each one
/// every erasure pattern whose weight is within the correction limit.
///
/// Returns the number of cases checked.
fn exhaustive_sweep(min_total: usize, max_total: usize) -> usize {
    let mut cases = 0usize;
    for total in min_total..=max_total {
        for data_shards in 1..total {
            let parity_shards = total - data_shards;
            let len = SMALL_LENGTHS[(total * 5 + data_shards) % SMALL_LENGTHS.len()];
            let seed = 0x5EED_0000 ^ ((total as u64) << 8) ^ data_shards as u64;
            cases += sweep(data_shards, parity_shards, len, seed, |check| {
                for bits in 0u32..(1u32 << total) {
                    if bits.count_ones() as usize <= parity_shards {
                        check(&erase_bits(total, bits));
                    }
                }
            });
        }
    }
    cases
}

#[test]
fn every_erasure_pattern_recovers_for_totals_up_to_18() {
    assert_eq!(exhaustive_sweep(2, 18), 4_456_431);
}

/// The same proof carried out to a total of 22.
///
/// `cargo test --release --test exhaustive -- --ignored`
#[test]
#[ignore = "84M cases, about 90 seconds; run with --ignored when the decoder changes"]
fn every_erasure_pattern_recovers_for_totals_up_to_22() {
    assert_eq!(exhaustive_sweep(19, 22), 83_623_932);
}

// ---------------------------------------------------------------------------
// Tier 2: exhaustive at low weight, stratified above it
// ---------------------------------------------------------------------------

/// Random patterns drawn per erasure weight in tier 2.
const SAMPLES_PER_WEIGHT: usize = 48;

/// Extra random patterns drawn at exactly the correction limit, where the
/// decoder solves its largest system and has the least slack.
const SAMPLES_AT_LIMIT: usize = 64;

#[test]
fn sampled_erasure_patterns_recover_for_totals_17_to_32() {
    let mut cases = 0usize;
    for total in 17..=32 {
        for data_shards in 1..total {
            let parity_shards = total - data_shards;
            let len = SMALL_LENGTHS[(total * 7 + data_shards) % SMALL_LENGTHS.len()];
            let seed = 0xBEEF_0000 ^ ((total as u64) << 8) ^ data_shards as u64;
            let mut rng = Rng::new(seed);
            let mut pool: Vec<usize> = (0..total).collect();

            cases += sweep(data_shards, parity_shards, len, seed, |check| {
                // Weights 0, 1 and 2 exhaustively. Single and double erasures
                // dominate real workloads and are cheap enough to prove
                // outright at these sizes.
                check(&ShardMask::all_present(total));
                for i in 0..total {
                    check(&erase(total, [i]));
                }
                if parity_shards >= 2 {
                    for i in 0..total {
                        for j in (i + 1)..total {
                            check(&erase(total, [i, j]));
                        }
                    }
                }

                for e in 3..=parity_shards {
                    for pattern in structured_patterns(total, data_shards, e) {
                        check(&pattern);
                    }
                    let draws = if e == parity_shards {
                        SAMPLES_PER_WEIGHT + SAMPLES_AT_LIMIT
                    } else {
                        SAMPLES_PER_WEIGHT
                    };
                    for _ in 0..draws {
                        check(&sample_pattern(&mut rng, &mut pool, e));
                    }
                }
            });
        }
    }
    assert_eq!(cases, 357_644);
}

// ---------------------------------------------------------------------------
// Tier 3: large parameters
// ---------------------------------------------------------------------------

/// `(data_shards, parity_shards, shard_len)` beyond the reach of tier 2,
/// including both ends of the shard-count range and the 1200-byte UDP payload
/// the driving consumer actually sends.
const LARGE_PARAMS: &[(usize, usize, usize)] = &[
    (32, 16, 1200),
    (40, 24, 64),
    (64, 16, 1200),
    (64, 64, 64),
    (100, 28, 32),
    (128, 128, 32),
    (200, 56, 16),
    (254, 2, 64),
    (255, 1, 1200),
    (2, 254, 16),
    (1, 255, 8),
    (17, 239, 8),
];

/// Random patterns drawn per erasure weight in tier 3.
const LARGE_SAMPLES_PER_WEIGHT: usize = 12;

#[test]
fn sampled_erasure_patterns_recover_for_large_parameters() {
    let mut cases = 0usize;
    for &(data_shards, parity_shards, len) in LARGE_PARAMS {
        let total = data_shards + parity_shards;
        let seed = 0xF00D_0000 ^ ((data_shards as u64) << 16) ^ parity_shards as u64;
        let mut rng = Rng::new(seed);
        let mut pool: Vec<usize> = (0..total).collect();

        // A spread of erasure weights from none to the correction limit. Low
        // weights are the common case; the limit is the hard one. Every entry
        // is clamped to the limit, so splits with very little parity simply
        // yield fewer distinct weights.
        let mut weights = [
            0,
            1,
            2.min(parity_shards),
            parity_shards / 4,
            parity_shards / 2,
            parity_shards * 3 / 4,
            parity_shards.saturating_sub(1),
            parity_shards,
        ];
        weights.sort_unstable();

        cases += sweep(data_shards, parity_shards, len, seed, |check| {
            let mut previous = usize::MAX;
            for e in weights {
                if e == previous {
                    continue;
                }
                previous = e;

                if e == 0 {
                    check(&ShardMask::all_present(total));
                    continue;
                }
                for pattern in structured_patterns(total, data_shards, e) {
                    check(&pattern);
                }
                for _ in 0..LARGE_SAMPLES_PER_WEIGHT {
                    check(&sample_pattern(&mut rng, &mut pool, e));
                }
            }
        });
    }
    assert_eq!(cases, 1_180);
}

// ---------------------------------------------------------------------------
// Shard lengths
// ---------------------------------------------------------------------------

/// Every length that exercises a distinct alignment or tail case for the SIMD
/// kernels to come, plus 1200: a UDP-MTU payload, the driving consumer's normal
/// shard size.
const LENGTHS: &[usize] = &[1, 2, 15, 16, 17, 31, 32, 33, 63, 64, 65, 1200];

/// Splits small enough to sweep exhaustively once per shard length.
const LENGTH_PARAMS: &[(usize, usize)] = &[(1, 1), (2, 1), (3, 2), (4, 2), (5, 3), (6, 3), (8, 4)];

#[test]
fn every_shard_length_round_trips_for_every_erasure_pattern() {
    let mut cases = 0usize;
    for &len in LENGTHS {
        for &(data_shards, parity_shards) in LENGTH_PARAMS {
            let total = data_shards + parity_shards;
            let seed = 0x1E46_0000 ^ ((len as u64) << 8) ^ data_shards as u64;
            cases += sweep(data_shards, parity_shards, len, seed, |check| {
                for bits in 0u32..(1u32 << total) {
                    if bits.count_ones() as usize <= parity_shards {
                        check(&erase_bits(total, bits));
                    }
                }
            });
        }
    }
    assert_eq!(cases, 12_744);
}

// ---------------------------------------------------------------------------
// Parity shards are reconstructed too
// ---------------------------------------------------------------------------

/// Splits whose parity subsets are few enough to enumerate exhaustively.
const PARITY_ONLY_PARAMS: &[(usize, usize)] = &[
    (1, 1),
    (2, 3),
    (4, 2),
    (5, 5),
    (8, 4),
    (16, 8),
    (3, 10),
    (10, 10),
];

#[test]
fn reconstruct_regenerates_every_subset_of_missing_parity_shards() {
    // With no data shard missing, the decoder's recovery path is a no-op and
    // parity regeneration is the entire test.
    let mut cases = 0usize;
    for &(data_shards, parity_shards) in PARITY_ONLY_PARAMS {
        let total = data_shards + parity_shards;
        let seed = 0x9A17_0000 ^ ((data_shards as u64) << 8) ^ parity_shards as u64;
        let checked = sweep(data_shards, parity_shards, 97, seed, |check| {
            for bits in 0u32..(1u32 << parity_shards) {
                check(&erase(
                    total,
                    (0..parity_shards)
                        .filter(|&i| bits >> i & 1 == 1)
                        .map(|i| data_shards + i),
                ));
            }
        });
        assert_eq!(checked, 1 << parity_shards);
        cases += checked;
    }
    assert_eq!(cases, 2_366);
}

// ---------------------------------------------------------------------------
// Recovery from parity alone
// ---------------------------------------------------------------------------

/// Splits with at least as much parity as data, where losing every data shard
/// is still recoverable.
const PARITY_SURVIVOR_PARAMS: &[(usize, usize, usize)] = &[
    (1, 1, 1200),
    (1, 3, 65),
    (2, 2, 33),
    (3, 5, 17),
    (4, 4, 1200),
    (4, 6, 16),
    (8, 8, 64),
    (16, 16, 63),
    (32, 32, 32),
    (64, 64, 16),
    (128, 128, 8),
    (17, 239, 8),
    (2, 254, 16),
    (1, 255, 8),
];

/// Random parity subsets dropped when the survivor set can be chosen in more
/// than `parity_shards` ways.
const SURVIVOR_SAMPLES: usize = 16;

#[test]
fn recovery_from_parity_only_survivors() {
    // Every data shard is gone. The survivors are exactly `data_shards` parity
    // shards, which is the information-theoretic minimum, so this is the
    // decode path with no slack at all.
    let mut cases = 0usize;
    for &(data_shards, parity_shards, len) in PARITY_SURVIVOR_PARAMS {
        let total = data_shards + parity_shards;
        let dropped = parity_shards - data_shards;
        let seed = 0x0DD_0000 ^ ((data_shards as u64) << 16) ^ parity_shards as u64;
        let mut rng = Rng::new(seed);
        let mut pool: Vec<usize> = (data_shards..total).collect();

        cases += sweep(data_shards, parity_shards, len, seed, |check| {
            let mut erase_data_and = |extra: &[usize]| {
                check(&erase(total, (0..data_shards).chain(extra.iter().copied())));
            };
            match dropped {
                // Only one survivor set exists: all of the parity.
                0 => erase_data_and(&[]),
                // Enumerate which single parity shard is also lost.
                1 => {
                    for i in data_shards..total {
                        erase_data_and(&[i]);
                    }
                }
                _ => {
                    erase_data_and(&(data_shards..data_shards + dropped).collect::<Vec<_>>());
                    erase_data_and(&(total - dropped..total).collect::<Vec<_>>());
                    let stride: Vec<usize> = (0..dropped)
                        .map(|i| data_shards + i * parity_shards / dropped)
                        .collect();
                    erase_data_and(&stride);
                    for _ in 0..SURVIVOR_SAMPLES {
                        // Partial Fisher-Yates over the parity indices, which
                        // leaves `pool` a permutation for the next draw.
                        for i in 0..dropped {
                            let j = i + rng.below(parity_shards - i);
                            pool.swap(i, j);
                        }
                        erase_data_and(&pool[..dropped]);
                    }
                }
            }
        });
    }
    assert_eq!(cases, 122);
}

// ---------------------------------------------------------------------------
// reconstruct_data leaves missing parity alone
// ---------------------------------------------------------------------------

#[test]
fn reconstruct_data_restores_data_and_leaves_missing_parity_untouched() {
    for &(data_shards, parity_shards) in &[(1usize, 1usize), (4, 2), (5, 3), (8, 4), (10, 6)] {
        let total = data_shards + parity_shards;
        let len = 71;
        let codec = Codec::new(data_shards, parity_shards).unwrap();
        let mut workspace = codec.workspace();

        let mut original: Vec<Vec<u8>> = (0..total)
            .map(|i| {
                if i < data_shards {
                    filled(0xD47A ^ i as u64, len)
                } else {
                    vec![0; len]
                }
            })
            .collect();
        {
            let mut refs: Vec<&mut [u8]> = original.iter_mut().map(|s| &mut s[..]).collect();
            codec.encode_all(&mut refs).unwrap();
        }
        let original = original;

        let mut work = original.clone();
        let mut refs: Vec<&mut [u8]> = work.iter_mut().map(|s| &mut s[..]).collect();
        let mut poisoned = vec![vec![0u8; len]; total];

        for bits in 0u32..(1u32 << total) {
            if bits.count_ones() as usize > parity_shards {
                continue;
            }
            let present = erase_bits(total, bits);
            let nonce = (bits as u8).wrapping_mul(11);

            for (i, shard) in refs.iter_mut().enumerate() {
                if present.is_present(i) {
                    shard.copy_from_slice(&original[i]);
                } else {
                    poison(shard, &original[i], nonce);
                }
                poisoned[i].copy_from_slice(&shard[..]);
            }

            codec
                .reconstruct_data(&mut refs, &present, &mut workspace)
                .unwrap();

            for i in 0..data_shards {
                assert_eq!(
                    &refs[i][..],
                    &original[i][..],
                    "{data_shards}+{parity_shards}: reconstruct_data lost data shard {i} \
                     for erasures {:?}",
                    missing(&present, total)
                );
            }
            for i in data_shards..total {
                let expected: &[u8] = if present.is_present(i) {
                    &original[i]
                } else {
                    &poisoned[i]
                };
                assert_eq!(
                    &refs[i][..],
                    expected,
                    "{data_shards}+{parity_shards}: reconstruct_data touched parity shard \
                     {i} for erasures {:?}",
                    missing(&present, total)
                );
            }

            // The full reconstruct must agree on the data and additionally
            // restore the parity, from the state reconstruct_data left behind.
            codec
                .reconstruct(&mut refs, &present, &mut workspace)
                .unwrap();
            for i in 0..total {
                assert_eq!(
                    &refs[i][..],
                    &original[i][..],
                    "{data_shards}+{parity_shards}: reconstruct after reconstruct_data \
                     left shard {i} wrong for erasures {:?}",
                    missing(&present, total)
                );
            }
        }
    }
}
