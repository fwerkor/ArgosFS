//! Known-answer tests: the regression barrier for the wire format.
//!
//! Sender and receiver frequently run different versions of an erasure library.
//! If the generator matrix changes between versions, parity shards silently
//! stop reconstructing, and no ordinary unit test notices: both sides remain
//! internally consistent and only disagree with each other. These vectors are
//! what makes that failure loud.
//!
//! Everything here parses `tests/vectors/*.txt` by hand and uses nothing but
//! `std` and the crate under test. That is deliberate: a parser that shares no
//! code with the codec cannot be wrong in the same direction as the codec.
//!
//! `tests/regenerate_vectors.rs` produces the files, and asserts on every
//! `cargo test` that the committed text is still what the generator emits.

mod kat;

use kat::{
    Block, CODEC_FILE, FORMAT_NAME, FORMAT_VERSION, MATRIX_FILE, REQUIRED_PARAMS, data_shard,
    from_hex, parse_blocks, parse_indices, read_vector_file, take_format_header, to_hex,
};
use reed_solomon_engine::{Codec, ParityMatrix, ParityMatrixRef, ShardMask, workspace_len};

/// Byte written over a missing shard before reconstruction, matching the
/// generator. Not zero, so a codec that silently leaves a shard alone fails
/// rather than accidentally passing.
const SCRATCH_FILL: u8 = 0xA5;

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// One pinned parity matrix.
struct MatrixVector {
    data_shards: usize,
    parity_shards: usize,
    coefficients: Vec<u8>,
}

/// One pinned encode/decode case.
struct CaseVector {
    data_shards: usize,
    parity_shards: usize,
    shard_len: usize,
    /// Data shards followed by parity shards, the layout the codec uses.
    shards: Vec<Vec<u8>>,
    erasures: Vec<Vec<usize>>,
}

impl CaseVector {
    fn params(&self) -> (usize, usize) {
        (self.data_shards, self.parity_shards)
    }

    fn total_shards(&self) -> usize {
        self.data_shards + self.parity_shards
    }

    fn label(&self) -> String {
        format!(
            "{}+{} at shard length {}",
            self.data_shards, self.parity_shards, self.shard_len
        )
    }
}

fn load_blocks(relative: &str) -> Vec<Block> {
    let text = read_vector_file(relative);
    let blocks = parse_blocks(&text).unwrap_or_else(|e| panic!("{relative}: {e}"));
    take_format_header(blocks, relative)
}

fn load_matrix_vectors() -> Vec<MatrixVector> {
    load_blocks(MATRIX_FILE)
        .into_iter()
        .map(|block| {
            assert_eq!(block.name, "matrix", "unexpected block in {MATRIX_FILE}");
            let data_shards = block.get_usize("data_shards").unwrap();
            let parity_shards = block.get_usize("parity_shards").unwrap();
            let coefficients = block.get_hex("coefficients").unwrap();
            assert_eq!(
                coefficients.len(),
                data_shards * parity_shards,
                "the pinned matrix for {data_shards}+{parity_shards} has the wrong length"
            );
            MatrixVector {
                data_shards,
                parity_shards,
                coefficients,
            }
        })
        .collect()
}

fn load_case_vectors() -> Vec<CaseVector> {
    load_blocks(CODEC_FILE)
        .into_iter()
        .map(|block| {
            assert_eq!(block.name, "case", "unexpected block in {CODEC_FILE}");
            let data_shards = block.get_usize("data_shards").unwrap();
            let parity_shards = block.get_usize("parity_shards").unwrap();
            let shard_len = block.get_usize("shard_len").unwrap();

            let mut shards = Vec::with_capacity(data_shards + parity_shards);
            for i in 0..data_shards {
                shards.push(block.get_hex(&format!("data[{i}]")).unwrap());
            }
            for j in 0..parity_shards {
                shards.push(block.get_hex(&format!("parity[{j}]")).unwrap());
            }
            for (index, shard) in shards.iter().enumerate() {
                assert_eq!(
                    shard.len(),
                    shard_len,
                    "shard {index} of {data_shards}+{parity_shards} is {} bytes, \
                     shard_len says {shard_len}",
                    shard.len()
                );
            }

            // Catch a stray `data[7]` in a 4-shard case rather than ignoring it.
            let declared = block
                .fields
                .iter()
                .filter(|(key, _)| key.starts_with("data[") || key.starts_with("parity["))
                .count();
            assert_eq!(
                declared,
                data_shards + parity_shards,
                "{data_shards}+{parity_shards} declares {declared} shard fields"
            );

            let erasures: Vec<Vec<usize>> = block
                .all("erasures")
                .into_iter()
                .map(|raw| parse_indices(raw).unwrap())
                .collect();
            assert!(
                !erasures.is_empty(),
                "case {data_shards}+{parity_shards} pins no erasure pattern"
            );
            for pattern in &erasures {
                assert!(
                    pattern.len() <= parity_shards,
                    "erasure pattern {pattern:?} exceeds the correction limit of \
                     {parity_shards}"
                );
                assert!(
                    pattern.iter().all(|&i| i < data_shards + parity_shards),
                    "erasure pattern {pattern:?} indexes past the shard set"
                );
            }

            CaseVector {
                data_shards,
                parity_shards,
                shard_len,
                shards,
                erasures,
            }
        })
        .collect()
}

/// Reports the first differing byte, because comparing two 1200-byte shards by
/// dumping both is useless in a test log.
fn assert_shards_eq(actual: &[Vec<u8>], expected: &[Vec<u8>], context: &str) {
    assert_eq!(
        actual.len(),
        expected.len(),
        "{context}: shard count differs"
    );
    for (index, (a, b)) in actual.iter().zip(expected).enumerate() {
        if a == b {
            continue;
        }
        let offset = a.iter().zip(b).position(|(x, y)| x != y);
        match offset {
            Some(offset) => panic!(
                "{context}: shard {index} differs at byte {offset}: \
                 got 0x{:02x}, expected 0x{:02x}\n  got:      {}\n  expected: {}",
                a[offset],
                b[offset],
                to_hex(&a[..a.len().min(48)]),
                to_hex(&b[..b.len().min(48)]),
            ),
            None => panic!(
                "{context}: shard {index} has length {} but should have {}",
                a.len(),
                b.len()
            ),
        }
    }
}

// ---------------------------------------------------------------------------
// Coverage
// ---------------------------------------------------------------------------

#[test]
fn vector_files_declare_the_format_this_build_understands() {
    for relative in [MATRIX_FILE, CODEC_FILE] {
        let text = read_vector_file(relative);
        let blocks = parse_blocks(&text).unwrap_or_else(|e| panic!("{relative}: {e}"));
        let header = blocks
            .first()
            .unwrap_or_else(|| panic!("{relative} is empty"));
        assert_eq!(header.name, "format");
        assert_eq!(header.get("name").unwrap(), FORMAT_NAME);
        assert_eq!(header.get_usize("version").unwrap() as u32, FORMAT_VERSION);
    }
}

#[test]
fn every_required_parameter_set_is_pinned() {
    // Trimming the vector table would quietly shrink the regression barrier,
    // so the floor is asserted rather than merely documented.
    let matrices = load_matrix_vectors();
    let cases = load_case_vectors();

    for &(data_shards, parity_shards) in REQUIRED_PARAMS {
        assert!(
            matrices
                .iter()
                .any(|m| (m.data_shards, m.parity_shards) == (data_shards, parity_shards)),
            "{MATRIX_FILE} does not pin the matrix for {data_shards}+{parity_shards}"
        );
        assert!(
            cases
                .iter()
                .any(|c| c.params() == (data_shards, parity_shards)),
            "{CODEC_FILE} does not pin an encode case for {data_shards}+{parity_shards}"
        );
    }
}

#[test]
fn pinned_shard_lengths_cover_the_awkward_cases() {
    // A vectorised kernel that mishandles its tail must fail these vectors.
    let cases = load_case_vectors();
    for len in [1usize, 2, 15, 16, 17, 31, 32, 33, 63, 64, 65, 1200] {
        assert!(
            cases.iter().any(|c| c.shard_len == len),
            "no pinned case uses a shard length of {len}"
        );
    }
}

// ---------------------------------------------------------------------------
// The matrix
// ---------------------------------------------------------------------------

#[test]
fn parity_matrices_match_the_committed_vectors() {
    for vector in load_matrix_vectors() {
        let codec = Codec::new(vector.data_shards, vector.parity_shards).unwrap();
        let actual = codec.parity_matrix();
        if actual == vector.coefficients {
            continue;
        }
        let offset = actual
            .iter()
            .zip(&vector.coefficients)
            .position(|(a, b)| a != b)
            .expect("lengths were checked at load time");
        panic!(
            "the parity matrix for {}+{} no longer matches the pinned wire format.\n\
             First difference at coefficient {offset} (row {}, column {}): \
             got 0x{:02x}, pinned 0x{:02x}.\n\
             This is a semver-major change. If it is intended, regenerate the \
             vectors and say so loudly in the changelog.",
            vector.data_shards,
            vector.parity_shards,
            offset / vector.data_shards,
            offset % vector.data_shards,
            actual[offset],
            vector.coefficients[offset],
        );
    }
}

#[test]
fn const_parity_matrices_match_the_committed_vectors() {
    // The `no_std`, no-allocator path builds the matrix in a `const`. It has to
    // land on the same bytes as the runtime path or the wire format would
    // depend on which constructor the caller reached for.
    const FOUR_TWO: ParityMatrix<8> = ParityMatrix::new(4, 2);
    const EIGHT_FOUR: ParityMatrix<32> = ParityMatrix::new(8, 4);
    const SIXTEEN_EIGHT: ParityMatrix<128> = ParityMatrix::new(16, 8);

    let matrices = load_matrix_vectors();
    let pinned = |data_shards: usize, parity_shards: usize| -> Vec<u8> {
        matrices
            .iter()
            .find(|m| (m.data_shards, m.parity_shards) == (data_shards, parity_shards))
            .unwrap_or_else(|| panic!("no pinned matrix for {data_shards}+{parity_shards}"))
            .coefficients
            .clone()
    };

    assert_eq!(FOUR_TWO.coefficients(), pinned(4, 2), "const 4+2 matrix");
    assert_eq!(EIGHT_FOUR.coefficients(), pinned(8, 4), "const 8+4 matrix");
    assert_eq!(
        SIXTEEN_EIGHT.coefficients(),
        pinned(16, 8),
        "const 16+8 matrix"
    );
}

// ---------------------------------------------------------------------------
// Encode
// ---------------------------------------------------------------------------

#[test]
fn pinned_data_shards_match_the_documented_generator() {
    // The file header claims the inputs can be reproduced from its prose alone.
    // This checks that the claim is true of the bytes actually committed, so a
    // reader who reimplements the header gets the same shards.
    for case in load_case_vectors() {
        for i in 0..case.data_shards {
            let expected = data_shard(case.data_shards, case.parity_shards, case.shard_len, i);
            assert_eq!(
                case.shards[i],
                expected,
                "data shard {i} of {} does not match the generator described in \
                 the file header",
                case.label()
            );
        }
    }
}

#[test]
fn encoded_parity_matches_the_committed_vectors() {
    for case in load_case_vectors() {
        let codec = Codec::new(case.data_shards, case.parity_shards).unwrap();

        let mut shards = case.shards.clone();
        for shard in &mut shards[case.data_shards..] {
            shard.fill(0);
        }
        codec.encode_all(&mut shards).unwrap();
        assert_shards_eq(
            &shards,
            &case.shards,
            &format!("encode_all, {}", case.label()),
        );

        // `encode` writes into a separate parity slice, which is a different
        // code path through the same kernel.
        let data = &case.shards[..case.data_shards];
        let mut parity_buffers: Vec<Vec<u8>> = vec![vec![0u8; case.shard_len]; case.parity_shards];
        codec.encode(data, &mut parity_buffers).unwrap();
        assert_shards_eq(
            &parity_buffers,
            &case.shards[case.data_shards..],
            &format!("encode, {}", case.label()),
        );
    }
}

#[test]
fn the_allocation_free_encoder_matches_the_committed_vectors() {
    // Driven by the coefficients read out of the vector file rather than by a
    // freshly built matrix, so this pins how the codec *applies* the matrix
    // independently of how it builds one.
    let matrices = load_matrix_vectors();

    for case in load_case_vectors() {
        let coefficients = &matrices
            .iter()
            .find(|m| (m.data_shards, m.parity_shards) == case.params())
            .unwrap_or_else(|| panic!("no pinned matrix for {}", case.label()))
            .coefficients;
        let matrix =
            ParityMatrixRef::new(case.data_shards, case.parity_shards, coefficients).unwrap();

        let data = &case.shards[..case.data_shards];
        let mut parity_buffers: Vec<Vec<u8>> = vec![vec![0u8; case.shard_len]; case.parity_shards];
        matrix.encode(data, &mut parity_buffers).unwrap();
        assert_shards_eq(
            &parity_buffers,
            &case.shards[case.data_shards..],
            &format!("ParityMatrixRef::encode, {}", case.label()),
        );

        let mut shards = case.shards.clone();
        for shard in &mut shards[case.data_shards..] {
            shard.fill(0);
        }
        matrix.encode_all(&mut shards).unwrap();
        assert_shards_eq(
            &shards,
            &case.shards,
            &format!("ParityMatrixRef::encode_all, {}", case.label()),
        );
    }
}

// ---------------------------------------------------------------------------
// Decode
// ---------------------------------------------------------------------------

#[test]
fn reconstruction_matches_the_committed_vectors() {
    for case in load_case_vectors() {
        let codec = Codec::new(case.data_shards, case.parity_shards).unwrap();
        let mut workspace = codec.workspace();

        for pattern in &case.erasures {
            let (mut shards, present) = damage(&case, pattern);
            codec
                .reconstruct(&mut shards, &present, &mut workspace)
                .unwrap();
            assert_shards_eq(
                &shards,
                &case.shards,
                &format!("reconstruct {pattern:?}, {}", case.label()),
            );
        }
    }
}

#[test]
fn data_only_reconstruction_matches_the_committed_vectors() {
    for case in load_case_vectors() {
        let codec = Codec::new(case.data_shards, case.parity_shards).unwrap();
        let mut workspace = codec.workspace();

        for pattern in &case.erasures {
            let (mut shards, present) = damage(&case, pattern);
            codec
                .reconstruct_data(&mut shards, &present, &mut workspace)
                .unwrap();

            // Missing parity shards are explicitly left as scratch, so only the
            // data half is pinned here.
            assert_shards_eq(
                &shards[..case.data_shards],
                &case.shards[..case.data_shards],
                &format!("reconstruct_data {pattern:?}, {}", case.label()),
            );
            for &index in pattern {
                if index >= case.data_shards {
                    assert!(
                        shards[index].iter().all(|&b| b == SCRATCH_FILL),
                        "reconstruct_data regenerated parity shard {index} for {}",
                        case.label()
                    );
                }
            }
        }
    }
}

#[test]
fn the_allocation_free_decoder_matches_the_committed_vectors() {
    let matrices = load_matrix_vectors();

    for case in load_case_vectors() {
        let coefficients = &matrices
            .iter()
            .find(|m| (m.data_shards, m.parity_shards) == case.params())
            .unwrap_or_else(|| panic!("no pinned matrix for {}", case.label()))
            .coefficients;
        // Sized before the matrix exists, which is the reason `workspace_len`
        // is still a free function.
        let mut workspace = vec![0u8; workspace_len(case.data_shards, case.parity_shards)];
        let matrix =
            ParityMatrixRef::new(case.data_shards, case.parity_shards, coefficients).unwrap();

        for pattern in &case.erasures {
            let (mut shards, present) = damage(&case, pattern);
            matrix
                .reconstruct(&mut shards, &present, &mut workspace)
                .unwrap();
            assert_shards_eq(
                &shards,
                &case.shards,
                &format!("ParityMatrixRef::reconstruct {pattern:?}, {}", case.label()),
            );

            let (mut shards, present) = damage(&case, pattern);
            matrix
                .reconstruct_data(&mut shards, &present, &mut workspace)
                .unwrap();
            assert_shards_eq(
                &shards[..case.data_shards],
                &case.shards[..case.data_shards],
                &format!(
                    "ParityMatrixRef::reconstruct_data {pattern:?}, {}",
                    case.label()
                ),
            );
        }
    }
}

/// Applies an erasure pattern: the listed shards are filled with scratch and
/// marked missing, everything else is left untouched.
fn damage(case: &CaseVector, pattern: &[usize]) -> (Vec<Vec<u8>>, ShardMask) {
    let mut shards = case.shards.clone();
    let mut present = ShardMask::all_present(case.total_shards());
    for &index in pattern {
        shards[index].fill(SCRATCH_FILL);
        present.remove(index);
    }
    (shards, present)
}

// ---------------------------------------------------------------------------
// The parser itself
// ---------------------------------------------------------------------------

#[test]
fn hex_decoding_round_trips_every_byte() {
    let bytes: Vec<u8> = (0..=255u8).collect();
    assert_eq!(from_hex(&to_hex(&bytes)).unwrap(), bytes);
    assert!(from_hex("0").is_err(), "odd-length hex must be rejected");
    assert!(
        from_hex("zz").is_err(),
        "non-hex characters must be rejected"
    );
}

#[test]
fn the_parser_handles_continuations_and_repeated_keys() {
    let text = "\
# leading comment
[format]
name = x
version = 1

[case]
wrapped =
  0011
  2233
erasures = 0
erasures = 1,2
";
    let blocks = parse_blocks(text).unwrap();
    assert_eq!(blocks.len(), 2);
    assert_eq!(
        blocks[1].get_hex("wrapped").unwrap(),
        vec![0x00, 0x11, 0x22, 0x33]
    );
    assert_eq!(blocks[1].all("erasures"), vec!["0", "1,2"]);
    assert_eq!(parse_indices("1,2").unwrap(), vec![1, 2]);
    assert_eq!(parse_indices("1,2,").unwrap(), vec![1, 2]);
}

#[test]
fn the_parser_rejects_malformed_input() {
    assert!(parse_blocks("[unterminated\n").is_err());
    assert!(parse_blocks("[block]\nno equals sign\n").is_err());
    assert!(
        parse_blocks("key = value\n").is_err(),
        "field outside a block"
    );
    assert!(
        parse_blocks("  continuation\n").is_err(),
        "continuation before a field"
    );
}
