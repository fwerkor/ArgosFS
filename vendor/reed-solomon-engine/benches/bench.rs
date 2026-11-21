//! The `reed-solomon-engine` benchmark sweep.
//!
//! ```text
//! cargo bench --bench bench                  # everything
//! cargo bench --bench bench -- encode        # one section
//! cargo bench --bench bench -- --quick       # coarse, for harness work
//! ```
//!
//! Output is markdown, so a run can be pasted into a README without being
//! retyped, which is the only way published numbers stay honest.
//!
//! # What is measured, and why it is split up this way
//!
//! - **Initialization** gets its own table. `reed-solomon-simd` pays several
//!   milliseconds building GF(2^16) tables on first use; an amortized
//!   throughput figure hides that completely. This crate's field tables are
//!   `const fn` evaluated, so the only runtime cost is building the generator
//!   matrix, which is `O(data_shards^2)` and worth showing rather than
//!   claiming.
//! - **Encode and decode are never averaged together.** Aggregate throughput
//!   is precisely how a fixed decode cost stays invisible.
//! - **Decode is swept over erasure count.** The claim that decode cost scales
//!   with erasures rather than sitting at a floor is only worth making if the
//!   table shows it.
//! - **Kernels are measured against each other**, not just against the clock.
//!   A SIMD kernel is only worth its `unsafe` if the speedup over the scalar
//!   reference is visible, and only worth trusting if it is byte-identical to
//!   it. The `kernels` section checks the second before measuring the first,
//!   and reports the speedup per shard size so that the size at which the win
//!   evaporates is a cell in a table rather than a footnote.
//!
//! `benches/comparison.rs` runs the same workloads against
//! `reed-solomon-simd`.

mod harness;

use harness::{
    Budget, Options, Sample, Table, bytes, heading, mib, note, ns, print_environment, speedup, us,
};
use reed_solomon_engine::{Codec, Kernel, ParityMatrix, ShardMask};

/// Shard sizes swept, in bytes. 1200 is a UDP MTU payload; the driving
/// consumer shards its 1024-byte chunks and adds a header to reach a datagram,
/// so its real shard size sits just under that and in the same regime. The ends
/// of the range are there to show where the per-call overhead stops mattering.
const SIZES: &[usize] = &[64, 256, 1200, 4096, 16384, 65536];

/// The parameter sets the size sweep covers.
///
/// The first three are the three splits the driving consumer's documentation
/// recommends: 8+1 when bandwidth is tight, 4+2 for general use and 4+4 for
/// lossy links. They are cheap to run and they are the shape of the traffic
/// this crate actually sees, so a change that regresses one of them has to show
/// up here. An earlier version of the blocked encode kernel was 50% slower at
/// 8+1 and this sweep could not see it, because it started at 4+2 and went up.
///
/// The rest climb to 64+16, the widest split that consumer will build.
const CONFIGS: &[(usize, usize)] = &[(8, 1), (4, 2), (4, 4), (8, 4), (16, 8), (32, 16), (64, 16)];

/// Larger parameter sets, measured at the MTU size only. These are the range
/// where the GF(2^16) FFT incumbent is expected to win.
const LARGE_CONFIGS: &[(usize, usize)] = &[
    (32, 32),
    (64, 64),
    (128, 32),
    (128, 128),
    (200, 50),
    (250, 5),
];

/// Shard size used by every table that does not sweep size.
const MTU: usize = 1200;

/// Extra shard lengths the cross-kernel agreement check uses on top of
/// [`SIZES`]. Every swept size is a multiple of 16, so on their own they never
/// reach a vector kernel's scalar tail; these do, at every remainder a 16-byte
/// and a 64-byte block can leave behind.
const RAGGED: &[usize] = &[1, 15, 17, 31, 63, 65, 79, 1201];

fn main() {
    let options = Options::from_args();
    let budget = options.budget();

    print_environment("reed-solomon-engine benchmarks");
    println!();
    println!(
        "Figures are the median of batched samples, in microseconds per operation \
         unless stated otherwise. Encoder and decoder objects are built once, outside \
         the timed region."
    );

    if options.wants("init") {
        bench_init(budget);
    }
    if options.wants("encode") {
        bench_encode(budget);
    }
    if options.wants("decode") {
        bench_decode(budget);
    }
    if options.wants("erasures") {
        bench_erasure_sweep(budget);
    }
    if options.wants("kernels") {
        bench_kernels(budget);
    }
    if options.wants("large") {
        bench_large_configs(budget);
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// Deterministic filler, so a rerun measures the same bytes.
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

/// An encoded shard set: data shards followed by their parity shards.
fn encoded_shards(data_shards: usize, parity_shards: usize, len: usize) -> Vec<Vec<u8>> {
    encoded_shards_with(Kernel::detect(), data_shards, parity_shards, len)
}

/// [`encoded_shards`] with the inner kernel pinned.
///
/// Every kernel must produce the same bytes here, which is what
/// [`verify_kernels`] checks and what the assertions inside the timing
/// functions re-check for the exact shape they are about to measure.
fn encoded_shards_with(
    kernel: Kernel,
    data_shards: usize,
    parity_shards: usize,
    len: usize,
) -> Vec<Vec<u8>> {
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
    let encoder = Codec::with_kernel(kernel, data_shards, parity_shards).unwrap();
    let mut slices: Vec<&mut [u8]> = shards.iter_mut().map(|s| s.as_mut_slice()).collect();
    encoder.encode_all(&mut slices).unwrap();
    drop(slices);
    shards
}

/// `reference` with its first `erasures` data shards destroyed and rebuilt by
/// `kernel`.
fn reconstructed_shards_with(
    kernel: Kernel,
    data_shards: usize,
    parity_shards: usize,
    reference: &[Vec<u8>],
    erasures: usize,
) -> Vec<Vec<u8>> {
    let decoder = Codec::with_kernel(kernel, data_shards, parity_shards).unwrap();
    let mut workspace = decoder.workspace();
    let mut shards = reference.to_vec();
    let mut present = ShardMask::all_present(data_shards + parity_shards);
    for (index, shard) in shards.iter_mut().enumerate().take(erasures) {
        present.remove(index);
        shard.fill(0xA5);
    }

    let mut slices: Vec<&mut [u8]> = shards.iter_mut().map(|s| s.as_mut_slice()).collect();
    decoder
        .reconstruct(&mut slices, &present, &mut workspace)
        .unwrap();
    drop(slices);
    shards
}

/// Times `encode_all` with the kernel [`Kernel::detect`] chooses, which is
/// what a caller who never mentions kernels gets.
fn time_encode(budget: Budget, data_shards: usize, parity_shards: usize, len: usize) -> Sample {
    time_encode_with(budget, Kernel::detect(), data_shards, parity_shards, len)
}

/// Times `encoder.encode_all` over a freshly prepared shard set.
///
/// The parity is zeroed and recomputed once before timing starts, and checked
/// against the reference: a benchmark that measures the wrong work is worse
/// than no benchmark. The reference is built by whichever kernel
/// [`encoded_shards`] uses, so this assertion also pins `kernel` to the same
/// bytes as every other kernel.
fn time_encode_with(
    budget: Budget,
    kernel: Kernel,
    data_shards: usize,
    parity_shards: usize,
    len: usize,
) -> Sample {
    let encoder = Codec::with_kernel(kernel, data_shards, parity_shards).unwrap();
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
        "the {kernel} kernel produced the wrong parity for {data_shards}+{parity_shards} \
         at {len}-byte shards"
    );

    harness::measure(budget, || encoder.encode_all(&mut slices).unwrap())
}

/// Times `decoder.reconstruct` with the first `erasures` **data** shards lost.
///
/// Losing data shards rather than parity shards is the case that actually
/// exercises the decoder: a missing parity shard is just a re-encode. It is
/// also the only pattern `reed-solomon-simd` can be asked to perform, so it is
/// what `comparison.rs` uses, and keeping the two identical is what makes the
/// numbers comparable.
///
/// The survivors never change between iterations, so every iteration performs
/// exactly the same work; there is no per-iteration reset to pay for or to
/// accidentally measure.
fn time_decode(
    budget: Budget,
    data_shards: usize,
    parity_shards: usize,
    len: usize,
    erasures: usize,
) -> Sample {
    time_decode_with(
        budget,
        Kernel::detect(),
        data_shards,
        parity_shards,
        len,
        erasures,
    )
}

/// [`time_decode`] with the inner kernel pinned.
fn time_decode_with(
    budget: Budget,
    kernel: Kernel,
    data_shards: usize,
    parity_shards: usize,
    len: usize,
    erasures: usize,
) -> Sample {
    assert!(erasures <= parity_shards.min(data_shards));
    let decoder = Codec::with_kernel(kernel, data_shards, parity_shards).unwrap();
    let mut workspace = decoder.workspace();
    let reference = encoded_shards(data_shards, parity_shards, len);
    let mut shards = reference.clone();

    let mut present = ShardMask::all_present(data_shards + parity_shards);
    for (index, shard) in shards.iter_mut().enumerate().take(erasures) {
        present.remove(index);
        shard.fill(0xA5);
    }

    let mut slices: Vec<&mut [u8]> = shards.iter_mut().map(|s| s.as_mut_slice()).collect();
    decoder
        .reconstruct(&mut slices, &present, &mut workspace)
        .unwrap();
    assert!(
        slices
            .iter()
            .zip(&reference)
            .all(|(actual, expected)| *actual == expected.as_slice()),
        "the {kernel} kernel restored the wrong bytes for {data_shards}+{parity_shards} \
         at {len}-byte shards"
    );

    harness::measure(budget, || {
        decoder
            .reconstruct(&mut slices, &present, &mut workspace)
            .unwrap()
    })
}

/// Times reconstruction when one data shard, and where the code can afford it
/// one parity shard as well, are lost, with and without regenerating the
/// parity shard.
///
/// This is the pattern that makes `reconstruct_data` worth having: a receiver
/// that only wants its payload back should not pay `data_shards` shard
/// operations rebuilding a parity shard it is about to discard.
///
/// The saving exists only when a parity shard is among the erasures, since
/// that is the shard `reconstruct_data` declines to rebuild. A code with a
/// single parity shard has no correctable pattern that loses both it and a
/// data shard, so at those splits the pattern is one lost data shard, the two
/// calls do identical work, and the row reports no saving. That is the honest
/// answer for that shape rather than a gap in the sweep.
fn time_data_only(
    budget: Budget,
    data_shards: usize,
    parity_shards: usize,
    len: usize,
) -> (Sample, Sample) {
    let decoder = Codec::new(data_shards, parity_shards).unwrap();
    let mut workspace = decoder.workspace();
    let reference = encoded_shards(data_shards, parity_shards, len);
    let mut shards = reference.clone();

    let mut present = ShardMask::all_present(data_shards + parity_shards);
    let lost: &[usize] = if parity_shards >= 2 {
        &[0, data_shards]
    } else {
        &[0]
    };
    for &index in lost {
        present.remove(index);
        shards[index].fill(0xA5);
    }

    let mut slices: Vec<&mut [u8]> = shards.iter_mut().map(|s| s.as_mut_slice()).collect();
    decoder
        .reconstruct_data(&mut slices, &present, &mut workspace)
        .unwrap();
    assert!(
        slices[..data_shards]
            .iter()
            .zip(&reference)
            .all(|(actual, expected)| *actual == expected.as_slice()),
        "reconstruct_data restored the wrong bytes for {data_shards}+{parity_shards}"
    );

    let data_only = harness::measure(budget, || {
        decoder
            .reconstruct_data(&mut slices, &present, &mut workspace)
            .unwrap()
    });
    let full = harness::measure(budget, || {
        decoder
            .reconstruct(&mut slices, &present, &mut workspace)
            .unwrap()
    });
    (full, data_only)
}

// ---------------------------------------------------------------------------
// Sections
// ---------------------------------------------------------------------------

fn bench_init(budget: Budget) {
    heading("Initialization");
    note(
        "One-time cost of building a codec. There is no lazy table init, no `OnceLock` \
         and no lock anywhere in this crate: the GF(2^8) log and exp tables are `const fn` \
         evaluated into `.rodata`. What is left is building the generator matrix, which \
         has a closed form (one Lagrange evaluation per entry) and is therefore \
         `O(data_shards^2)`, with no matrix inversion anywhere. `Decoder::new` also \
         allocates the decode workspace.",
    );

    let mut table = Table::new(&["config", "Encoder::new", "Decoder::new"]);
    for &(data_shards, parity_shards) in CONFIGS.iter().chain(LARGE_CONFIGS) {
        let encoder = harness::measure(budget, || Codec::new(data_shards, parity_shards));
        let decoder = harness::measure(budget, || Codec::new(data_shards, parity_shards));
        table.row(&[
            format!("{data_shards}+{parity_shards}"),
            us(encoder.median_ns),
            us(decoder.median_ns),
        ]);
    }
    table.print();

    // The no-allocator path moves the whole cost to compile time. Reading a
    // coefficient out of a `static` is the entire runtime "initialization",
    // and measuring it is the only way to make the claim checkable.
    static CONST_4_2: ParityMatrix<8> = ParityMatrix::new(4, 2);
    static CONST_16_8: ParityMatrix<128> = ParityMatrix::new(16, 8);
    static CONST_64_16: ParityMatrix<1024> = ParityMatrix::new(64, 16);

    heading("Initialization, compile-time matrix (`no_std`, no allocator)");
    note(
        "`ParityMatrix::new` is a `const fn`, so the matrix is built by the compiler and \
         lands in `.rodata`. The figures below time a full read of the coefficient slice \
         from a `static`, which is all that is left to do at runtime.",
    );

    let mut table = Table::new(&["config", "matrix bytes", "read from `static`"]);
    for (label, coefficients) in [
        ("4+2", CONST_4_2.coefficients()),
        ("16+8", CONST_16_8.coefficients()),
        ("64+16", CONST_64_16.coefficients()),
    ] {
        let sample = harness::measure(budget, || {
            std::hint::black_box(coefficients)
                .iter()
                .fold(0u8, |a, &b| a ^ b)
        });
        table.row(&[
            label.to_string(),
            coefficients.len().to_string(),
            us(sample.median_ns),
        ]);
    }
    table.print();
}

fn bench_encode(budget: Budget) {
    heading("Encode");
    note("Microseconds per `encode_all` call, by shard size.");

    let mut table = Table::new(
        &std::iter::once("config".to_string())
            .chain(SIZES.iter().map(|&size| format!("{}B", bytes(size))))
            .collect::<Vec<_>>(),
    );
    for &(data_shards, parity_shards) in CONFIGS {
        let mut row = vec![format!("{data_shards}+{parity_shards}")];
        for &size in SIZES {
            row.push(us(
                time_encode(budget, data_shards, parity_shards, size).median_ns
            ));
        }
        table.row(&row);
    }
    table.print();

    heading("Encode throughput");
    note(
        "Payload MiB/s, counting only the data bytes fed in. Reading across a row shows \
         where the fixed per-call cost stops mattering; reading down a column shows the \
         `O(data_shards * parity_shards)` cost of the matrix engine.",
    );

    let mut table = Table::new(
        &std::iter::once("config".to_string())
            .chain(SIZES.iter().map(|&size| format!("{}B", bytes(size))))
            .collect::<Vec<_>>(),
    );
    for &(data_shards, parity_shards) in CONFIGS {
        let mut row = vec![format!("{data_shards}+{parity_shards}")];
        for &size in SIZES {
            let sample = time_encode(budget, data_shards, parity_shards, size);
            row.push(mib(sample.mib_per_s(data_shards * size)));
        }
        table.row(&row);
    }
    table.print();
}

fn bench_decode(budget: Budget) {
    heading("Decode, worst case");
    note(
        "Microseconds per `reconstruct` call with `parity_shards` data shards missing, \
         which is the most the code can recover and therefore the most work it can be \
         asked to do. Compare cell for cell with the encode table above: at the maximum \
         erasure count decode is the same shape of computation as encode.",
    );

    let mut table = Table::new(
        &std::iter::once("config".to_string())
            .chain(SIZES.iter().map(|&size| format!("{}B", bytes(size))))
            .collect::<Vec<_>>(),
    );
    for &(data_shards, parity_shards) in CONFIGS {
        let erasures = parity_shards.min(data_shards);
        let mut row = vec![format!("{data_shards}+{parity_shards} ({erasures} lost)")];
        for &size in SIZES {
            row.push(us(time_decode(
                budget,
                data_shards,
                parity_shards,
                size,
                erasures,
            )
            .median_ns));
        }
        table.row(&row);
    }
    table.print();

    heading("Decode, one shard lost");
    note(
        "The common case in a lossy transport. This is the number that matters for a \
         receiver, and it is the one an aggregate throughput figure buries.",
    );

    let mut table = Table::new(
        &std::iter::once("config".to_string())
            .chain(SIZES.iter().map(|&size| format!("{}B", bytes(size))))
            .collect::<Vec<_>>(),
    );
    for &(data_shards, parity_shards) in CONFIGS {
        let mut row = vec![format!("{data_shards}+{parity_shards}")];
        for &size in SIZES {
            row.push(us(
                time_decode(budget, data_shards, parity_shards, size, 1).median_ns
            ));
        }
        table.row(&row);
    }
    table.print();

    heading("Decode, data only");
    note(
        "One data shard and one parity shard lost, at 1200-byte shards. \
         `reconstruct` rebuilds both; `reconstruct_data` rebuilds only the data shard \
         and leaves the parity shard as scratch. The difference is the `data_shards` \
         shard operations a receiver spends regenerating parity it is about to throw \
         away.",
    );

    let mut table = Table::new(&["config", "reconstruct", "reconstruct_data", "saved"]);
    for &(data_shards, parity_shards) in CONFIGS {
        let (full, data_only) = time_data_only(budget, data_shards, parity_shards, MTU);
        table.row(&[
            format!("{data_shards}+{parity_shards}"),
            us(full.median_ns),
            us(data_only.median_ns),
            format!(
                "{:.0}%",
                100.0 * (1.0 - data_only.median_ns / full.median_ns)
            ),
        ]);
    }
    table.print();
}

fn bench_erasure_sweep(budget: Budget) {
    heading("Decode cost against erasure count");
    note(
        "Microseconds per `reconstruct` at 1200-byte shards, with `n` data shards \
         missing. A matrix decoder should be roughly linear in `n`, because recovering \
         `n` shards is `n` shard-combinations plus an `O(n^3)` solve on a tiny matrix. \
         A full-domain FFT decoder is flat here, and flat is much worse than linear at \
         the left-hand end of the table.",
    );

    let max_erasures = CONFIGS
        .iter()
        .map(|&(data_shards, parity_shards)| parity_shards.min(data_shards))
        .max()
        .unwrap_or(1);

    let mut table = Table::new(
        &std::iter::once("config".to_string())
            .chain((1..=max_erasures).map(|n| n.to_string()))
            .collect::<Vec<_>>(),
    );
    for &(data_shards, parity_shards) in CONFIGS {
        let limit = parity_shards.min(data_shards);
        let mut row = vec![format!("{data_shards}+{parity_shards}")];
        for erasures in 1..=max_erasures {
            if erasures > limit {
                row.push("-".to_string());
                continue;
            }
            row.push(us(time_decode(
                budget,
                data_shards,
                parity_shards,
                MTU,
                erasures,
            )
            .median_ns));
        }
        table.row(&row);
    }
    table.print();
}

// ---------------------------------------------------------------------------
// Kernels
// ---------------------------------------------------------------------------

/// Every timing taken at one (configuration, shard size, kernel) point.
///
/// Decode is held at two erasure counts because they stress the kernel very
/// differently. At the correction limit the decoder does the most work it can
/// be asked to do, which is where a vector kernel shows its best. With one
/// shard lost it does `data_shards` kernel calls and nothing else, which is
/// where per-call overhead shows worst, and that is the case a receiver on a
/// lossy link actually runs.
#[derive(Clone, Copy)]
struct Point {
    encode: Sample,
    decode_one: Sample,
    decode_limit: Sample,
}

/// The measurement grid: one [`Point`] per configuration, shard size and
/// kernel.
///
/// Everything the kernel section prints is derived from this one grid rather
/// than re-timed per table. Re-timing would make two tables that show the same
/// quantity disagree by whatever the machine's noise happens to be, and a
/// reader has no way to tell that apart from a real difference.
struct Grid {
    kernels: Vec<Kernel>,
    points: Vec<Point>,
}

impl Grid {
    /// Measures every point. `configs` and [`SIZES`] are the two swept axes.
    fn measure(budget: Budget, kernels: &[Kernel]) -> Self {
        let mut points = Vec::with_capacity(CONFIGS.len() * SIZES.len() * kernels.len());
        for &(data_shards, parity_shards) in CONFIGS {
            let limit = parity_shards.min(data_shards);
            for &size in SIZES {
                for &kernel in kernels {
                    points.push(Point {
                        encode: time_encode_with(budget, kernel, data_shards, parity_shards, size),
                        decode_one: time_decode_with(
                            budget,
                            kernel,
                            data_shards,
                            parity_shards,
                            size,
                            1,
                        ),
                        decode_limit: time_decode_with(
                            budget,
                            kernel,
                            data_shards,
                            parity_shards,
                            size,
                            limit,
                        ),
                    });
                }
            }
        }
        Self {
            kernels: kernels.to_vec(),
            points,
        }
    }

    fn at(&self, config: usize, size: usize, kernel: usize) -> Point {
        self.points[(config * SIZES.len() + size) * self.kernels.len() + kernel]
    }
}

/// Which timing a table is reading out of the grid.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Op {
    Encode,
    DecodeOne,
    DecodeLimit,
}

impl Op {
    fn get(self, point: &Point) -> f64 {
        match self {
            Op::Encode => point.encode.median_ns,
            Op::DecodeOne => point.decode_one.median_ns,
            Op::DecodeLimit => point.decode_limit.median_ns,
        }
    }
}

/// Panics unless every supported kernel produces byte-identical output to
/// [`Kernel::SCALAR`], for both encode and decode, at every configuration and
/// every length in [`SIZES`] and [`RAGGED`].
///
/// This runs before any timing. The whole value of the tables below is that
/// the kernels they rank are interchangeable, and that is a claim to check,
/// not to assume.
fn verify_kernels(kernels: &[Kernel]) -> usize {
    let mut points = 0;
    for &(data_shards, parity_shards) in CONFIGS {
        let limit = parity_shards.min(data_shards);
        for &len in SIZES.iter().chain(RAGGED) {
            let expected_encode =
                encoded_shards_with(Kernel::SCALAR, data_shards, parity_shards, len);
            let expected_decode = [1, limit].map(|erasures| {
                reconstructed_shards_with(
                    Kernel::SCALAR,
                    data_shards,
                    parity_shards,
                    &expected_encode,
                    erasures,
                )
            });

            for &kernel in kernels {
                assert_eq!(
                    encoded_shards_with(kernel, data_shards, parity_shards, len),
                    expected_encode,
                    "the {kernel} kernel disagrees with the scalar reference when encoding \
                     {data_shards}+{parity_shards} at {len}-byte shards"
                );
                for (slot, &erasures) in [1, limit].iter().enumerate() {
                    assert_eq!(
                        reconstructed_shards_with(
                            kernel,
                            data_shards,
                            parity_shards,
                            &expected_encode,
                            erasures,
                        ),
                        expected_decode[slot],
                        "the {kernel} kernel disagrees with the scalar reference when \
                         reconstructing {erasures} lost shards of \
                         {data_shards}+{parity_shards} at {len}-byte shards"
                    );
                }
                points += 1;
            }
        }
    }
    points
}

/// Header row shared by every kernel table: two label columns, then one column
/// per shard size.
fn kernel_headers() -> Vec<String> {
    ["config".to_string(), "kernel".to_string()]
        .into_iter()
        .chain(SIZES.iter().map(|&size| format!("{}B", bytes(size))))
        .collect()
}

/// A table of absolute timings, one row per configuration and kernel.
fn kernel_time_table(grid: &Grid, op: Op) {
    let mut table = Table::new(&kernel_headers());
    table.left_align(1);
    for (config_index, &(data_shards, parity_shards)) in CONFIGS.iter().enumerate() {
        for (kernel_index, kernel) in grid.kernels.iter().enumerate() {
            let mut row = vec![
                format!("{data_shards}+{parity_shards}"),
                kernel.name().to_string(),
            ];
            for size_index in 0..SIZES.len() {
                row.push(us(op.get(&grid.at(config_index, size_index, kernel_index))));
            }
            table.row(&row);
        }
    }
    table.print();
}

/// A table of speedups over [`Kernel::SCALAR`], one row per configuration and
/// non-scalar kernel. Nothing is printed when the scalar kernel is the only
/// one this build can run here.
fn kernel_speedup_table(grid: &Grid, scalar_index: usize, op: Op) {
    if grid.kernels.len() < 2 {
        note(
            "This build has only the scalar kernel available on this machine, so there \
             is nothing to compare it against.",
        );
        return;
    }

    let mut table = Table::new(&kernel_headers());
    table.left_align(1);
    for (config_index, &(data_shards, parity_shards)) in CONFIGS.iter().enumerate() {
        for (kernel_index, kernel) in grid.kernels.iter().enumerate() {
            if kernel_index == scalar_index {
                continue;
            }
            let mut row = vec![
                format!("{data_shards}+{parity_shards}"),
                kernel.name().to_string(),
            ];
            for size_index in 0..SIZES.len() {
                let baseline = op.get(&grid.at(config_index, size_index, scalar_index));
                let candidate = op.get(&grid.at(config_index, size_index, kernel_index));
                row.push(speedup(baseline, candidate));
            }
            table.row(&row);
        }
    }
    table.print();
}

fn bench_kernels(budget: Budget) {
    let kernels: Vec<Kernel> = Kernel::supported().collect();
    let scalar_index = kernels
        .iter()
        .position(|&kernel| kernel == Kernel::SCALAR)
        .expect("`Kernel::supported` always yields the scalar reference");
    let names: Vec<&str> = kernels.iter().map(|kernel| kernel.name()).collect();

    heading("Kernels, agreement");
    note(&format!(
        "Kernels this build can run on this machine, most capable first: `{}`. Every one \
         of them is checked against the scalar reference before anything is timed, over \
         every configuration, every swept shard size, and a set of ragged lengths the \
         sweep does not reach because all its sizes are multiples of 16. A benchmark that \
         measures a wrong-but-fast kernel is worse than no benchmark.",
        names.join("`, `")
    ));

    let checked = verify_kernels(&kernels);
    let mut table = Table::new(&["kernel", "points checked", "encode", "decode"]);
    for kernel in &kernels {
        let verdict = if *kernel == Kernel::SCALAR {
            "reference"
        } else {
            "identical"
        };
        table.row(&[
            kernel.name().to_string(),
            (checked / kernels.len()).to_string(),
            verdict.to_string(),
            verdict.to_string(),
        ]);
    }
    table.print();

    let grid = Grid::measure(budget, &kernels);

    heading("Encode by kernel");
    note("Microseconds per `encode_all`, by shard size.");
    kernel_time_table(&grid, Op::Encode);

    heading("Encode speedup over the scalar kernel");
    note(
        "How much the vector kernel is worth, per shard size. The left-hand column is \
         the interesting one: a 64-byte shard is four vector registers of work per kernel \
         call, so the call, the table load and the loop scaffolding are a large fraction \
         of it, and the speedup there is far below what the same kernel reaches at 64 KB. \
         Averaging over shard sizes would hide exactly this.",
    );
    kernel_speedup_table(&grid, scalar_index, Op::Encode);

    heading("Decode by kernel, at the correction limit");
    note(
        "Microseconds per `reconstruct` with `min(data_shards, parity_shards)` data \
         shards missing, which is the most work the decoder can be asked to do and \
         therefore the case that most resembles encode.",
    );
    kernel_time_table(&grid, Op::DecodeLimit);

    heading("Decode speedup over the scalar kernel, at the correction limit");
    kernel_speedup_table(&grid, scalar_index, Op::DecodeLimit);

    heading("Decode speedup over the scalar kernel, one data shard lost");
    note(
        "The common case on a lossy link, and the harshest test of a vector kernel: \
         recovering one shard is `data_shards` kernel calls and a two-by-two solve, so \
         there is very little work to amortize the fixed costs over. Where this table \
         reads near `1.00x` the codec is bound by something other than the field \
         arithmetic, and no kernel can help.",
    );
    kernel_speedup_table(&grid, scalar_index, Op::DecodeOne);

    heading("Cost of one kernel call, encode");
    note(
        "Nanoseconds per `mul_slice`/`mul_add_slice` call, obtained by dividing the \
         encode time by the `data_shards * parity_shards` calls it makes. This is the \
         quantity the SIMD work is trying to reduce. Read along a row: once the figure \
         stops falling in proportion to the shard size, the call is overhead-bound rather \
         than throughput-bound, and that is where the speedup table above collapses.",
    );
    let mut table = Table::new(&kernel_headers());
    table.left_align(1);
    for (config_index, &(data_shards, parity_shards)) in CONFIGS.iter().enumerate() {
        let calls = (data_shards * parity_shards) as f64;
        for (kernel_index, kernel) in grid.kernels.iter().enumerate() {
            let mut row = vec![
                format!("{data_shards}+{parity_shards}"),
                kernel.name().to_string(),
            ];
            for size_index in 0..SIZES.len() {
                row.push(ns(grid
                    .at(config_index, size_index, kernel_index)
                    .encode
                    .median_ns
                    / calls));
            }
            table.row(&row);
        }
    }
    table.print();
}

fn bench_large_configs(budget: Budget) {
    heading("Large parameter sets, 1200-byte shards");
    note(
        "Where an `O(n log n)` FFT engine is expected to win and this matrix engine is \
         expected to lose. Published so the loss is visible rather than omitted. Decode \
         is shown at one erasure and at the correction limit.",
    );

    let mut table = Table::new(&[
        "config",
        "init (enc+dec)",
        "encode",
        "decode, 1 lost",
        "decode, limit",
        "encode MiB/s",
    ]);
    for &(data_shards, parity_shards) in LARGE_CONFIGS {
        let limit = parity_shards.min(data_shards);
        let init_encoder = harness::measure(budget, || Codec::new(data_shards, parity_shards));
        let init_decoder = harness::measure(budget, || Codec::new(data_shards, parity_shards));
        let encode = time_encode(budget, data_shards, parity_shards, MTU);
        let decode_one = time_decode(budget, data_shards, parity_shards, MTU, 1);
        let decode_limit = time_decode(budget, data_shards, parity_shards, MTU, limit);
        table.row(&[
            format!("{data_shards}+{parity_shards}"),
            us(init_encoder.median_ns + init_decoder.median_ns),
            us(encode.median_ns),
            us(decode_one.median_ns),
            format!("{} ({limit})", us(decode_limit.median_ns)),
            mib(encode.mib_per_s(data_shards * MTU)),
        ]);
    }
    table.print();
}
