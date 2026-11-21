//! Head-to-head against `reed-solomon-simd`.
//!
//! ```text
//! cargo bench --bench comparison
//! cargo bench --bench comparison -- headline
//! ```
//!
//! Excluded from a bare `cargo bench` (`bench = false` in `Cargo.toml`)
//! because it is slow and pulls in a crate that exists here only to be
//! measured. It is a dev-dependency and is not reachable from the library.
//!
//! # Methodology
//!
//! Identical workloads, identical shard bytes, identical erasure patterns, and
//! the same harness for both crates:
//!
//! - Codec objects are constructed once and reused. Initialization is excluded
//!   from every steady-state figure and reported separately, because it is a
//!   real cost that an amortized number hides.
//! - Decode always loses **data** shards, never parity shards, and always the
//!   same ones. Losing a parity shard is just a re-encode, and
//!   `reed-solomon-simd`'s API cannot be asked to regenerate one, so data-only
//!   erasures are the only pattern both can perform.
//! - No shard contents change between iterations, so every iteration of a
//!   measurement performs exactly the same work and no per-iteration reset is
//!   folded into the timing.
//!
//! # Two places this is not perfectly apples to apples, stated up front
//!
//! - `reed-solomon-simd` leaves its recovery shards in internal working
//!   memory and hands out borrowed slices; this crate writes parity into
//!   caller-owned buffers. A consumer of `reed-solomon-simd` that needs to
//!   transmit the parity pays one additional copy of
//!   `parity_shards * shard_bytes` that is not counted here. At 1200-byte
//!   shards that is a fraction of a microsecond, so it does not change any
//!   conclusion.
//! - `reed-solomon-simd`'s GF(2^16) tables are process-global `LazyLock`s, so
//!   only the first codec built in a process pays for them. The "first use"
//!   table below measures that one-shot cost before anything else runs; the
//!   per-call initialization table measures the steady state, which for
//!   `reed-solomon-simd` is allocation only.

mod harness;

use harness::{Budget, Options, Sample, Table, bytes, heading, mib, note, print_environment, us};
use reed_solomon_engine::{Codec, ShardMask};
use reed_solomon_simd::{ReedSolomonDecoder as SimdDecoder, ReedSolomonEncoder as SimdEncoder};
use std::hint::black_box;
use std::time::Instant;

/// The parameter sets from the project brief's comparison table, plus the
/// other two splits the driving consumer's documentation recommends, 8+1 and
/// 4+4. The head-to-head is the table people read to decide whether to adopt
/// this crate, so it should cover the splits they are most likely to run.
const CONFIGS: &[(usize, usize)] = &[(8, 1), (4, 2), (4, 4), (8, 4), (16, 8), (32, 16), (64, 16)];

/// Larger parameter sets, where the FFT engine is expected to pull ahead.
const LARGE_CONFIGS: &[(usize, usize)] = &[(64, 64), (128, 32), (128, 128)];

/// Shard sizes for the size sweep. All must be even for `reed-solomon-simd`.
const SIZES: &[usize] = &[64, 1200, 16384, 65536];

/// Shard size for every table that does not sweep size.
const MTU: usize = 1200;

/// Column labels, in a fixed order so every table reads the same way.
const ENGINE: &str = "rs-engine";
const SIMD: &str = "rs-simd";

fn main() {
    let options = Options::from_args();
    let budget = options.budget();

    // Must happen before anything else touches either crate: the interesting
    // number is the one-shot, process-global table build.
    let first_use = measure_first_use();

    print_environment("reed-solomon-engine vs. reed-solomon-simd");
    println!();
    println!(
        "All figures are microseconds per operation, median of batched samples. \
         Codec objects are reused; initialization is excluded from the steady-state \
         numbers and reported on its own. Lower is better."
    );

    if options.wants("first") {
        report_first_use(&first_use);
    }
    if options.wants("init") {
        bench_init(budget);
    }
    if options.wants("headline") {
        bench_headline(budget);
    }
    if options.wants("sizes") {
        bench_size_sweep(budget);
    }
    if options.wants("large") {
        bench_large(budget);
    }
}

// ---------------------------------------------------------------------------
// First use
// ---------------------------------------------------------------------------

/// One-shot cost of the first codec each crate builds in a fresh process.
struct FirstUse {
    engine_ns: f64,
    simd_ns: f64,
}

fn measure_first_use() -> FirstUse {
    // Each of these is the very first call into its crate in this process, so
    // it carries whatever lazily-initialized global state the crate keeps.
    let start = Instant::now();
    let codec = Codec::new(32, 16).unwrap();
    let engine_ns = start.elapsed().as_secs_f64() * 1e9;
    black_box(&codec);

    let start = Instant::now();
    let codec = SimdCodec::new(32, 16, MTU).unwrap();
    let simd_ns = start.elapsed().as_secs_f64() * 1e9;
    black_box(&codec);

    FirstUse { engine_ns, simd_ns }
}

fn report_first_use(first_use: &FirstUse) {
    heading("First use in a fresh process, 32+16");
    note(
        "A single untimed-loop measurement of the first encoder each crate builds, taken \
         before anything else runs. This is what a short-lived process actually pays. \
         `reed-solomon-simd` builds its GF(2^16) exp/log, multiplication and skew tables \
         here, behind process-global `LazyLock`s. This crate's GF(2^8) tables are `const \
         fn` evaluated into `.rodata`, so there is nothing to build.",
    );

    let mut table = Table::new(&["crate", "first `new`"]);
    table.row(&[ENGINE.to_string(), us(first_use.engine_ns)]);
    table.row(&[SIMD.to_string(), us(first_use.simd_ns)]);
    table.print();
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

fn fill(seed: u64, len: usize) -> Vec<u8> {
    let mut state = seed | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}

/// Data shards followed by the parity shards this crate computes for them.
///
/// `reed-solomon-simd` uses a different field and a different code, so it is
/// not byte-compatible and gets its own recovery shards built from the same
/// data shards.
fn encoded_shards(data_shards: usize, parity_shards: usize, len: usize) -> Vec<Vec<u8>> {
    let total = data_shards + parity_shards;
    let mut shards: Vec<Vec<u8>> = (0..total)
        .map(|i| {
            if i < data_shards {
                fill(i as u64 + 1, len)
            } else {
                vec![0u8; len]
            }
        })
        .collect();
    let encoder = Codec::new(data_shards, parity_shards).unwrap();
    let mut slices: Vec<&mut [u8]> = shards.iter_mut().map(|s| s.as_mut_slice()).collect();
    encoder.encode_all(&mut slices).unwrap();
    drop(slices);
    shards
}

/// Recovery shards as `reed-solomon-simd` computes them.
fn simd_recovery(data: &[Vec<u8>], parity_shards: usize, len: usize) -> Vec<Vec<u8>> {
    let mut encoder = SimdCodec::new(data.len(), parity_shards, len).unwrap();
    for shard in data {
        encoder.add_original_shard(shard).unwrap();
    }
    let result = encoder.encode().unwrap();
    result.recovery_iter().map(<[u8]>::to_vec).collect()
}

/// Whether `reed-solomon-simd` supports a parameter set at all. Its rate
/// constraints are not the same as a matrix code's.
fn simd_supports(data_shards: usize, parity_shards: usize) -> bool {
    SimdEncoder::supports(data_shards, parity_shards)
        && SimdDecoder::supports(data_shards, parity_shards)
}

// ---------------------------------------------------------------------------
// Encode
// ---------------------------------------------------------------------------

fn encode_engine(budget: Budget, data_shards: usize, parity_shards: usize, len: usize) -> Sample {
    let encoder = Codec::new(data_shards, parity_shards).unwrap();
    let reference = encoded_shards(data_shards, parity_shards, len);
    let mut shards = reference.clone();
    for shard in &mut shards[data_shards..] {
        shard.fill(0);
    }

    let mut slices: Vec<&mut [u8]> = shards.iter_mut().map(|s| s.as_mut_slice()).collect();
    encoder.encode_all(&mut slices).unwrap();
    assert!(
        slices
            .iter()
            .zip(&reference)
            .all(|(actual, expected)| *actual == expected.as_slice()),
        "rs-engine encoded the wrong parity for {data_shards}+{parity_shards}"
    );

    harness::measure(budget, || encoder.encode_all(&mut slices).unwrap())
}

fn encode_simd(budget: Budget, data_shards: usize, parity_shards: usize, len: usize) -> Sample {
    let shards = encoded_shards(data_shards, parity_shards, len);
    let data = &shards[..data_shards];
    let reference = simd_recovery(data, parity_shards, len);
    let mut encoder = SimdCodec::new(data_shards, parity_shards, len).unwrap();

    // Verified once, outside the timed region, so that copying the recovery
    // shards out for comparison does not land in rs-simd's number.
    {
        for shard in data {
            encoder.add_original_shard(shard).unwrap();
        }
        let result = encoder.encode().unwrap();
        let produced: Vec<Vec<u8>> = result.recovery_iter().map(<[u8]>::to_vec).collect();
        assert_eq!(
            produced, reference,
            "rs-simd produced unstable recovery shards for {data_shards}+{parity_shards}"
        );
    }

    // Feeding the shards is part of encoding with this API: the encoder copies
    // each one into its own working space and there is no way to skip it.
    harness::measure(budget, || {
        for shard in data {
            encoder.add_original_shard(shard).unwrap();
        }
        let result = encoder.encode().unwrap();
        black_box(result.recovery(0).unwrap().as_ptr());
    })
}

// ---------------------------------------------------------------------------
// Decode
// ---------------------------------------------------------------------------

fn decode_engine(
    budget: Budget,
    data_shards: usize,
    parity_shards: usize,
    len: usize,
    erasures: usize,
) -> Sample {
    let decoder = Codec::new(data_shards, parity_shards).unwrap();
    let reference = encoded_shards(data_shards, parity_shards, len);
    let mut shards = reference.clone();
    let mut present = ShardMask::all_present(data_shards + parity_shards);
    for (index, shard) in shards.iter_mut().enumerate().take(erasures) {
        present.remove(index);
        shard.fill(0xA5);
    }

    let mut slices: Vec<&mut [u8]> = shards.iter_mut().map(|s| s.as_mut_slice()).collect();
    decoder.reconstruct(&mut slices, &present).unwrap();
    assert!(
        slices
            .iter()
            .zip(&reference)
            .all(|(actual, expected)| *actual == expected.as_slice()),
        "rs-engine restored the wrong bytes for {data_shards}+{parity_shards}"
    );

    harness::measure(budget, || {
        decoder.reconstruct(&mut slices, &present).unwrap()
    })
}

fn decode_simd(
    budget: Budget,
    data_shards: usize,
    parity_shards: usize,
    len: usize,
    erasures: usize,
) -> Sample {
    let shards = encoded_shards(data_shards, parity_shards, len);
    let recovery = simd_recovery(&shards[..data_shards], parity_shards, len);
    let survivors: Vec<(usize, &Vec<u8>)> = shards[erasures..data_shards]
        .iter()
        .enumerate()
        .map(|(i, s)| (i + erasures, s))
        .collect();
    let mut decoder = SimdCodec::new(data_shards, parity_shards, len).unwrap();

    // Verified once, outside the timed region: a decoder handed the wrong
    // shard indices would still be fast, and would still be wrong.
    {
        for (index, shard) in &survivors {
            decoder.add_original_shard(*index, shard).unwrap();
        }
        for (index, shard) in recovery.iter().enumerate().take(erasures) {
            decoder.add_recovery_shard(index, shard).unwrap();
        }
        let result = decoder.decode().unwrap();
        for (index, expected) in shards.iter().enumerate().take(erasures) {
            assert_eq!(
                result.restored_original(index),
                Some(expected.as_slice()),
                "rs-simd restored the wrong bytes for shard {index} of \
                 {data_shards}+{parity_shards}"
            );
        }
    }

    harness::measure(budget, || {
        for (index, shard) in &survivors {
            decoder.add_original_shard(*index, shard).unwrap();
        }
        for (index, shard) in recovery.iter().enumerate().take(erasures) {
            decoder.add_recovery_shard(index, shard).unwrap();
        }
        let result = decoder.decode().unwrap();
        black_box(result.restored_original(0).unwrap().as_ptr());
    })
}

// ---------------------------------------------------------------------------
// Sections
// ---------------------------------------------------------------------------

fn bench_init(budget: Budget) {
    heading("Steady-state initialization");
    note(
        "Cost of building one more codec once the process is warm: allocation and the \
         generator-matrix build, which is `O(data_shards^2)` here, a closed form with no \
         inversion. `reed-solomon-simd` reads its tables from process globals here, so \
         its columns are allocation only and understate what the first codec costs by \
         several milliseconds; see the first-use table.",
    );

    let mut table = Table::new(&[
        "config",
        "rs-engine enc",
        "rs-engine dec",
        "rs-simd enc",
        "rs-simd dec",
    ]);
    for &(data_shards, parity_shards) in CONFIGS.iter().chain(LARGE_CONFIGS) {
        let engine_encoder = harness::measure(budget, || Codec::new(data_shards, parity_shards));
        let engine_decoder = harness::measure(budget, || Codec::new(data_shards, parity_shards));
        let (simd_encoder, simd_decoder) = if simd_supports(data_shards, parity_shards) {
            (
                us(
                    harness::measure(budget, || SimdCodec::new(data_shards, parity_shards, MTU))
                        .median_ns,
                ),
                us(
                    harness::measure(budget, || SimdCodec::new(data_shards, parity_shards, MTU))
                        .median_ns,
                ),
            )
        } else {
            ("n/a".to_string(), "n/a".to_string())
        };

        table.row(&[
            format!("{data_shards}+{parity_shards}"),
            us(engine_encoder.median_ns),
            us(engine_decoder.median_ns),
            simd_encoder,
            simd_decoder,
        ]);
    }
    table.print();
}

fn bench_headline(budget: Budget) {
    heading("Encode, 1200-byte shards");
    let mut table = Table::new(&["config", ENGINE, SIMD, "best"]);
    for &(data_shards, parity_shards) in CONFIGS {
        table.row(&encode_row(budget, data_shards, parity_shards, MTU));
    }
    table.print();

    for (label, erasures) in [
        ("one data shard lost", 1usize),
        ("at the correction limit", 0),
    ] {
        heading(&format!("Decode, 1200-byte shards, {label}"));
        if erasures == 0 {
            note(
                "`parity_shards` data shards missing, the most the code can recover. \
                 For the matrix codes this is the same shape of computation as encode. \
                 For the FFT code it is the same cost as losing one shard, which is the \
                 point of the previous table.",
            );
        }
        let mut table = Table::new(&["config", ENGINE, SIMD, "best"]);
        for &(data_shards, parity_shards) in CONFIGS {
            let lost = if erasures == 0 {
                parity_shards.min(data_shards)
            } else {
                erasures
            };
            table.row(&decode_row(budget, data_shards, parity_shards, MTU, lost));
        }
        table.print();
    }
}

fn bench_size_sweep(budget: Budget) {
    heading("Encode across shard sizes");
    note("Microseconds per encode. Two columns per size, in the order rs-engine / rs-simd.");

    let mut table = Table::new(
        &std::iter::once("config".to_string())
            .chain(SIZES.iter().flat_map(|&size| {
                [
                    format!("{}B {ENGINE}", bytes(size)),
                    format!("{}B {SIMD}", bytes(size)),
                ]
            }))
            .collect::<Vec<_>>(),
    );
    for &(data_shards, parity_shards) in CONFIGS {
        let mut row = vec![format!("{data_shards}+{parity_shards}")];
        for &size in SIZES {
            row.push(us(
                encode_engine(budget, data_shards, parity_shards, size).median_ns
            ));
            row.push(if simd_supports(data_shards, parity_shards) {
                us(encode_simd(budget, data_shards, parity_shards, size).median_ns)
            } else {
                "n/a".to_string()
            });
        }
        table.row(&row);
    }
    table.print();

    heading("Decode across shard sizes, one data shard lost");
    note("Microseconds per decode, in the same column order.");

    let mut table = Table::new(
        &std::iter::once("config".to_string())
            .chain(SIZES.iter().flat_map(|&size| {
                [
                    format!("{}B {ENGINE}", bytes(size)),
                    format!("{}B {SIMD}", bytes(size)),
                ]
            }))
            .collect::<Vec<_>>(),
    );
    for &(data_shards, parity_shards) in CONFIGS {
        let mut row = vec![format!("{data_shards}+{parity_shards}")];
        for &size in SIZES {
            row.push(us(decode_engine(
                budget,
                data_shards,
                parity_shards,
                size,
                1,
            )
            .median_ns));
            row.push(if simd_supports(data_shards, parity_shards) {
                us(decode_simd(budget, data_shards, parity_shards, size, 1).median_ns)
            } else {
                "n/a".to_string()
            });
        }
        table.row(&row);
    }
    table.print();
}

fn bench_large(budget: Budget) {
    heading("Large parameter sets, 1200-byte shards");
    note(
        "Published because this is where an `O(n^2)` matrix engine loses, and a \
         comparison that only shows the wins is not worth reading.",
    );

    let mut table = Table::new(&["config", "op", ENGINE, SIMD, "best"]);
    for &(data_shards, parity_shards) in LARGE_CONFIGS {
        let encode = encode_row(budget, data_shards, parity_shards, MTU);
        table.row(&[
            encode[0].clone(),
            "encode".to_string(),
            encode[1].clone(),
            encode[2].clone(),
            encode[3].clone(),
        ]);

        let decode = decode_row(budget, data_shards, parity_shards, MTU, 1);
        table.row(&[
            decode[0].clone(),
            "decode, 1 lost".to_string(),
            decode[1].clone(),
            decode[2].clone(),
            decode[3].clone(),
        ]);
    }
    table.print();

    heading("Encode throughput at 1200-byte shards");
    note("Payload MiB/s, counting only the data bytes fed in.");

    let mut table = Table::new(&["config", ENGINE, SIMD]);
    for &(data_shards, parity_shards) in CONFIGS.iter().chain(LARGE_CONFIGS) {
        let payload = data_shards * MTU;
        let engine = encode_engine(budget, data_shards, parity_shards, MTU);
        let simd = if simd_supports(data_shards, parity_shards) {
            mib(encode_simd(budget, data_shards, parity_shards, MTU).mib_per_s(payload))
        } else {
            "n/a".to_string()
        };
        table.row(&[
            format!("{data_shards}+{parity_shards}"),
            mib(engine.mib_per_s(payload)),
            simd,
        ]);
    }
    table.print();
}

// ---------------------------------------------------------------------------
// Row builders
// ---------------------------------------------------------------------------

fn encode_row(budget: Budget, data_shards: usize, parity_shards: usize, len: usize) -> Vec<String> {
    let engine = encode_engine(budget, data_shards, parity_shards, len).median_ns;
    let simd = simd_supports(data_shards, parity_shards)
        .then(|| encode_simd(budget, data_shards, parity_shards, len).median_ns);
    row(format!("{data_shards}+{parity_shards}"), engine, simd)
}

fn decode_row(
    budget: Budget,
    data_shards: usize,
    parity_shards: usize,
    len: usize,
    erasures: usize,
) -> Vec<String> {
    let engine = decode_engine(budget, data_shards, parity_shards, len, erasures).median_ns;
    let simd = simd_supports(data_shards, parity_shards)
        .then(|| decode_simd(budget, data_shards, parity_shards, len, erasures).median_ns);
    row(
        format!("{data_shards}+{parity_shards} ({erasures} lost)"),
        engine,
        simd,
    )
}

/// Formats one comparison row and names the winner, so a reader does not have
/// to scan the columns to find it.
fn row(label: String, engine: f64, simd: Option<f64>) -> Vec<String> {
    let mut best = (ENGINE, engine);
    if let Some(simd) = simd {
        if simd < best.1 {
            best = (SIMD, simd);
        }
    }
    vec![
        label,
        us(engine),
        simd.map_or_else(|| "n/a".to_string(), us),
        format!("{} ({:.1}x)", best.0, engine / best.1),
    ]
}
