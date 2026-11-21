//! Shared plumbing for the known-answer vectors.
//!
//! This module holds three things that both the verifier
//! (`tests/known_answer.rs`) and the regenerator
//! (`tests/regenerate_vectors.rs`) need:
//!
//! - the input generator, which turns `(data_shards, parity_shards,
//!   shard_len, shard_index)` into shard bytes,
//! - the parameter table that says which cases are pinned,
//! - the parser and writer for the vector text format.
//!
//! It deliberately depends on nothing but `std`: a parser that shares no code
//! with the encoder is a parser that cannot be wrong in the same direction as
//! the encoder.

// Both test binaries include this module and neither uses all of it.
#![allow(dead_code)]

use std::fmt::Write as _;

/// Value of the `version` field in each vector file's `[format]` block.
///
/// Bumping this is a wire-format event, not a housekeeping one.
pub const FORMAT_VERSION: u32 = 1;

/// Value of the `name` field in each vector file's `[format]` block.
pub const FORMAT_NAME: &str = "reed-solomon-engine-kat";

/// Path of the parity-matrix vector file, relative to the crate root.
pub const MATRIX_FILE: &str = "tests/vectors/parity_matrices.txt";

/// Path of the encode/decode vector file, relative to the crate root.
pub const CODEC_FILE: &str = "tests/vectors/encode_decode.txt";

/// Hex payloads shorter than this many bytes are written on the `key = value`
/// line; longer ones are wrapped into indented continuation lines.
const INLINE_HEX_LIMIT: usize = 32;

/// Bytes per line of a wrapped hex payload.
const HEX_BYTES_PER_LINE: usize = 32;

// ---------------------------------------------------------------------------
// Input generation
// ---------------------------------------------------------------------------

/// SplitMix64, the generator the vector file header specifies.
///
/// Chosen because it is ten lines in any language, has no seeding ritual and no
/// warm-up, and so can be reimplemented from the prose in the file header by
/// someone who has never seen this code. That reimplementability is the whole
/// point of the vectors.
pub struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    /// Seeds the generator directly with `seed` as the initial state.
    pub const fn new(seed: u64) -> Self {
        Self { state: seed }
    }

    /// Advances the state and returns the next 64-bit output.
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// The seed for shard `index` of the case `(data_shards, parity_shards,
/// shard_len)`, exactly as the vector file header specifies it.
///
/// The multiplier is a prime well above any shard count or length used here,
/// so distinct cases cannot collide onto the same seed.
pub fn shard_seed(data_shards: usize, parity_shards: usize, shard_len: usize, index: usize) -> u64 {
    const MIX: u64 = 1_000_003;
    let mut seed = data_shards as u64;
    seed = seed.wrapping_mul(MIX).wrapping_add(parity_shards as u64);
    seed = seed.wrapping_mul(MIX).wrapping_add(shard_len as u64);
    seed = seed.wrapping_mul(MIX).wrapping_add(index as u64);
    seed
}

/// Generates the pinned contents of one data shard.
///
/// The bytes are the little-endian encodings of successive [`SplitMix64`]
/// outputs, concatenated and truncated to `shard_len`.
pub fn data_shard(
    data_shards: usize,
    parity_shards: usize,
    shard_len: usize,
    index: usize,
) -> Vec<u8> {
    let mut rng = SplitMix64::new(shard_seed(data_shards, parity_shards, shard_len, index));
    let mut out = Vec::with_capacity(shard_len);
    while out.len() < shard_len {
        out.extend_from_slice(&rng.next_u64().to_le_bytes());
    }
    out.truncate(shard_len);
    out
}

// ---------------------------------------------------------------------------
// The pinned parameter table
// ---------------------------------------------------------------------------

/// Parameter sets whose parity matrix is pinned.
///
/// The first eleven entries are the coverage floor: dropping any of them is a
/// deliberate reduction in the regression barrier and `known_answer.rs`
/// enforces their presence.
pub const MATRIX_PARAMS: &[(usize, usize)] = &[
    (1, 1),
    (2, 1),
    (4, 2),
    (5, 3),
    (8, 4),
    (16, 8),
    (17, 5),
    (32, 16),
    (64, 16),
    (128, 128),
    (255, 1),
    (3, 2),
    (10, 4),
    (1, 255),
];

/// Parameter sets that `known_answer.rs` requires to be present in both files.
pub const REQUIRED_PARAMS: &[(usize, usize)] = &[
    (1, 1),
    (2, 1),
    (4, 2),
    (5, 3),
    (8, 4),
    (16, 8),
    (17, 5),
    (32, 16),
    (64, 16),
    (128, 128),
    (255, 1),
];

/// One pinned encode/decode case: a parameter set at a specific shard length.
///
/// The shard lengths across the table cover 1, 2, 4, 8, 15, 16, 17, 31, 32, 33,
/// 63, 64, 65 and 1200. The awkward lengths are there so that a future SIMD
/// kernel with a broken tail fails these vectors rather than passing them; 1200
/// is a UDP MTU payload and the driving consumer's normal case.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Case {
    /// Number of data shards.
    pub data_shards: usize,
    /// Number of parity shards.
    pub parity_shards: usize,
    /// Length of every shard, in bytes.
    pub shard_len: usize,
}

impl Case {
    /// Total number of shards.
    pub const fn total_shards(&self) -> usize {
        self.data_shards + self.parity_shards
    }
}

/// The pinned encode/decode cases.
pub const CASES: &[Case] = &[
    case(1, 1, 1),
    case(1, 1, 17),
    case(2, 1, 2),
    case(2, 1, 16),
    case(3, 2, 33),
    case(4, 2, 15),
    case(4, 2, 31),
    case(4, 2, 64),
    case(4, 2, 1200),
    case(5, 3, 63),
    case(8, 4, 32),
    case(8, 4, 65),
    case(10, 4, 1),
    case(16, 8, 32),
    case(17, 5, 15),
    case(32, 16, 16),
    case(64, 16, 8),
    case(128, 128, 4),
    case(255, 1, 4),
    case(1, 255, 2),
];

const fn case(data_shards: usize, parity_shards: usize, shard_len: usize) -> Case {
    Case {
        data_shards,
        parity_shards,
        shard_len,
    }
}

/// Builds the erasure patterns for a case, exactly as the vector file header
/// specifies them.
///
/// Candidates that are empty, out of range or a repeat of an earlier pattern
/// are dropped, which is why small parameter sets end up with fewer patterns
/// than large ones.
pub fn erasure_patterns(data_shards: usize, parity_shards: usize) -> Vec<Vec<usize>> {
    let k = data_shards;
    let m = parity_shards;
    let total = k + m;

    let mut candidates: Vec<Vec<usize>> = Vec::new();

    // E1: the first data shard.
    candidates.push(vec![0]);
    // E2: the first parity shard.
    candidates.push(vec![k]);
    // E3: the last data shard.
    candidates.push(vec![k - 1]);
    // E4: the last data shard and the first parity shard together.
    if m >= 2 {
        candidates.push(vec![k - 1, k]);
    }
    // E5: as many leading data shards as the code can lose.
    candidates.push((0..m.min(k)).collect());
    // E6: every parity shard.
    candidates.push((k..total).collect());
    // E7: m shards taken from the end with stride 2, which straddles the
    // data/parity boundary whenever the code has room for it.
    if m <= k {
        let mut pattern: Vec<usize> = (0..m).map(|i| total - 1 - 2 * i).collect();
        pattern.sort_unstable();
        candidates.push(pattern);
    }

    let mut patterns: Vec<Vec<usize>> = Vec::new();
    for mut pattern in candidates {
        pattern.sort_unstable();
        pattern.dedup();
        if pattern.is_empty() || pattern.len() > m {
            continue;
        }
        if pattern.iter().any(|&i| i >= total) {
            continue;
        }
        if !patterns.contains(&pattern) {
            patterns.push(pattern);
        }
    }
    patterns
}

// ---------------------------------------------------------------------------
// Hex
// ---------------------------------------------------------------------------

const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

/// Encodes bytes as lowercase hex with no separators.
pub fn to_hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX_DIGITS[(b >> 4) as usize] as char);
        out.push(HEX_DIGITS[(b & 0x0F) as usize] as char);
    }
    out
}

/// Decodes lowercase or uppercase hex with no separators.
pub fn from_hex(text: &str) -> Result<Vec<u8>, String> {
    if text.len() % 2 != 0 {
        return Err(format!("hex payload has an odd length of {}", text.len()));
    }
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    let mut i = 0;
    while i < bytes.len() {
        let hi = hex_digit(bytes[i])?;
        let lo = hex_digit(bytes[i + 1])?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Ok(out)
}

fn hex_digit(c: u8) -> Result<u8, String> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(format!("{:?} is not a hex digit", c as char)),
    }
}

// ---------------------------------------------------------------------------
// The text format
// ---------------------------------------------------------------------------

/// One `[name]` block of a vector file, with its fields in file order.
///
/// Fields are kept as an ordered list rather than a map because a block may
/// repeat a key: `erasures` appears once per pinned erasure pattern.
#[derive(Debug, Clone)]
pub struct Block {
    /// The name between the square brackets.
    pub name: String,
    /// `(key, value)` pairs in the order they appear.
    pub fields: Vec<(String, String)>,
    /// 1-based line number of the `[name]` line, for error messages.
    pub line: usize,
}

impl Block {
    /// Returns the value of the first field named `key`.
    pub fn get(&self, key: &str) -> Result<&str, String> {
        self.fields
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .ok_or_else(|| format!("block [{}] at line {} has no `{key}`", self.name, self.line))
    }

    /// Returns every value of every field named `key`, in file order.
    pub fn all(&self, key: &str) -> Vec<&str> {
        self.fields
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .collect()
    }

    /// Returns the value of `key` parsed as a decimal integer.
    pub fn get_usize(&self, key: &str) -> Result<usize, String> {
        let raw = self.get(key)?;
        raw.parse::<usize>().map_err(|e| {
            format!(
                "block [{}] at line {}: `{key} = {raw}` is not a number: {e}",
                self.name, self.line
            )
        })
    }

    /// Returns the value of `key` parsed as a hex byte string.
    pub fn get_hex(&self, key: &str) -> Result<Vec<u8>, String> {
        let raw = self.get(key)?;
        from_hex(raw).map_err(|e| {
            format!(
                "block [{}] at line {}, field `{key}`: {e}",
                self.name, self.line
            )
        })
    }
}

/// Parses a vector file into its blocks.
///
/// The grammar is deliberately tiny, because the point of hand-rolling it is
/// that a reader can confirm by inspection that it does not share a bug with
/// the writer:
///
/// - A line whose first character is a space or a tab continues the value of
///   the most recent field; its trimmed contents are appended with no
///   separator.
/// - Otherwise the line is trimmed. Empty lines and lines starting with `#`
///   are ignored.
/// - `[name]` opens a block.
/// - `key = value` adds a field to the open block. `value` may be empty, in
///   which case the value comes entirely from continuation lines.
pub fn parse_blocks(text: &str) -> Result<Vec<Block>, String> {
    let mut blocks: Vec<Block> = Vec::new();

    for (index, raw) in text.lines().enumerate() {
        let line_no = index + 1;
        let raw = raw.strip_suffix('\r').unwrap_or(raw);

        if raw.starts_with(' ') || raw.starts_with('\t') {
            let piece = raw.trim();
            if piece.is_empty() || piece.starts_with('#') {
                continue;
            }
            let field = blocks
                .last_mut()
                .and_then(|b| b.fields.last_mut())
                .ok_or_else(|| format!("line {line_no}: continuation before any field"))?;
            field.1.push_str(piece);
            continue;
        }

        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        if let Some(rest) = line.strip_prefix('[') {
            let name = rest
                .strip_suffix(']')
                .ok_or_else(|| format!("line {line_no}: block header is missing its `]`"))?;
            blocks.push(Block {
                name: name.trim().to_string(),
                fields: Vec::new(),
                line: line_no,
            });
            continue;
        }

        let (key, value) = line
            .split_once('=')
            .ok_or_else(|| format!("line {line_no}: expected `key = value`, got {line:?}"))?;
        let block = blocks
            .last_mut()
            .ok_or_else(|| format!("line {line_no}: field outside of any block"))?;
        block
            .fields
            .push((key.trim().to_string(), value.trim().to_string()));
    }

    Ok(blocks)
}

/// Parses a comma-separated list of decimal indices.
pub fn parse_indices(text: &str) -> Result<Vec<usize>, String> {
    let mut out = Vec::new();
    for piece in text.split(',') {
        let piece = piece.trim();
        if piece.is_empty() {
            continue;
        }
        out.push(
            piece
                .parse::<usize>()
                .map_err(|e| format!("bad index {piece:?}: {e}"))?,
        );
    }
    Ok(out)
}

/// Accumulates a vector file, wrapping long values the way [`parse_blocks`]
/// expects to read them back.
#[derive(Debug, Default)]
pub struct Writer {
    text: String,
}

impl Writer {
    /// Creates an empty writer.
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends a `#`-prefixed comment line, or a bare `#` for an empty one.
    pub fn comment(&mut self, text: &str) {
        if text.is_empty() {
            self.text.push_str("#\n");
        } else {
            let _ = writeln!(self.text, "# {text}");
        }
    }

    /// Appends a blank separator line.
    pub fn blank(&mut self) {
        self.text.push('\n');
    }

    /// Opens a block.
    pub fn block(&mut self, name: &str) {
        let _ = writeln!(self.text, "[{name}]");
    }

    /// Writes a single-line field.
    pub fn field(&mut self, key: &str, value: &str) {
        let _ = writeln!(self.text, "{key} = {value}");
    }

    /// Writes an integer field.
    pub fn field_usize(&mut self, key: &str, value: usize) {
        self.field(key, &value.to_string());
    }

    /// Writes a hex field, inline when short and wrapped when long.
    pub fn field_hex(&mut self, key: &str, bytes: &[u8]) {
        if bytes.len() <= INLINE_HEX_LIMIT {
            self.field(key, &to_hex(bytes));
            return;
        }
        let _ = writeln!(self.text, "{key} =");
        for chunk in bytes.chunks(HEX_BYTES_PER_LINE) {
            let _ = writeln!(self.text, "  {}", to_hex(chunk));
        }
    }

    /// Writes a comma-separated index list, inline when short and wrapped when
    /// long. Wrapped lines keep a trailing comma so that concatenating them
    /// reproduces the inline form.
    pub fn field_indices(&mut self, key: &str, indices: &[usize]) {
        let inline = indices
            .iter()
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(",");
        if inline.len() + key.len() + 3 <= 96 {
            self.field(key, &inline);
            return;
        }
        let _ = writeln!(self.text, "{key} =");
        for chunk in indices.chunks(16) {
            let piece = chunk
                .iter()
                .map(|i| i.to_string())
                .collect::<Vec<_>>()
                .join(",");
            let _ = writeln!(self.text, "  {piece},");
        }
        // The final trailing comma is harmless: `parse_indices` skips empties.
    }

    /// Consumes the writer and returns the file contents.
    pub fn finish(self) -> String {
        self.text
    }
}

/// Returns the absolute path of a file inside the crate, given its
/// crate-relative path.
pub fn crate_path(relative: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(relative)
}

/// Reads a vector file, with a message that says what to do if it is missing.
pub fn read_vector_file(relative: &str) -> String {
    let path = crate_path(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "cannot read the known-answer vectors at {}: {e}\n\
             regenerate them with:\n  \
             cargo test --test regenerate_vectors -- --ignored",
            path.display()
        )
    })
}

/// Checks a file's `[format]` block and returns the remaining blocks.
pub fn take_format_header(blocks: Vec<Block>, file: &str) -> Vec<Block> {
    let first = blocks.first().unwrap_or_else(|| panic!("{file} is empty"));
    assert_eq!(
        first.name, "format",
        "{file} must open with a [format] block"
    );
    assert_eq!(
        first.get("name").unwrap(),
        FORMAT_NAME,
        "{file} declares an unexpected format name"
    );
    assert_eq!(
        first.get_usize("version").unwrap() as u32,
        FORMAT_VERSION,
        "{file} declares format version {}, this build understands {FORMAT_VERSION}",
        first.get("version").unwrap()
    );
    blocks.into_iter().skip(1).collect()
}
