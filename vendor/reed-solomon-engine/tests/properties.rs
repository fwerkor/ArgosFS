//! Deterministic property tests.
//!
//! No `proptest`, no `quickcheck`. A twenty-line seeded xorshift generator gives
//! the same randomized coverage with none of the dependency and none of the
//! run-to-run flakiness: every case here is the same case on every machine, on
//! every run, forever. When one fails, the iteration number in the message
//! names it exactly.
//!
//! The properties checked:
//!
//! - **Round trip.** Random splits, random shard lengths, random loss patterns;
//!   the original bytes come back exactly.
//! - **Systematic.** Encoding never touches the data shards.
//! - **Linear.** `encode(a) ^ encode(b) == encode(a ^ b)`, the defining property
//!   of the matrix product the whole crate implements.
//! - **Deterministic.** The same input produces the same bytes, from the same
//!   codec, from a fresh codec, and through every entry point.
//! - **Idempotent.** Reconstructing a complete set changes nothing;
//!   reconstructing twice gives the same answer as reconstructing once.
//! - **Equivalent paths.** The allocation-free [`ParityMatrixRef`] and the
//!   `const` [`ParityMatrix`] agree byte for byte with [`Codec`].
//! - **Total.** Every error variant a caller can reach is reachable, and no
//!   malformed input makes the codec panic.

use reed_solomon_engine::{
    Codec, Error, MAX_SHARDS, ParityMatrix, ParityMatrixRef, ShardMask, gf, workspace_len,
};

// ---------------------------------------------------------------------------
// The generator
// ---------------------------------------------------------------------------

/// Seeded xorshift64. Small enough to read in one sitting, which is the point:
/// a test generator nobody understands is a test nobody trusts.
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

    /// The high bits are used because the low bit of an xorshift stream is its
    /// weakest.
    fn byte(&mut self) -> u8 {
        (self.next_u64() >> 33) as u8
    }

    fn bytes(&mut self, len: usize) -> Vec<u8> {
        (0..len).map(|_| self.byte()).collect()
    }
}

/// Draws a `(data_shards, parity_shards)` split for the high-iteration
/// properties, biased toward the small configurations that dominate real use.
///
/// The cap at 64 total shards is deliberate: building a generator matrix costs
/// `O(data_shards^3)` and would otherwise dominate the runtime of every test in
/// this file. The large end of the parameter range is covered exhaustively by
/// `tests/exhaustive.rs` and, here, by [`large_splits_round_trip`].
fn random_split(rng: &mut Rng) -> (usize, usize) {
    let total = if rng.below(4) == 0 {
        2 + rng.below(63)
    } else {
        2 + rng.below(15)
    };
    let data_shards = 1 + rng.below(total - 1);
    (data_shards, total - data_shards)
}

/// Draws a split from the upper half of the shard-count range, up to the
/// 256-shard limit.
fn random_large_split(rng: &mut Rng) -> (usize, usize) {
    let total = 65 + rng.below(192);
    let data_shards = 1 + rng.below(total - 1);
    (data_shards, total - data_shards)
}

/// Lengths that exercise every alignment and tail case a future SIMD kernel
/// will have, plus 1200: a UDP-MTU payload, the driving consumer's normal shard
/// size.
const LENGTHS: &[usize] = &[
    1, 2, 3, 5, 7, 8, 9, 15, 16, 17, 31, 32, 33, 63, 64, 65, 127, 128, 129, 255, 256, 1200,
];

/// A round trip costs `O(total^2 * len)` byte operations. Capping that product
/// keeps one iteration cheap without capping either factor on its own, so small
/// splits still see 1200-byte shards and large splits still get tested.
const WORK_BUDGET: usize = 400_000;

fn random_len(rng: &mut Rng, total: usize) -> usize {
    let ceiling = (WORK_BUDGET / (total * total)).max(1);
    LENGTHS[rng.below(LENGTHS.len())].min(ceiling)
}

/// Draws a presence mask over `total` shards with exactly `erasures` missing.
fn random_erasures(rng: &mut Rng, total: usize, erasures: usize) -> ShardMask {
    let mut pool: Vec<usize> = (0..total).collect();
    let mut mask = ShardMask::all_present(total);
    for i in 0..erasures {
        let j = i + rng.below(total - i);
        pool.swap(i, j);
        mask.remove(pool[i]);
    }
    mask
}

/// Overwrites `shard` with bytes that are non-zero everywhere and that differ
/// from `truth` in every position, so a codec that no-ops, that zero-fills or
/// that accumulates into stale contents cannot pass.
fn poison(shard: &mut [u8], truth: &[u8], nonce: u8) {
    // Keeping the mask in 0x01..=0x7F makes both guarantees provable: it is
    // non-zero, so `t ^ mask != t`, and it is below 0x80, so the fallback is
    // itself non-zero and still differs from `t`.
    let mask = (nonce | 0x01) & 0x7F;
    for (byte, &t) in shard.iter_mut().zip(truth) {
        let poisoned = t ^ mask;
        *byte = if poisoned == 0 { t ^ 0xFF } else { poisoned };
    }
}

/// Poisons every shard the mask reports missing.
fn poison_missing(buffers: &mut [Vec<u8>], truth: &[Vec<u8>], present: &ShardMask, nonce: u8) {
    for (i, shard) in buffers.iter_mut().enumerate() {
        if !present.is_present(i) {
            poison(shard, &truth[i], nonce);
        }
    }
}

/// A full shard set: random data shards followed by their parity.
fn encoded(rng: &mut Rng, codec: &Codec, len: usize) -> Vec<Vec<u8>> {
    let mut buffers: Vec<Vec<u8>> = (0..codec.total_shards())
        .map(|i| {
            if i < codec.data_shards() {
                rng.bytes(len)
            } else {
                vec![0; len]
            }
        })
        .collect();
    codec.encode_all(&mut buffers).unwrap();
    buffers
}

// ---------------------------------------------------------------------------
// Round trip
// ---------------------------------------------------------------------------

const ITERATIONS: usize = 20_000;

#[test]
fn random_loss_patterns_round_trip_exactly() {
    let mut rng = Rng::new(0x0011_2233_4455_6677);
    for iteration in 0..ITERATIONS {
        let (data_shards, parity_shards) = random_split(&mut rng);
        let total = data_shards + parity_shards;
        let len = random_len(&mut rng, total);

        let codec = Codec::new(data_shards, parity_shards).unwrap();
        let mut workspace = codec.workspace();

        let original = encoded(&mut rng, &codec, len);
        let mut buffers = original.clone();

        let erasures = rng.below(parity_shards + 1);
        let present = random_erasures(&mut rng, total, erasures);
        poison_missing(&mut buffers, &original, &present, rng.byte());

        let context = format!(
            "iteration {iteration}: {data_shards}+{parity_shards}, len {len}, \
             {erasures} erasures"
        );
        codec
            .reconstruct(&mut buffers, &present, &mut workspace)
            .unwrap_or_else(|error| panic!("{context}: reconstruct failed with \"{error}\""));
        assert_eq!(buffers, original, "{context}");
    }
}

#[test]
fn large_splits_round_trip() {
    // 65 to 256 total shards. Fewer iterations because building the generator
    // matrix is cubic in the data shard count and dominates here.
    let mut rng = Rng::new(0x1A46_E000);
    for iteration in 0..600 {
        let (data_shards, parity_shards) = random_large_split(&mut rng);
        let total = data_shards + parity_shards;
        let len = random_len(&mut rng, total);

        let codec = Codec::new(data_shards, parity_shards).unwrap();
        let mut workspace = codec.workspace();

        let original = encoded(&mut rng, &codec, len);
        let mut buffers = original.clone();

        let erasures = rng.below(parity_shards + 1);
        let present = random_erasures(&mut rng, total, erasures);
        poison_missing(&mut buffers, &original, &present, rng.byte());

        let context = format!(
            "iteration {iteration}: {data_shards}+{parity_shards}, len {len}, \
             {erasures} erasures"
        );
        codec
            .reconstruct(&mut buffers, &present, &mut workspace)
            .unwrap_or_else(|error| panic!("{context}: reconstruct failed with \"{error}\""));
        assert_eq!(buffers, original, "{context}");
    }
}

#[test]
fn reconstruct_data_agrees_with_reconstruct_on_the_data_shards() {
    let mut rng = Rng::new(0x0DA7_A001_0000);
    for iteration in 0..ITERATIONS {
        let (data_shards, parity_shards) = random_split(&mut rng);
        let total = data_shards + parity_shards;
        let len = random_len(&mut rng, total);

        let codec = Codec::new(data_shards, parity_shards).unwrap();
        let mut workspace = codec.workspace();

        let original = encoded(&mut rng, &codec, len);
        let erasures = rng.below(parity_shards + 1);
        let present = random_erasures(&mut rng, total, erasures);

        let mut buffers = original.clone();
        poison_missing(&mut buffers, &original, &present, rng.byte());
        let poisoned = buffers.clone();

        codec
            .reconstruct_data(&mut buffers, &present, &mut workspace)
            .unwrap();

        for i in 0..data_shards {
            assert_eq!(
                buffers[i], original[i],
                "iteration {iteration}: reconstruct_data lost data shard {i}"
            );
        }
        for i in data_shards..total {
            let expected = if present.is_present(i) {
                &original[i]
            } else {
                &poisoned[i]
            };
            assert_eq!(
                &buffers[i], expected,
                "iteration {iteration}: reconstruct_data touched parity shard {i}"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Structural properties of the code
// ---------------------------------------------------------------------------

#[test]
fn encoding_leaves_the_data_shards_untouched() {
    // The systematic property. If it ever breaks, every consumer that reads its
    // payload straight out of the first `data_shards` shards breaks with it.
    let mut rng = Rng::new(0x5751_E4A7_1C00);
    for _ in 0..2_500 {
        let (data_shards, parity_shards) = random_split(&mut rng);
        let len = random_len(&mut rng, data_shards + parity_shards);
        let codec = Codec::new(data_shards, parity_shards).unwrap();

        let data: Vec<Vec<u8>> = (0..data_shards).map(|_| rng.bytes(len)).collect();
        let mut buffers = data.clone();
        buffers.extend((0..parity_shards).map(|_| vec![0xFFu8; len]));

        codec.encode_all(&mut buffers).unwrap();
        assert_eq!(
            &buffers[..data_shards],
            &data[..],
            "{data_shards}+{parity_shards}"
        );
    }
}

#[test]
fn encoding_is_linear_over_the_field() {
    // Parity is a matrix product, so it distributes over XOR. That is a far
    // stronger statement about the kernel than any single encode: it says the
    // per-byte operation really is field multiplication, and not something that
    // merely agrees with it on the samples the other tests happen to use.
    let mut rng = Rng::new(0x11BE_A200);
    for _ in 0..2_000 {
        let (data_shards, parity_shards) = random_split(&mut rng);
        let len = random_len(&mut rng, data_shards + parity_shards);
        let codec = Codec::new(data_shards, parity_shards).unwrap();

        let a: Vec<Vec<u8>> = (0..data_shards).map(|_| rng.bytes(len)).collect();
        let b: Vec<Vec<u8>> = (0..data_shards).map(|_| rng.bytes(len)).collect();
        let sum: Vec<Vec<u8>> = a
            .iter()
            .zip(&b)
            .map(|(x, y)| x.iter().zip(y).map(|(p, q)| p ^ q).collect())
            .collect();

        let parity_of = |shards: &[Vec<u8>]| -> Vec<Vec<u8>> {
            let mut out = vec![vec![0u8; len]; parity_shards];
            codec.encode(shards, &mut out).unwrap();
            out
        };

        let (pa, pb, psum) = (parity_of(&a), parity_of(&b), parity_of(&sum));
        for row in 0..parity_shards {
            let expected: Vec<u8> = pa[row].iter().zip(&pb[row]).map(|(p, q)| p ^ q).collect();
            assert_eq!(
                psum[row], expected,
                "{data_shards}+{parity_shards}: parity row {row} is not linear"
            );
        }
    }
}

#[test]
fn encoding_is_deterministic() {
    // Sender and receiver frequently run different builds of this library. If
    // the same input ever produced different parity, the wire format would be a
    // lie.
    let mut rng = Rng::new(0xDE7E_5A11_0000);
    for _ in 0..1_500 {
        let (data_shards, parity_shards) = random_split(&mut rng);
        let len = random_len(&mut rng, data_shards + parity_shards);

        let codec = Codec::new(data_shards, parity_shards).unwrap();
        let data: Vec<Vec<u8>> = (0..data_shards).map(|_| rng.bytes(len)).collect();

        let mut expected = vec![vec![0u8; len]; parity_shards];
        codec.encode(&data, &mut expected).unwrap();

        // The same codec, repeatedly, into buffers that start out dirty.
        for _ in 0..4 {
            let mut again = vec![vec![0xA5u8; len]; parity_shards];
            codec.encode(&data, &mut again).unwrap();
            assert_eq!(
                again, expected,
                "{data_shards}+{parity_shards}: codec is not stable"
            );
        }

        // A freshly built codec, which rebuilds the generator matrix from
        // scratch.
        let fresh = Codec::new(data_shards, parity_shards).unwrap();
        assert_eq!(
            fresh.parity_matrix(),
            codec.parity_matrix(),
            "{data_shards}+{parity_shards}: generator matrix is not reproducible"
        );
        let mut again = vec![vec![0xA5u8; len]; parity_shards];
        fresh.encode(&data, &mut again).unwrap();
        assert_eq!(
            again, expected,
            "{data_shards}+{parity_shards}: fresh codec differs"
        );
    }
}

#[test]
fn reconstructing_a_complete_shard_set_changes_nothing() {
    let mut rng = Rng::new(0x1DE_1D0E_0000);
    for _ in 0..2_500 {
        let (data_shards, parity_shards) = random_split(&mut rng);
        let total = data_shards + parity_shards;
        let len = random_len(&mut rng, total);

        let codec = Codec::new(data_shards, parity_shards).unwrap();
        let mut workspace = codec.workspace();

        let original = encoded(&mut rng, &codec, len);
        let mut buffers = original.clone();
        let all = ShardMask::all_present(total);

        codec
            .reconstruct(&mut buffers, &all, &mut workspace)
            .unwrap();
        assert_eq!(
            buffers, original,
            "{data_shards}+{parity_shards}: reconstruct was not inert"
        );

        codec
            .reconstruct_data(&mut buffers, &all, &mut workspace)
            .unwrap();
        assert_eq!(
            buffers, original,
            "{data_shards}+{parity_shards}: reconstruct_data was not inert"
        );
    }
}

#[test]
fn reconstructing_twice_gives_the_same_answer_as_once() {
    let mut rng = Rng::new(0x71CE_0000);
    for _ in 0..2_500 {
        let (data_shards, parity_shards) = random_split(&mut rng);
        let total = data_shards + parity_shards;
        let len = random_len(&mut rng, total);

        let codec = Codec::new(data_shards, parity_shards).unwrap();
        let mut workspace = codec.workspace();

        let original = encoded(&mut rng, &codec, len);
        let erasures = rng.below(parity_shards + 1);
        let present = random_erasures(&mut rng, total, erasures);

        let mut buffers = original.clone();
        poison_missing(&mut buffers, &original, &present, 0x3C);

        codec
            .reconstruct(&mut buffers, &present, &mut workspace)
            .unwrap();
        let once = buffers.clone();
        codec
            .reconstruct(&mut buffers, &present, &mut workspace)
            .unwrap();
        assert_eq!(
            buffers, once,
            "{data_shards}+{parity_shards}: reconstruct is not idempotent"
        );
    }
}

// ---------------------------------------------------------------------------
// The allocation-free path agrees with the owned-buffer path
// ---------------------------------------------------------------------------

#[test]
fn a_borrowed_matrix_agrees_with_the_owned_codec() {
    let mut rng = Rng::new(0xF9EE_F000);
    for _ in 0..3_000 {
        let (data_shards, parity_shards) = random_split(&mut rng);
        let total = data_shards + parity_shards;
        let len = random_len(&mut rng, total);

        let codec = Codec::new(data_shards, parity_shards).unwrap();
        let matrix = codec.parity_matrix().to_vec();

        let data: Vec<Vec<u8>> = (0..data_shards).map(|_| rng.bytes(len)).collect();

        // The matrix is handed over as bytes, exactly as a caller reloading it
        // from storage would, and the split is declared alongside it.
        let borrowed = ParityMatrixRef::new(data_shards, parity_shards, &matrix).unwrap();

        // Encoder::encode vs ParityMatrixRef::encode
        let mut via_encoder = vec![vec![0u8; len]; parity_shards];
        codec.encode(&data, &mut via_encoder).unwrap();
        let mut via_free = vec![vec![0xA5u8; len]; parity_shards];
        borrowed.encode(&data, &mut via_free).unwrap();
        assert_eq!(
            via_free, via_encoder,
            "{data_shards}+{parity_shards}: the borrowed encode differs"
        );

        // Encoder::encode_all vs ParityMatrixRef::encode_all
        let mut all_encoder = data.clone();
        all_encoder.extend((0..parity_shards).map(|_| vec![0u8; len]));
        codec.encode_all(&mut all_encoder).unwrap();
        let mut all_free = data.clone();
        all_free.extend((0..parity_shards).map(|_| vec![0xA5u8; len]));
        borrowed.encode_all(&mut all_free).unwrap();
        assert_eq!(
            all_free, all_encoder,
            "{data_shards}+{parity_shards}: the borrowed encode_all differs"
        );
        assert_eq!(
            all_encoder[data_shards..],
            via_encoder[..],
            "{data_shards}+{parity_shards}: encode and encode_all disagree"
        );

        // Decoder::reconstruct vs ParityMatrixRef::reconstruct, and the
        // data-only pair.
        let original = all_encoder;
        let erasures = rng.below(parity_shards + 1);
        let present = random_erasures(&mut rng, total, erasures);
        let mut workspace = vec![0u8; workspace_len(data_shards, parity_shards)];

        for data_only in [false, true] {
            let mut lhs = original.clone();
            let mut rhs = original.clone();
            poison_missing(&mut lhs, &original, &present, 0x66);
            poison_missing(&mut rhs, &original, &present, 0x66);

            if data_only {
                codec
                    .reconstruct_data(&mut lhs, &present, &mut workspace)
                    .unwrap();
                borrowed
                    .reconstruct_data(&mut rhs, &present, &mut workspace)
                    .unwrap();
            } else {
                codec
                    .reconstruct(&mut lhs, &present, &mut workspace)
                    .unwrap();
                borrowed
                    .reconstruct(&mut rhs, &present, &mut workspace)
                    .unwrap();
            }
            assert_eq!(
                rhs, lhs,
                "{data_shards}+{parity_shards}: the borrowed reconstruct differs \
                 (data_only = {data_only})"
            );
        }
    }
}

/// Asserts that the `const` matrix for `$data + $parity` is byte-identical to
/// the one [`Encoder`] builds at runtime, and that both produce the same parity.
macro_rules! assert_const_matrix_matches_runtime {
    ($cap:expr, $data:expr, $parity:expr) => {{
        const MATRIX: ParityMatrix<$cap> = ParityMatrix::new($data, $parity);
        let codec = Codec::new($data, $parity).unwrap();

        assert_eq!(
            MATRIX.coefficients(),
            codec.parity_matrix(),
            "const and runtime coefficients differ for {}+{}",
            $data,
            $parity
        );
        assert_eq!(MATRIX.data_shards(), codec.data_shards());
        assert_eq!(MATRIX.parity_shards(), codec.parity_shards());
        assert_eq!(MATRIX.total_shards(), codec.total_shards());

        let mut rng = Rng::new((($data as u64) << 32) | ($parity as u64));
        let data: Vec<Vec<u8>> = (0..$data).map(|_| rng.bytes(37)).collect();
        let mut from_const = vec![vec![0u8; 37]; $parity];
        let mut from_encoder = vec![vec![0u8; 37]; $parity];
        MATRIX.encode(&data, &mut from_const).unwrap();
        codec.encode(&data, &mut from_encoder).unwrap();
        assert_eq!(
            from_const, from_encoder,
            "const and runtime parity differ for {}+{}",
            $data, $parity
        );
    }};
}

#[test]
fn const_parity_matrices_match_the_runtime_encoder() {
    // The no-alloc path and the alloc path have to be the same wire format.
    // They share one `build_parity_matrix`, but nothing except this test proves
    // that const evaluation and runtime evaluation of it agree.
    assert_const_matrix_matches_runtime!(1, 1, 1);
    assert_const_matrix_matches_runtime!(2, 2, 1);
    assert_const_matrix_matches_runtime!(8, 4, 2);
    assert_const_matrix_matches_runtime!(15, 5, 3);
    assert_const_matrix_matches_runtime!(32, 8, 4);
    assert_const_matrix_matches_runtime!(60, 10, 6);
    assert_const_matrix_matches_runtime!(85, 17, 5);
    assert_const_matrix_matches_runtime!(128, 16, 8);
    assert_const_matrix_matches_runtime!(512, 32, 16);
    assert_const_matrix_matches_runtime!(255, 1, 255);
    assert_const_matrix_matches_runtime!(256, 64, 4);
}

// ---------------------------------------------------------------------------
// Error paths
// ---------------------------------------------------------------------------

/// Requires `result` to be `Err(expected)` and returns the error, so callers can
/// tally which variants a real call actually produced.
#[track_caller]
fn expect_err<T: core::fmt::Debug>(what: &str, expected: Error, result: Result<T, Error>) -> Error {
    match result {
        Ok(value) => panic!("{what}: expected Err({expected:?}), got Ok({value:?})"),
        Err(actual) => {
            assert_eq!(actual, expected, "{what}");
            actual
        }
    }
}

#[test]
fn every_caller_reachable_error_variant_is_produced_by_a_real_call() {
    let mut seen: Vec<Error> = Vec::new();
    let codec = Codec::new(4, 2).unwrap();
    let matrix = codec.parity_matrix().to_vec();
    let all_present = ShardMask::all_present(6);
    let mut workspace = vec![0u8; workspace_len(4, 2)];

    // Seven uniform 8-byte shards, so a call can be given too few or too many.
    let uniform: Vec<Vec<u8>> = (0..7).map(|i| vec![i as u8 + 1; 8]).collect();
    let mut scratch = vec![vec![0u8; 8]; 3];

    // --- splits no codec can be built for ----------------------------------
    seen.push(expect_err(
        "Codec::new(0, 2)",
        Error::NoDataShards,
        Codec::new(0, 2),
    ));
    seen.push(expect_err(
        "Codec::new(0, 2)",
        Error::NoDataShards,
        Codec::new(0, 2),
    ));
    seen.push(expect_err(
        "ParityMatrixRef::new(0, 2, ..)",
        Error::NoDataShards,
        ParityMatrixRef::new(0, 2, &[]),
    ));

    seen.push(expect_err(
        "Codec::new(4, 0)",
        Error::NoParityShards,
        Codec::new(4, 0),
    ));
    seen.push(expect_err(
        "ParityMatrixRef::new(4, 0, ..)",
        Error::NoParityShards,
        ParityMatrixRef::new(4, 0, &[]),
    ));

    seen.push(expect_err(
        "Codec::new(200, 57)",
        Error::TooManyShards { total: 257 },
        Codec::new(200, 57),
    ));
    seen.push(expect_err(
        "Codec::new(1, 256)",
        Error::TooManyShards { total: 257 },
        Codec::new(1, 256),
    ));
    seen.push(expect_err(
        "ParityMatrixRef::new(200, 57, ..)",
        Error::TooManyShards { total: 257 },
        ParityMatrixRef::new(200, 57, &[]),
    ));

    // --- wrong number of shards handed to a built codec --------------------
    seen.push(expect_err(
        "Codec::encode with three data shards",
        Error::ShardCountMismatch {
            expected: 4,
            actual: 3,
        },
        codec.encode(&uniform[..3], &mut scratch[..2]),
    ));
    seen.push(expect_err(
        "Codec::encode with three parity shards",
        Error::ShardCountMismatch {
            expected: 2,
            actual: 3,
        },
        codec.encode(&uniform[..4], &mut scratch),
    ));
    seen.push(expect_err(
        "Codec::encode_all with five shards",
        Error::ShardCountMismatch {
            expected: 6,
            actual: 5,
        },
        codec.encode_all(&mut uniform[..5].to_vec()),
    ));
    seen.push(expect_err(
        "Codec::reconstruct with seven shards",
        Error::ShardCountMismatch {
            expected: 6,
            actual: 7,
        },
        codec.reconstruct(&mut uniform.clone(), &all_present, &mut workspace),
    ));
    // The borrowed view reports the same counts as the owned codecs, because
    // all three go through the same check.
    let borrowed = ParityMatrixRef::new(4, 2, &matrix).unwrap();
    seen.push(expect_err(
        "ParityMatrixRef::encode_all with three shards",
        Error::ShardCountMismatch {
            expected: 6,
            actual: 3,
        },
        borrowed.encode_all(&mut uniform[..3].to_vec()),
    ));

    seen.push(expect_err(
        "ParityMatrixRef::reconstruct with three shards",
        Error::ShardCountMismatch {
            expected: 6,
            actual: 3,
        },
        borrowed.reconstruct(&mut uniform[..3].to_vec(), &all_present, &mut workspace),
    ));
    seen.push(expect_err(
        "ParityMatrixRef::encode with three data shards",
        Error::ShardCountMismatch {
            expected: 4,
            actual: 3,
        },
        borrowed.encode(&uniform[..3], &mut scratch[..2]),
    ));

    // --- malformed shard buffers -------------------------------------------
    let mut ragged: Vec<Vec<u8>> = (0..6).map(|_| vec![0u8; 8]).collect();
    ragged[2].push(0);
    seen.push(expect_err(
        "Codec::encode_all with an over-long data shard",
        Error::ShardLengthMismatch,
        codec.encode_all(&mut ragged.clone()),
    ));
    seen.push(expect_err(
        "Codec::reconstruct with an over-long data shard",
        Error::ShardLengthMismatch,
        codec.reconstruct(&mut ragged, &all_present, &mut workspace),
    ));
    let mut ragged_parity = vec![vec![0u8; 8], vec![0u8; 9]];
    seen.push(expect_err(
        "Codec::encode with an over-long parity shard",
        Error::ShardLengthMismatch,
        codec.encode(&uniform[..4], &mut ragged_parity),
    ));

    let mut empty: Vec<Vec<u8>> = (0..6).map(|_| Vec::new()).collect();
    seen.push(expect_err(
        "Codec::encode_all with zero-length shards",
        Error::EmptyShard,
        codec.encode_all(&mut empty.clone()),
    ));
    seen.push(expect_err(
        "Codec::reconstruct with zero-length shards",
        Error::EmptyShard,
        codec.reconstruct(&mut empty, &all_present, &mut workspace),
    ));

    // --- not enough information to reconstruct -----------------------------
    let three_present: ShardMask = [0usize, 1, 4].into_iter().collect();
    seen.push(expect_err(
        "Codec::reconstruct with three of six shards",
        Error::TooFewShards {
            present: 3,
            required: 4,
        },
        codec.reconstruct(&mut uniform[..6].to_vec(), &three_present, &mut workspace),
    ));
    seen.push(expect_err(
        "Codec::reconstruct with nothing present",
        Error::TooFewShards {
            present: 0,
            required: 4,
        },
        codec.reconstruct(
            &mut uniform[..6].to_vec(),
            &ShardMask::new(),
            &mut workspace,
        ),
    ));

    // --- caller-supplied matrices and workspaces ---------------------------
    // The coefficient count is checked once, in the constructor, so this is the
    // only place a size mismatch can now be raised.
    seen.push(expect_err(
        "ParityMatrixRef::new given a truncated matrix",
        Error::MatrixSizeMismatch {
            expected: 8,
            actual: 7,
        },
        ParityMatrixRef::new(4, 2, &matrix[..7]),
    ));
    let short = workspace.len() - 1;
    seen.push(expect_err(
        "ParityMatrixRef::reconstruct given a workspace one byte short",
        Error::WorkspaceTooSmall {
            required: 12,
            actual: short,
        },
        borrowed.reconstruct(
            &mut uniform[..6].to_vec(),
            &all_present,
            &mut vec![0u8; short],
        ),
    ));

    {
        // A caller who supplies their own coefficients can supply ones that do
        // not come from an MDS code. An all-zero 2x2 parity matrix leaves the
        // codec with a singular system the moment both data shards go
        // missing. Nothing built by `Encoder::new` or `Decoder::new` can reach
        // this, which is exactly why testing it needs a caller-supplied matrix.
        let parity_only: ShardMask = [2usize, 3].into_iter().collect();
        let singular = ParityMatrixRef::new(2, 2, &[0u8; 4]).unwrap();
        seen.push(expect_err(
            "ParityMatrixRef::reconstruct given a singular caller matrix",
            Error::SingularMatrix,
            singular.reconstruct(
                &mut uniform[..4].to_vec(),
                &parity_only,
                &mut vec![0u8; workspace_len(2, 2)],
            ),
        ));
    }

    // Every variant the crate documents as caller-reachable must appear above.
    // `Error` is `#[non_exhaustive]`, so this list is maintained by hand: a new
    // variant a caller can trigger belongs here too.
    let reachable = [
        Error::NoDataShards,
        Error::NoParityShards,
        Error::TooManyShards { total: 0 },
        Error::ShardCountMismatch {
            expected: 0,
            actual: 0,
        },
        Error::ShardLengthMismatch,
        Error::EmptyShard,
        Error::TooFewShards {
            present: 0,
            required: 0,
        },
        Error::MatrixSizeMismatch {
            expected: 0,
            actual: 0,
        },
        Error::WorkspaceTooSmall {
            required: 0,
            actual: 0,
        },
        Error::SingularMatrix,
    ];
    for variant in reachable {
        let wanted = core::mem::discriminant(&variant);
        assert!(
            seen.iter()
                .any(|error| core::mem::discriminant(error) == wanted),
            "no call in this test produced {variant:?}"
        );
    }
}

#[test]
fn every_error_variant_has_a_distinct_display_string() {
    // The Display impl is the only thing a caller sees in a log line, so an
    // accidental duplicate would be a real, if quiet, defect.
    let messages = [
        Error::NoDataShards.to_string(),
        Error::NoParityShards.to_string(),
        Error::TooManyShards { total: 257 }.to_string(),
        Error::ShardCountMismatch {
            expected: 6,
            actual: 5,
        }
        .to_string(),
        Error::ShardLengthMismatch.to_string(),
        Error::EmptyShard.to_string(),
        Error::TooFewShards {
            present: 3,
            required: 4,
        }
        .to_string(),
        Error::MatrixSizeMismatch {
            expected: 8,
            actual: 7,
        }
        .to_string(),
        Error::WorkspaceTooSmall {
            required: 12,
            actual: 11,
        }
        .to_string(),
        Error::SingularMatrix.to_string(),
    ];
    for (i, a) in messages.iter().enumerate() {
        assert!(!a.is_empty(), "variant {i} has an empty Display string");
        for b in &messages[i + 1..] {
            assert_ne!(a, b, "two error variants share the message \"{a}\"");
        }
    }
}

#[test]
fn malformed_input_returns_an_error_and_never_panics() {
    // Every entry point is fed inconsistent shard counts, ragged and empty
    // buffers, presence masks with bits set past the end of the shard slice,
    // undersized workspaces, and matrices sized for different parameters. The
    // contract is that all of that comes back as `Err`, never as a panic, so
    // this test failing at all - by panicking - is the whole assertion.
    let mut rng = Rng::new(0xBAD1_1111_0000);
    let codec = Codec::new(4, 2).unwrap();
    let matrix = codec.parity_matrix().to_vec();

    for _ in 0..100_000 {
        let count = rng.below(9);
        let mut buffers: Vec<Vec<u8>> =
            (0..count).map(|_| vec![rng.byte(); rng.below(5)]).collect();

        // A mask over the real shards, plus a bit that may point past them.
        let mut present = ShardMask::new();
        for i in 0..count {
            present.set(i, rng.below(2) == 0);
        }
        let mut wild = present;
        wild.insert(rng.below(MAX_SHARDS));

        // A split the caller asserts, which is free to be wrong in both halves
        // and to disagree with the coefficient count.
        let claimed_data_shards = rng.below(9);
        let claimed_parity_shards = rng.below(9);
        let mut workspace = vec![0u8; rng.below(32)];

        let _ = codec.reconstruct(&mut buffers, &wild, &mut workspace);
        let _ = codec.reconstruct_data(&mut buffers, &present, &mut workspace);
        let _ = codec.encode_all(&mut buffers);
        if let Ok(borrowed) =
            ParityMatrixRef::new(claimed_data_shards, claimed_parity_shards, &matrix)
        {
            let _ = borrowed.encode_all(&mut buffers);
            let _ = borrowed.reconstruct(&mut buffers, &wild, &mut workspace);
            let _ = borrowed.reconstruct_data(&mut buffers, &present, &mut workspace);
        }

        // The split form, where the data and parity halves can disagree with
        // the declared split independently, and a matrix truncated to an
        // arbitrary length.
        let truncated = rng.below(matrix.len() + 1);
        let split = claimed_data_shards.min(count);
        let (data, parity) = buffers.split_at_mut(split);
        let _ = codec.encode(data, parity);
        if let Ok(borrowed) =
            ParityMatrixRef::new(claimed_data_shards, claimed_parity_shards, &matrix)
        {
            let _ = borrowed.encode(data, parity);
        }
        if let Ok(borrowed) = ParityMatrixRef::new(
            claimed_data_shards,
            claimed_parity_shards,
            &matrix[..truncated],
        ) {
            let _ = borrowed.encode(data, parity);
        }
    }
}

#[test]
fn out_of_range_presence_bits_are_ignored_rather_than_trusted() {
    // A presence mask is caller-supplied and 256 bits wide however many shards
    // there actually are. Bits past the end must not be counted as survivors,
    // or a codec could be talked into believing it has enough information.
    let codec = Codec::new(4, 2).unwrap();
    let mut workspace = codec.workspace();
    let mut rng = Rng::new(0x0F_2A9E_0000);
    let original = encoded(&mut rng, &codec, 32);

    let mut sparse: ShardMask = [0usize, 1].into_iter().collect();
    for i in 6..MAX_SHARDS {
        sparse.insert(i);
    }
    assert_eq!(sparse.count_present(), 2 + (MAX_SHARDS - 6));
    assert_eq!(sparse.count_present_within(6), 2);

    let mut buffers = original.clone();
    assert_eq!(
        codec.reconstruct(&mut buffers, &sparse, &mut workspace),
        Err(Error::TooFewShards {
            present: 2,
            required: 4
        })
    );
    assert_eq!(
        buffers, original,
        "a failed reconstruct must not have written anything"
    );

    // The same mask with enough real survivors succeeds, extra bits and all.
    let mut enough = sparse;
    enough.insert(4);
    enough.insert(5);
    let mut buffers = original.clone();
    poison_missing(&mut buffers, &original, &enough, 0x2B);
    codec
        .reconstruct(&mut buffers, &enough, &mut workspace)
        .unwrap();
    assert_eq!(buffers, original);
}

// ---------------------------------------------------------------------------
// Documented panics
// ---------------------------------------------------------------------------

#[test]
#[should_panic(expected = "count exceeds MAX_SHARDS")]
fn all_present_past_the_shard_limit_panics_as_documented() {
    let _ = ShardMask::all_present(core::hint::black_box(MAX_SHARDS + 1));
}

#[test]
#[should_panic(expected = "zero has no multiplicative inverse")]
fn inverting_zero_panics_as_documented() {
    let _ = gf::inv(core::hint::black_box(0));
}

#[test]
#[should_panic(expected = "division by zero")]
fn dividing_by_zero_panics_as_documented() {
    let _ = gf::div(core::hint::black_box(1), core::hint::black_box(0));
}
