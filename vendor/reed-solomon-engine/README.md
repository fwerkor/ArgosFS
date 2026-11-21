# reed-solomon-engine

[![crates.io](https://img.shields.io/crates/v/reed-solomon-engine.svg)](https://crates.io/crates/reed-solomon-engine)
[![docs.rs](https://img.shields.io/docsrs/reed-solomon-engine)](https://docs.rs/reed-solomon-engine)
[![CI](https://github.com/stephenberry/reed-solomon-engine/actions/workflows/ci.yml/badge.svg)](https://github.com/stephenberry/reed-solomon-engine/actions/workflows/ci.yml)
[![MSRV](https://img.shields.io/badge/MSRV-1.85-blue.svg)](https://blog.rust-lang.org/2025/02/20/Rust-1.85.0/)
[![license](https://img.shields.io/badge/license-MIT%20OR%20Apache--2.0-blue.svg)](#license)

Reed-Solomon erasure coding over GF(2^8), in pure Rust, with **zero runtime dependencies**.

```toml
[dependencies]
reed-solomon-engine = "0.1"
```

`cargo tree --edges normal` shows one crate: this one.

This is *erasure coding*, which recovers shards that are known to be missing. It is not error correction: it does not find or fix unknown-corrupt bytes. Detecting corruption is the caller's job, typically with a checksum carried alongside each shard.

---

## Why this exists

Erasure coding has two implementation families, and they sit on opposite sides of a performance cliff.

**FFT engines** win at large shard counts and lose badly at small ones. `reed-solomon-simd` is the strong Rust example: genuinely good work, and fast from about 16+8 up. But its decode path pays a **fixed cost of roughly 170 microseconds regardless of parameters**, because the Walsh-Hadamard transform it uses to locate erasures runs over a 65536-point domain whether you lost one shard or fifty. It also pays several milliseconds of one-time initialisation: 3.8 ms as re-measured here. If you are recovering one lost shard out of six, you pay all of it.

**Matrix engines** are the opposite: decode that costs what the erasure count costs, and `O(n^2)` asymptotics that lose once the shard count climbs. That is the right trade for small parameters, and it is the trade this crate makes.

So below roughly 16+8 you want the matrix code, above it you want the FFT code, and picking correctly has meant benchmarking first.

The goal of `reed-solomon-engine` is to be the crate you can reach for without running that benchmark, and without inheriting a dependency tree you did not ask for. It is a matrix engine with SIMD on by default, zero runtime dependencies, and decode cost that scales with what you actually lost.

Both claims about `reed-solomon-simd` above have been re-measured by this repository's own harness and hold: it decodes 4+2 in 153 microseconds, against 0.158 for this crate, and pays several milliseconds on first use. See [Benchmarks](#benchmarks) for the full head-to-head, including where this crate loses.

### What is true today

Version 0.1 is a correct scalar core **plus the SIMD kernels that make it fast**. What it gives you:

- **Zero runtime dependencies.** `[dependencies]` is empty and is meant to stay that way.
- **SIMD kernels, selected at runtime, with no dependency.** NEON on aarch64, AVX2 and SSSE3 on x86 and x86-64. On a 1200-byte shard the NEON kernel is worth 19x to 33x over the scalar path on encode, depending on the split, and 14x to 20x on decode. See [Choosing a kernel](#choosing-a-kernel).
- **`no_std`, and `no_std` without an allocator.** The encode and decode primitives work on caller-provided buffers. Verified in CI by building for `thumbv7em-none-eabi`.
- **Zero initialisation cost.** Every table, including the 8 KiB of SIMD shuffle tables, and optionally the generator matrix itself, is `const fn` evaluated. No `OnceLock`, no lock, nothing to warm up.
- **Decode cost that scales with erasures.** Recovering two lost shards out of eighty solves a 2x2 linear system, not an 80x80 one.
- **Byte-compatible with the standard GF(2^8) matrix format.** The systematic Vandermonde construction with polynomial `0x11D`, which is what the widely deployed implementations of this code use. You can put it on one side of a link without a protocol version bump. Specified in [docs/WIRE_FORMAT.md](docs/WIRE_FORMAT.md).
- **`unsafe` confined to the SIMD kernels.** The crate root is `#![deny(unsafe_code)]` and `#![forbid(unsafe_op_in_unsafe_fn)]`. The only opt-outs are the two kernel modules and the three dispatch methods that enter them, and every block names the CPU feature check that makes it sound. Indexing inside the kernels stays bounds-checked; the `unsafe` is the intrinsics and nothing else. Every kernel is tested byte-identical to the scalar reference.

Where it still loses, and to whom, is in [Benchmarks](#benchmarks). The short version: `reed-solomon-simd`'s `O(n log n)` engine wins on encode from 16+8 up, and that gap closes only with an FFT engine of our own. See [dev/ROADMAP.md](dev/ROADMAP.md) and [docs/DESIGN.md](docs/DESIGN.md).

---

## Quick start

The snippets below are bodies of a function returning `Result<(), reed_solomon_engine::Error>`; that is where the `?` goes. Each one is complete and compiles as written.

```rust
use reed_solomon_engine::{Codec, ShardMask};

let codec = Codec::new(4, 2)?;
let mut workspace = codec.workspace();

// Data shards first, then space for the parity shards. Any container of byte
// slices works, so pass whatever you already hold.
let mut shards: Vec<Vec<u8>> = vec![
    b"alpha".to_vec(),
    b"bravo".to_vec(),
    b"charl".to_vec(),
    b"delta".to_vec(),
    vec![0; 5],
    vec![0; 5],
];

codec.encode_all(&mut shards)?;

// Any two of the six shards may go missing.
let mut present = ShardMask::all_present(6);
present.remove(1);
present.remove(5);

codec.reconstruct(&mut shards, &present, &mut workspace)?;
assert_eq!(shards[1], b"bravo");
```

One `Codec` does both directions and allocates once, in its constructor, and never again. Build one per stream and reuse it.

Every method takes `&self`, because the generator matrix is only ever read. Decode scratch is the one mutable part, and it is passed in, so a single `Codec` can be shared, including across threads, with one workspace per decoding thread and no lock between them. `codec.workspace()` allocates a correctly sized one.

If you only want your payload back and are about to throw the parity shards away, use `reconstruct_data`, which skips regenerating them. That saves `data_shards` shard operations per missing parity shard.

### Separate data and parity buffers

```rust
use reed_solomon_engine::Codec;

let codec = Codec::new(4, 2)?;

let data: [&[u8]; 4] = [b"north", b"south", b"east!", b"west!"];
let (mut p0, mut p1) = ([0u8; 5], [0u8; 5]);
let mut parity: [&mut [u8]; 2] = [&mut p0, &mut p1];

codec.encode(&data, &mut parity)?;
```

Shards may be any equal, non-zero length. Nothing in the format depends on the length.

Both arguments are slices of anything that is a byte slice, so `&[&[u8]]`, `&[Vec<u8>]`, `&[[u8; N]]` and `&[Box<[u8]>]` all work as they are. There is no adapter slice to build per call, which matters for a per-packet codec: collecting one is an allocation on the hot path spent satisfying a type rather than doing any work.

---

## `no_std` and no allocator

With `default-features = false` the crate is `no_std` and allocation-free. Build the generator matrix in a `const` or `static` and call the codec on it:

```rust
use reed_solomon_engine::{ParityMatrix, ShardMask};

// The split is stated once, here, and every call below is checked against it.
static MATRIX: ParityMatrix<8> = ParityMatrix::new(4, 2);

let mut buffers = [*b"alpha", *b"bravo", *b"charl", *b"delta", [0; 5], [0; 5]];
let mut shards: [&mut [u8]; 6] = {
    let [a, b, c, d, e, f] = &mut buffers;
    [a, b, c, d, e, f]
};

MATRIX.encode_all(&mut shards)?;

// Decoding needs a workspace, and the matrix knows how big at compile time.
let mut workspace = [0u8; MATRIX.workspace_len()];

let mut present = ShardMask::all_present(6);
present.remove(0);
shards[0].fill(0);

MATRIX.reconstruct(&mut shards, &present, &mut workspace)?;
assert_eq!(shards[0], b"alpha");
```

The shard split appears exactly once, in the type. That is deliberate: coefficients on their own cannot say what shape they are, because a 4+2 matrix and a 2+4 one hold the same eight bytes, so an API that takes the split again at each call has to be told the truth every time or it silently produces parity nothing can reconstruct. Stating it where the matrix is built removes the chance.

`ParityMatrix::new` is a `const fn`, so the matrix lands in `.rodata` and there is nothing to initialise at runtime. Its `CAP` parameter is the inline array capacity and must be at least `data_shards * parity_shards`. It accepts every split the crate supports, up to 255 data shards. Building the matrix is `O(data_shards^2)` field operations, which stays inside the compiler's const-eval step budget at every size: the largest expressible split costs about 0.6 seconds of compile time and needs no `#![allow(long_running_const_eval)]` anywhere.

For coefficients the compiler cannot compute, use `ParityMatrixRef`, which borrows them and carries the same split:

```rust
use reed_solomon_engine::ParityMatrixRef;

// Coefficients from anywhere: a file, flash, a linker section, a frame header.
let coefficients: &[u8] = &[27, 28, 18, 20, 28, 27, 20, 18];
let matrix = ParityMatrixRef::new(4, 2, coefficients)?;

let data: [&[u8]; 4] = [b"alpha", b"bravo", b"charl", b"delta"];
let (mut p0, mut p1) = ([0u8; 5], [0u8; 5]);
let mut parity: [&mut [u8]; 2] = [&mut p0, &mut p1];

matrix.encode(&data, &mut parity)?;
```

`ParityMatrixRef::new` is also a `const fn`, so a matrix in `.rodata` can be wrapped in a `const`. It checks that the coefficient count matches the split; it cannot check the split itself, which is why the split belongs next to the bytes at the point where their provenance is known. `Codec::parity_matrix` produces exactly the layout it expects, so a matrix can be built on a host and reloaded on a target with no allocator.

`ci/no-std-check/` in this repository is a standalone crate that builds for `thumbv7em-none-eabi` with no allocator and no panic runtime. It exercises both matrix types, including a shard split that only arrives at runtime. CI builds it on every push.

Runtime CPU feature detection is unavailable without `std`, so a `no_std` build selects its kernel from `#[cfg(target_feature = ...)]` at compile time. On aarch64 that costs nothing, because NEON is in the baseline and is always chosen. On x86 it means the scalar kernel unless you opt in with `RUSTFLAGS="-C target-feature=+avx2"`, `+ssse3`, or a `-C target-cpu` that implies one of them. See [Choosing a kernel](#choosing-a-kernel).

---

## Feature flags

| feature | default | effect |
|---|---|---|
| `std` | yes | implies `alloc`; enables runtime CPU feature detection for SIMD kernel selection |
| `alloc` | via `std` | enables `Codec`, which allocates only in its constructor |

With no features enabled the crate is `no_std` and allocation-free, and the API is `ParityMatrix` for a matrix the compiler builds, `ParityMatrixRef` for coefficients that arrive from somewhere else, plus `Kernel`, `ShardMask` and `workspace_len`. Both matrix types expose `encode`, `encode_all`, `reconstruct` and `reconstruct_data`.

---

## Choosing a kernel

The inner loop of every encode and every decode is "multiply a shard by a field scalar and XOR it into an accumulator". `Kernel` is which implementation of that runs.

You do not have to think about this. `Codec::new` calls `Kernel::detect` once and keeps the answer; `ParityMatrix` and `ParityMatrixRef` cannot, because their constructors are `const fn`, so they detect per operation unless a kernel is pinned with `ParityMatrixRef::with_kernel`. Either way the choice is the fastest kernel the current CPU can run:

| target | kernels, in preference order |
|---|---|
| aarch64 | `neon`, then `scalar` |
| x86 and x86-64 | `avx2`, then `ssse3`, then `scalar` |
| everything else | `scalar` |

On aarch64 the choice is made at compile time, because NEON is mandatory in the baseline. On x86 with the `std` feature it comes from `is_x86_feature_detected!`, which caches its answer after the first call; detection never happens per shard. Without `std` there is no runtime detection available anywhere, and the choice falls back to `#[cfg(target_feature = ...)]` as described above.

A `Kernel` can only be obtained from `Kernel::SCALAR`, `Kernel::detect` or `Kernel::supported`, all of which return something this CPU can actually execute. That is a soundness requirement rather than a convenience, which is why the variants are not public to construct.

To force one, for benchmarking or to pin the reference path:

```rust
use reed_solomon_engine::{Codec, Kernel};

// What a caller who never mentions kernels gets.
println!("fastest available: {}", Kernel::detect());

// Everything this build can run here, most capable first.
for kernel in Kernel::supported() {
    println!("supported: {kernel}");
}

// The portable reference, available on every target.
let reference = Codec::with_kernel(Kernel::SCALAR, 4, 2)?;
assert_eq!(reference.kernel(), Kernel::SCALAR);
```

**The output bytes are identical whichever kernel you pick**, and the tests enforce that; see [Wire-format stability](#wire-format-stability).

---

## Limits and errors

- `data_shards >= 1`, `parity_shards >= 1`, `data_shards + parity_shards <= 256`. The 256 limit comes from the field: the construction needs one distinct GF(2^8) element per shard.
- All shards must have the same non-zero length, including the ones marked missing.
- Reconstruction needs at least `data_shards` shards present. Below that it is information-theoretically impossible and you get `Error::TooFewShards`.

`Error` is a small non-exhaustive `Copy` enum that carries no allocation and implements `core::error::Error`. The public API does not panic.

---

## MSRV

**Rust 1.85**, declared as `rust-version` in `Cargo.toml` and tested in CI. An MSRV bump is a minor-version change with a changelog note, never a patch.

Edition 2024, which is what sets the floor: 1.85 is the first compiler that accepts it.

---

## Wire-format stability

The generator matrix is the systematic Vandermonde construction over GF(2^8) with reduction polynomial `0x11D`. It is **specified in prose** in [docs/WIRE_FORMAT.md](docs/WIRE_FORMAT.md), precisely enough to reimplement without reading the source, complete with a worked 4+2 example you can check by hand.

The commitment: **the bytes this crate produces will not change except in a semver-major release with a loud changelog entry.** Sender and receiver frequently run different versions of an erasure library, and a matrix change makes parity shards silently stop reconstructing. That is the highest-consequence failure mode in a library like this, and it is not caught by ordinary unit tests. It is pinned by known-answer vectors checked in CI, not by hope.

This is the de facto interchange format for GF(2^8) Reed-Solomon erasure coding, and implementations of it exist in several languages. What is tested here is that this crate matches the specification: the shipping closed-form construction is asserted byte-identical to an independent Gauss-Jordan construction of the same matrix for all 32,640 supported shard splits, and the resulting bytes are pinned by the known-answer vectors. Interoperating across languages is a matter of both sides implementing [docs/WIRE_FORMAT.md](docs/WIRE_FORMAT.md); if you are relying on that, generate vectors from your side and check them against `tests/vectors/` first.

**The SIMD kernels do not touch any of this.** Which kernel runs is an implementation detail of how `mul` is evaluated, never of what the bytes are: every kernel is required to be byte-identical to the scalar reference, the cross-kernel tests check that for every scalar and every shard length that exercises a vector tail, and the known-answer vectors are checked with whichever kernel the CI machine detects. A shard encoded with AVX2 and reconstructed with NEON is the same shard.

Compatibility is *not* claimed with the GF(2^16) additive-FFT family. Different field, different code.

---

## Benchmarks

Measured on an Apple M1 Pro (aarch64, NEON kernel), 1200-byte shards, release profile, codec objects reused, median of batched samples. Microseconds per operation, lower is better. Reproduce with `cargo bench --bench comparison` for the head-to-head and `cargo bench` for the by-kernel tables.

**These are aarch64 figures.** The SSSE3 and AVX2 kernels are correctness-tested on x86-64 in CI and, during development, under Rosetta 2 (`ROSETTA_ADVERTISE_AVX=1` makes AVX2 visible there), where both pass the full suite including the wire-format vectors. But translated execution says nothing useful about speed, and no native x86 hardware was available, so no x86 numbers are published here. Do not assume the aarch64 ratios carry over.

### What the SIMD kernels bought

`cargo bench` runs every kernel the machine supports and prints the ratio between them, over the same inputs, so this is one kernel against another rather than one run against another:

| operation, 16+8 | 64 B | 256 B | 1200 B | 4 KB | 16 KB | 64 KB |
|---|---:|---:|---:|---:|---:|---:|
| encode | 14.2x | 25.0x | 30.9x | 33.3x | 29.3x | 28.8x |
| decode, one shard lost | 5.5x | 12.1x | 19.3x | 21.9x | 23.1x | 22.9x |
| decode, at the correction limit | 3.3x | 7.9x | 16.2x | 20.7x | 22.3x | 23.0x |

In absolute terms at 4+2 and 1200-byte shards, encode went from 5.38 microseconds on the scalar kernel to 0.259 on NEON, and decode with one shard lost from about 2.7 to 0.158.

The encode row sits well above the decode rows, and that is recent. Encode hands the whole matrix to the kernel in one call, so the NEON path computes up to eight parity rows in one pass over the data, splitting each source byte's nibbles once for all of them instead of once per row. Decode does not: at one or two erasures there are one or two output rows and nothing to block over, so it still drives the kernel a row at a time and its ratios are where they were. The same is true of encode below four parity shards, which takes the row-major path for the same reason; at 4+2 the encode speedup is 20.7x rather than 16+8's 30.9x.

Two things this table is deliberately not hiding. The speedup is far lower on small shards, because a 64-byte shard is four vector registers of work and the call, the table load and the loop scaffolding are a large fraction of it. And decode at the correction limit gains least of all at 64 bytes, because that path spends proportionally more time in the `e x e` solve, which no kernel touches. Averaging over shard sizes would conceal both.

### Where it wins: decode

| config | rs-engine | `reed-solomon-simd` |
|---|---:|---:|
| 8+1, 1 lost | **0.296** | 154.6 |
| 4+2, 1 lost | **0.158** | 152.4 |
| 4+4, 1 lost | **0.160** | 152.5 |
| 8+4, 1 lost | **0.298** | 154.7 |
| 16+8, 1 lost | **0.571** | 160.1 |
| 32+16, 1 lost | **1.12** | 172.4 |
| 64+16, 1 lost | **2.19** | 200.9 |
| 128+128, 1 lost | **4.68** | 308.5 |

`reed-solomon-simd` pays a ~152 microsecond floor that does not scale down with shard count or erasure count. This crate is **960x faster than it at 4+2**, and still 66x faster at 128+128, the split where the FFT engine is otherwise at its strongest.

Decode cost is linear in the number of erasures, not flat:

| config | 1 lost | 2 | 4 | 8 | 16 |
|---|---:|---:|---:|---:|---:|
| 32+16 | 1.11 | 2.24 | 4.58 | 10.2 | 25.3 |
| `reed-solomon-simd`, same sweep | ~172 | ~172 | ~172 | ~172 | ~172 |

### Encode: ahead up to 8+4, behind above that

| config | rs-engine | `reed-solomon-simd` |
|---|---:|---:|
| 8+1 | **0.258** | 0.355 |
| 4+2 | **0.253** | 0.278 |
| 4+4 | **0.427** | 0.476 |
| 8+4 | **0.769** | 0.877 |
| 16+8 | 2.78 | **2.39** |
| 32+16 | 10.9 | **6.23** |
| 64+16 | 21.6 | **11.8** |
| 64+64 | 86.9 | **23.1** |
| 128+128 | 372 | **58.2** |

Ahead at every split through 8+4, by 1.4x at 8+1, 1.10x at 4+2, 1.11x at 4+4 and 1.14x at 8+4, and then behind: 1.2x at 16+8, 1.7x at 32+16, 1.8x at 64+16, 3.8x at 64+64, 6.4x at 128+128. That remaining gap is `O(n^2)` against `O(n log n)`, and no kernel work closes it. It closes, if at all, with the FFT engine in M5.

The upper half of that table moved with the blocked multi-row encode kernel, measured back to back against the row-major loop it replaced: 1.19x at 4+4, 1.32x at 8+4, 1.43x at 16+8 and 1.47x at 64+16. Splits with fewer than four parity shards take the row-major path unchanged, so 8+1 and 4+2 are exactly where they were.

**What that kernel cost to get right, since the first version of it was worse.** It originally blocked in groups of 8, 4, 2 and 1, which made encode *slower* than the loop it replaced wherever the parity count was small: 50% slower at 8+1, 25% at 4+3, 8% at 4+2. Setting a group up costs the same whether it holds one row or eight, and below four rows that setup costs more than the blocking saves. The benchmark sweep could not see any of this, because it began at 4+2 and climbed. Both sweeps now begin at 8+1 and include 4+4, the splits this crate's driving consumer actually recommends. [docs/DESIGN.md](docs/DESIGN.md) section 7 has the measurements, including the argument for the narrow groups that the numbers overturned.

Across shard sizes the small-split lead holds throughout. At 8+4 this crate is ahead of `reed-solomon-simd` at every size from 64 B (0.091 against 0.114) to 64 KB (42.6 against 52.0), and the same is true at 8+1, 4+2 and 4+4. At 16+8 and above it is behind at every size.

### The one place decode loses too

Decode at or near the **correction limit with large parameter sets** inherits the same asymptotic loss as encode, and adds one of its own. Recovering `e` shards solves an `e x e` system whose elimination costs `Theta(e^2 * (e + k))` field operations, and at 128+128 losing 128 shards that is about 84% of the whole decode rather than a rounding error. This path also still drives the kernel one output row at a time, so the blocked encode kernel did not move these figures at all:

| config, at the correction limit | rs-engine | `reed-solomon-simd` |
|---|---:|---:|
| 32+16, 16 lost | **25.3** | 173 |
| 64+16, 16 lost | **47.4** | 202 |
| 64+64, 64 lost | 507 | **~216** |
| 128+128, 128 lost | 3638 | **~309** |

The crossover is not the shard count on its own: 64+16 wins even when all 16 recoverable shards are gone. It needs the code to be large *and* the loss to be at the limit. At any erasure count a real lossy link produces, this crate wins by one to three orders of magnitude; at the correction limit of a 128+128 code it loses by 13x. Both are true and both are published.

### Initialization

| | rs-engine | `reed-solomon-simd` |
|---|---:|---:|
| first codec in a fresh process, 32+16 | **6.3** | 3770 |
| steady-state `new`, 4+2 | **0.049** | 0.119 |
| steady-state `new`, 64+16 | 11.5 | **0.842** |
| steady-state `new`, 128+128 | 102 | **1.51** |

The first row is what a short-lived process actually pays, and there the `const fn` tables deliver: **roughly 590x less than `reed-solomon-simd`**, which builds its GF(2^16) tables on first use. That row is a single untimed measurement and it is noisy: over eight fresh processes on an otherwise idle machine it ran 6.1 to 6.4 microseconds for this crate and 3.59 to 3.77 milliseconds for `reed-solomon-simd`, and on a machine still busy with the previous benchmark it read as high as 27. The order of magnitude is the claim, not the digits. Adding the SIMD shuffle tables did not change it, because they are `const fn` evaluated into `.rodata` like everything else: 8 KiB of binary, zero instructions at startup.

Building the generator matrix used to be the exception, and no longer is. It has a closed form (a Lagrange evaluation per entry), so it costs `O(data_shards^2)` field operations rather than the `O(data_shards^3)` of the Gauss-Jordan inversion it replaced. Across the range:

| `Codec::new` | 4+2 | 16+8 | 64+16 | 128+128 | 250+5 |
|---|---:|---:|---:|---:|---:|
| microseconds | 0.053 | 0.76 | 11.7 | 106 | 168 |

The largest cost anywhere in the supported range is 168 microseconds, at the widest split the field allows. At 64+16 with 1200-byte shards, building a codec costs about a third of a single encode. Callers who know their parameters at compile time can still avoid it entirely with `ParityMatrix`, which costs nothing at runtime at any size. The coefficients are unchanged: the closed form is checked byte for byte against the Gauss-Jordan reference for all 32,640 supported splits, and the known-answer vectors did not move.

### Against the project's performance targets

The project brief set five performance targets. Measured on aarch64 at 1200-byte shards:

| target | verdict |
|---|---|
| encode, small (<= 16+8): at least `reed-solomon-simd` | **met everywhere except 16+8.** Ahead at 8+1 (0.258 against 0.355), 4+2 (0.253 against 0.278), 4+4 (0.427 against 0.476) and 8+4 (0.769 against 0.877). Still missed at 16+8, but by 1.2x rather than the 1.7x before the blocked encode kernel. |
| decode, small (<= 16+8): 10x better than `reed-solomon-simd` | **met**, by 280x to 960x. |
| encode, large (>= 64 shards): within 2x of `reed-solomon-simd` | **partly**, where it was missed outright. Met at 64+16, now 1.8x behind rather than 2.7x. Missed at 64+64 (3.8x, from 5.6x) and at 128+128 (6.4x, from 9.2x). |
| decode, large (>= 64 shards): at least `reed-solomon-simd` | **partly.** Met by 61x to 96x at one erasure, and still met at 64+16 even at the correction limit (47.4 against 202). Missed at the correction limit of the largest codes: 64+64 losing 64 shards, 128+128 losing 128. |
| initialization: zero measurable cost | **met.** The field tables, the SIMD tables and `ParityMatrix` are `const fn` and cost nothing. `Codec::new` costs 0.053 microseconds at 4+2 and 168 at the widest split the field allows, and a whole first codec in a fresh process is 6.3. |

Every remaining shortfall is the `O(n^2)` engine rather than the kernel. Closing it needs an FFT engine, and [docs/DESIGN.md](docs/DESIGN.md) section 8b records what a study of that engine established: it would win encode from six parity shards up, lose decode by an order of magnitude at the erasure counts a lossy link produces, and emit different bytes, so it could only ever arrive as a separately selected format rather than an internal upgrade.

### Methodology

The harness is dependency-free (`std::time::Instant`, `harness = false`). Every fixture asserts its output is correct before being timed and checks every kernel against the scalar reference, so a benchmark cannot be fast by measuring the wrong work. Decode always loses *data* shards, so both crates do the same job. Two asymmetries are stated rather than hidden: `reed-solomon-simd` leaves recovered shards in internal buffers (one uncounted memcpy in its favour), and its tables are process-global, so its steady-state `new` is allocation only, which is why the first-use row exists.

Run-to-run spread on this machine is a few percent, so figures for the same quantity taken from different tables can disagree in the last digit; each table is internally consistent, having been produced by one run over the same inputs. The decode figures are unchanged from the previous release of this document, because the decode path is unchanged; they were re-measured and agreed within 4%.

Rows marked `~` above are read across from a table where `reed-solomon-simd` was measured at one erasure rather than at the correction limit. Its decode cost is flat in the erasure count, which is the entire point of the earlier table, so the substitution is sound, but it is a substitution and it is labelled as one.

The full sweep across 64 B to 64 KB shards, all erasure counts, both kernels and large parameter sets is in `cargo bench` and `cargo bench --bench comparison`. Nothing is omitted because it is unflattering.

---

## Testing

Erasure coding fails silently: a subtly wrong code path produces plausible-looking bytes that only fail when someone actually needs to recover data. The test effort is deliberately disproportionate to the code size.

- **Two independent constructions of the generator matrix**, asserted byte-identical for all 32,640 supported shard splits. The closed form that ships is `O(k^2)`; the textbook Gauss-Jordan construction is retained as a test-only reference. They share no code, so a wrong coefficient fails this, not merely a changed one.
- Exhaustive sweep over small parameters and every erasure pattern up to the correction limit.
- Property tests over random parameters, shard sizes and loss patterns.
- Known-answer vectors pinning the wire format.
- **Cross-kernel equivalence**: every SIMD kernel against the scalar reference, for all 256 field scalars, over shard lengths chosen to hit every vector width and every tail remainder, plus a canary check that no kernel writes past the end of its destination. The multi-row encode kernel gets its own sweep in the same shape, over parity counts that hit every group width it peels, with a canary on every output row. CI names the ISAs each runner is required to provide, so a runner that quietly loses AVX2 fails the build instead of quietly shrinking the sweep.
- Miri over the scalar path and all non-SIMD logic.
- A `no_std` no-allocator build for a bare-metal target.
- An empty-dependency-tree check, so the zero-dependency promise is enforced rather than asserted.

The scalar kernel in `src/kernel/scalar.rs` is the specification. It is written for obvious correctness, is never optimised, and is never deleted. The benchmark harness re-checks every kernel against it before it times anything, because a benchmark that measures a wrong-but-fast kernel is worse than no benchmark.

---

## Documentation

- [docs/WIRE_FORMAT.md](docs/WIRE_FORMAT.md), the normative byte-level specification.
- [docs/DESIGN.md](docs/DESIGN.md), architecture and the reasoning behind it.
- [CHANGELOG.md](CHANGELOG.md).
- [dev/ROADMAP.md](dev/ROADMAP.md), where this is going. Not a commitment.

---

## License

Licensed under either of [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option.

Unless you state otherwise, any contribution you intentionally submit for inclusion in this crate, as defined in the Apache-2.0 license, is dual licensed on those same terms, with no additional conditions.
