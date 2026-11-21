//! Regenerator for the known-answer vectors, and the cross-check that keeps
//! the committed bytes honest.
//!
//! The vectors in `tests/vectors/` are the binding definition of this crate's
//! wire format. They are produced here, and every erasure pattern is round
//! tripped before it is written, so a pattern that this crate cannot actually
//! recover never reaches the committed file.
//!
//! Two entry points:
//!
//! - [`committed_vectors_agree_with_the_generator`] runs on every `cargo test`.
//!   It regenerates the files in memory and asserts the committed text matches
//!   byte for byte. A wire-format change therefore fails the test suite twice:
//!   once here and once in `known_answer.rs`.
//! - [`regenerate_known_answer_vectors`] is `#[ignore]`d and rewrites the
//!   files. Changing the wire format means running it deliberately and
//!   reviewing the diff.
//!
//! ```text
//! cargo test --test regenerate_vectors -- --ignored
//! ```
//!
//! The coefficients these vectors pin are guarded independently of this file.
//! The closed form that ships is asserted byte-identical to the Gauss-Jordan
//! reference construction in `src/matrix.rs` for every supported shard split,
//! so a coefficient cannot move without failing that test first.

mod kat;

use kat::{
    CASES, CODEC_FILE, Case, FORMAT_NAME, FORMAT_VERSION, MATRIX_FILE, MATRIX_PARAMS, Writer,
    crate_path, data_shard, erasure_patterns,
};
use reed_solomon_engine::{Codec, ShardMask};

/// Byte written over a missing shard before reconstruction.
///
/// Deliberately not zero: a codec that forgot to overwrite a missing shard,
/// or that treated it as an input, produces visibly wrong bytes rather than
/// accidentally-correct ones.
const SCRATCH_FILL: u8 = 0xA5;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[test]
fn committed_vectors_agree_with_the_generator() {
    for (relative, generated) in [
        (MATRIX_FILE, build_matrix_file()),
        (CODEC_FILE, build_codec_file()),
    ] {
        let path = crate_path(relative);
        let committed = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
        assert!(
            committed == generated,
            "{relative} is not what the generator produces.\n\
             Either the wire format changed, in which case regenerate with\n  \
             cargo test --test regenerate_vectors -- --ignored\n\
             and review the diff, or the file was hand-edited.\n{}",
            first_difference(&committed, &generated)
        );
    }
}

#[test]
#[ignore = "rewrites tests/vectors/; run deliberately and review the diff"]
fn regenerate_known_answer_vectors() {
    for (relative, contents) in [
        (MATRIX_FILE, build_matrix_file()),
        (CODEC_FILE, build_codec_file()),
    ] {
        let path = crate_path(relative);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, contents.as_bytes())
            .unwrap_or_else(|e| panic!("cannot write {}: {e}", path.display()));
        println!("wrote {} ({} bytes)", path.display(), contents.len());
    }
}

/// Describes where two file texts first diverge, so a failure points at a line
/// rather than dumping tens of kilobytes of hex.
fn first_difference(left: &str, right: &str) -> String {
    for (index, (a, b)) in left.lines().zip(right.lines()).enumerate() {
        if a != b {
            return format!(
                "first difference at line {}:\n  committed: {a}\n  generated: {b}",
                index + 1
            );
        }
    }
    format!(
        "the shorter file is a prefix of the other: committed has {} lines, generated has {}",
        left.lines().count(),
        right.lines().count()
    )
}

// ---------------------------------------------------------------------------
// File construction
// ---------------------------------------------------------------------------

fn write_format_block(out: &mut Writer) {
    out.block("format");
    out.field("name", FORMAT_NAME);
    out.field_usize("version", FORMAT_VERSION as usize);
    out.blank();
}

fn write_shared_format_notes(out: &mut Writer) {
    out.comment("---------------------------------------------------------------------------");
    out.comment("FILE FORMAT");
    out.comment("---------------------------------------------------------------------------");
    out.comment("");
    out.comment("Line-oriented, and parsed by hand in tests/known_answer.rs so that the");
    out.comment("verifier shares no code with the encoder:");
    out.comment("");
    out.comment("  * `[name]` opens a block.");
    out.comment("  * `key = value` sets a field of the open block. A key may repeat.");
    out.comment("  * A line beginning with a space or a tab continues the previous field's");
    out.comment("    value; its trimmed text is appended with no separator.");
    out.comment("  * Blank lines, and lines whose first non-space character is `#`, are");
    out.comment("    ignored.");
    out.comment("");
    out.comment("Hex is lowercase with no separators. Payloads longer than 32 bytes are");
    out.comment("wrapped at 32 bytes (64 characters) per continuation line. Index lists are");
    out.comment("comma-separated decimal, wrapped at 16 entries per line with a trailing");
    out.comment("comma so that concatenation reproduces the single-line form.");
}

/// Builds `tests/vectors/parity_matrices.txt`.
fn build_matrix_file() -> String {
    let mut out = Writer::new();

    out.comment("reed-solomon-engine known-answer vectors: parity matrices");
    out.comment("");
    out.comment("GENERATED FILE. Do not hand-edit. Regenerate with");
    out.comment("");
    out.comment("    cargo test --test regenerate_vectors -- --ignored");
    out.comment("");
    out.comment("and read the diff carefully: every byte here is wire format. A build whose");
    out.comment("matrix differs from these coefficients produces parity shards that a build");
    out.comment("matching them cannot reconstruct from, silently. Changing this file is a");
    out.comment("semver-major event.");
    out.comment("");
    out.comment("---------------------------------------------------------------------------");
    out.comment("WHAT IS PINNED");
    out.comment("---------------------------------------------------------------------------");
    out.comment("");
    out.comment("One [matrix] block per pinned parameter set:");
    out.comment("");
    out.comment("  data_shards   = k");
    out.comment("  parity_shards = m");
    out.comment("  coefficients  = the m x k row-major parity matrix P over GF(2^8), hex");
    out.comment("");
    out.comment("P is defined by");
    out.comment("");
    out.comment("  parity[j] = XOR over c in 0..k of ( P[j*k + c] * data[c] )");
    out.comment("");
    out.comment("where the product is GF(2^8) multiplication in GF(2)[x]/p(x) with");
    out.comment("p(x) = 0x11D = x^8 + x^4 + x^3 + x^2 + 1 and primitive element 0x02, applied");
    out.comment("byte-wise across the shard.");
    out.comment("");
    out.comment("---------------------------------------------------------------------------");
    out.comment("HOW P IS CONSTRUCTED");
    out.comment("---------------------------------------------------------------------------");
    out.comment("");
    out.comment("The standard systematic Vandermonde construction:");
    out.comment("");
    out.comment("  1. Form the (k + m) x k Vandermonde matrix V over GF(2^8) with");
    out.comment("     V[r][c] = r^c, taking the row index r as a field element and using the");
    out.comment("     convention 0^0 = 1, so row 0 is [1, 0, 0, ...].");
    out.comment("  2. Let T be the top k x k block of V and set G = V * T^-1. The top block");
    out.comment("     of G is then the identity, which is what makes the code systematic.");
    out.comment("  3. P is the bottom m rows of G.");
    out.comment("");
    out.comment("---------------------------------------------------------------------------");
    out.comment("CROSS-VALIDATION");
    out.comment("---------------------------------------------------------------------------");
    out.comment("");
    out.comment("The coefficients below are produced by a closed form: one Lagrange");
    out.comment("evaluation per entry. That closed form is asserted byte-identical to a");
    out.comment("Gauss-Jordan transcription of the three steps above, which is retained in");
    out.comment("src/matrix.rs as a test-only reference, for every supported shard split. A");
    out.comment("coefficient cannot move here without failing that test first.");
    out.comment("");
    out.comment("The construction is stated in full above so that these bytes can be");
    out.comment("reproduced by an implementation that has never read this crate's source.");
    out.comment("");
    write_shared_format_notes(&mut out);
    out.blank();

    write_format_block(&mut out);

    for &(data_shards, parity_shards) in MATRIX_PARAMS {
        let codec = Codec::new(data_shards, parity_shards).unwrap();
        let coefficients = codec.parity_matrix();

        out.block("matrix");
        out.field_usize("data_shards", data_shards);
        out.field_usize("parity_shards", parity_shards);
        out.field_hex("coefficients", coefficients);
        out.blank();
    }

    out.finish()
}

/// Builds `tests/vectors/encode_decode.txt`.
fn build_codec_file() -> String {
    let mut out = Writer::new();

    out.comment("reed-solomon-engine known-answer vectors: encode and decode");
    out.comment("");
    out.comment("GENERATED FILE. Do not hand-edit. Regenerate with");
    out.comment("");
    out.comment("    cargo test --test regenerate_vectors -- --ignored");
    out.comment("");
    out.comment("and read the diff carefully: every byte here is wire format. Changing this");
    out.comment("file is a semver-major event.");
    out.comment("");
    out.comment("---------------------------------------------------------------------------");
    out.comment("WHAT IS PINNED");
    out.comment("---------------------------------------------------------------------------");
    out.comment("");
    out.comment("One [case] block per (data_shards, parity_shards, shard_len) triple:");
    out.comment("");
    out.comment("  data_shards   = k");
    out.comment("  parity_shards = m");
    out.comment("  shard_len     = L, the length in bytes of every shard");
    out.comment("  data[i]       = the pinned contents of data shard i, for i in 0..k");
    out.comment("  parity[j]     = the parity shard j the encoder must produce, j in 0..m");
    out.comment("  erasures      = one pinned erasure pattern, as comma-separated indices");
    out.comment("                  into data[0..k] followed by parity[0..m]; repeated once");
    out.comment("                  per pattern");
    out.comment("");
    out.comment("Encode: given data[0..k], the encoder must produce parity[0..m] exactly.");
    out.comment("");
    out.comment("Decode: for each `erasures` line, the listed shards are overwritten with");
    out.comment("0xA5 filler and marked missing, the rest are handed to the decoder");
    out.comment("unmodified, and the decoder must restore every listed shard to the bytes");
    out.comment("above, byte for byte.");
    out.comment("");
    out.comment("The shard lengths across the table cover 1, 2, 4, 8, 15, 16, 17, 31, 32,");
    out.comment("33, 63, 64, 65 and 1200. The awkward lengths are deliberate: a vectorised");
    out.comment("kernel with a mishandled tail must fail these vectors rather than pass");
    out.comment("them. 1200 is a UDP MTU payload.");
    out.comment("");
    out.comment("---------------------------------------------------------------------------");
    out.comment("HOW THE DATA SHARDS ARE GENERATED");
    out.comment("---------------------------------------------------------------------------");
    out.comment("");
    out.comment("Data shard i of the case (k, m, L) is the first L bytes of a SplitMix64");
    out.comment("stream whose 64-bit state is seeded with");
    out.comment("");
    out.comment("    seed = ((((k * 1000003 + m) * 1000003) + L) * 1000003 + i)  mod 2^64");
    out.comment("");
    out.comment("Each output is produced by advancing the state:");
    out.comment("");
    out.comment("    state  = (state + 0x9E3779B97F4A7C15)               mod 2^64");
    out.comment("    z      = state");
    out.comment("    z      = ((z XOR (z >> 30)) * 0xBF58476D1CE4E5B9)   mod 2^64");
    out.comment("    z      = ((z XOR (z >> 27)) * 0x94D049BB133111EB)   mod 2^64");
    out.comment("    output = z XOR (z >> 31)");
    out.comment("");
    out.comment("where `>>` is a logical right shift. The shard is the concatenation of the");
    out.comment("little-endian 8-byte encodings of the first, second, ... outputs, truncated");
    out.comment("to L bytes. Ten lines in any language, which is the point: these vectors");
    out.comment("can be reproduced without reading this crate's source.");
    out.comment("");
    out.comment("---------------------------------------------------------------------------");
    out.comment("HOW THE ERASURE PATTERNS ARE CHOSEN");
    out.comment("---------------------------------------------------------------------------");
    out.comment("");
    out.comment("With total = k + m, the candidates below are considered in order. Each is");
    out.comment("sorted ascending and deduplicated, then dropped if it is empty, if it has");
    out.comment("more than m entries, if any entry is >= total, or if an earlier candidate");
    out.comment("produced the same set. Small parameter sets therefore keep fewer patterns.");
    out.comment("");
    out.comment("  E1  {0}                         the first data shard");
    out.comment("  E2  {k}                         the first parity shard");
    out.comment("  E3  {k-1}                       the last data shard");
    out.comment("  E4  {k-1, k}                    across the data/parity boundary, if m >= 2");
    out.comment("  E5  {0, ..., min(m,k)-1}        as many leading data shards as possible");
    out.comment("  E6  {k, ..., k+m-1}             every parity shard");
    out.comment("  E7  {total-1, total-3, ...}     m shards from the end, stride 2, if m <= k");
    out.comment("");
    out.comment("---------------------------------------------------------------------------");
    out.comment("CROSS-VALIDATION");
    out.comment("---------------------------------------------------------------------------");
    out.comment("");
    out.comment("Every erasure pattern below was round tripped at generation time: the");
    out.comment("listed shards were overwritten with filler, reconstructed, and required to");
    out.comment("come back byte for byte. A pattern this crate cannot recover therefore");
    out.comment("never reaches this file.");
    out.comment("");
    out.comment("The parity bytes rest on the matrices in parity_matrices.txt, whose own");
    out.comment("cross-validation is described in that file's header. Both the data shard");
    out.comment("generator and the matrix construction are stated in full so that these");
    out.comment("bytes can be reproduced without reading this crate's source.");
    out.comment("");
    write_shared_format_notes(&mut out);
    out.blank();

    write_format_block(&mut out);

    for case in CASES {
        write_case(&mut out, *case);
    }

    out.finish()
}

fn write_case(out: &mut Writer, case: Case) {
    let Case {
        data_shards,
        parity_shards,
        shard_len,
    } = case;
    let total = case.total_shards();

    let mut shards: Vec<Vec<u8>> = (0..total)
        .map(|i| {
            if i < data_shards {
                data_shard(data_shards, parity_shards, shard_len, i)
            } else {
                vec![0u8; shard_len]
            }
        })
        .collect();

    let codec = Codec::new(data_shards, parity_shards).unwrap();
    codec.encode_all(&mut shards).unwrap();

    out.block("case");
    out.field_usize("data_shards", data_shards);
    out.field_usize("parity_shards", parity_shards);
    out.field_usize("shard_len", shard_len);
    for (i, shard) in shards[..data_shards].iter().enumerate() {
        out.field_hex(&format!("data[{i}]"), shard);
    }
    for (j, shard) in shards[data_shards..].iter().enumerate() {
        out.field_hex(&format!("parity[{j}]"), shard);
    }

    let codec = Codec::new(data_shards, parity_shards).unwrap();
    for pattern in erasure_patterns(data_shards, parity_shards) {
        check_reconstruction(&codec, &shards, &pattern);
        out.field_indices("erasures", &pattern);
    }
    out.blank();
}

/// Round trips `pattern` before it is pinned, so a pattern this crate cannot
/// actually recover never reaches the committed file.
fn check_reconstruction(codec: &Codec, original: &[Vec<u8>], pattern: &[usize]) {
    let data_shards = codec.data_shards();
    let parity_shards = codec.parity_shards();
    let total = original.len();

    let mut workspace = codec.workspace();
    let mut ours: Vec<Vec<u8>> = original.to_vec();
    let mut present = ShardMask::all_present(total);
    for &index in pattern {
        present.remove(index);
        ours[index].fill(SCRATCH_FILL);
    }
    codec
        .reconstruct(&mut ours, &present, &mut workspace)
        .unwrap();
    assert_eq!(
        ours, original,
        "this crate failed to reconstruct {pattern:?} for \
         {data_shards}+{parity_shards}"
    );
}
