# Design

**Audience:** whoever maintains this crate next, including a future version of the person who wrote it.

This document explains why the code is shaped the way it is. For what the bytes on the wire mean, see [WIRE_FORMAT.md](WIRE_FORMAT.md). For what the crate does, see the [README](../README.md).

---

## 1. What this crate is for

Reed-Solomon erasure coding has two implementation families, and they sit on opposite sides of a performance cliff. FFT engines, of which `reed-solomon-simd` is the strong Rust example, are excellent at large shard counts; its decode path pays a fixed cost of roughly 170 microseconds regardless of parameters, because the Walsh-Hadamard transform it uses runs over a 65536-point domain whether you lost one shard or fifty. Matrix engines are the opposite: decode cost that scales with the erasure count, and `O(n^2)` asymptotics that lose badly once the shard count climbs.

So below roughly 32+16 you want matrix code and above it you want FFT code, and no crate gives you both. The goal here is to be the crate you can reach for without first benchmarking to find out whether your parameters land in its good region: a matrix engine whose small-parameter case is genuinely fast, with SIMD on by default and nothing to inherit in the dependency tree.

Two properties follow directly from that goal and shape everything below:

- **Zero runtime dependencies.** Not "few". Every dependency an erasure library carries is inherited by every consumer, forever, including its advisories and its MSRV.
- **Decode cost that scales with the number of erasures.** Losing two shards out of eighty should cost about what losing two shards costs, not what eighty shards cost.

---

## 2. The four layers

The target architecture is four layers, each independently testable:

```
┌─────────────────────────────────────────────────────┐
│  Codec API      Codec / ParityMatrix, shard access  │
├─────────────────────────────────────────────────────┤
│  Dispatch       parameter-driven engine selection   │
├─────────────────────────────────────────────────────┤
│  Engines        MatrixEngine (GF 2^8, O(n^2))       │
│                 FftEngine    (GF 2^16, O(n log n))  │
├─────────────────────────────────────────────────────┤
│  Field kernels  scalar / SSSE3 / AVX2 / GFNI /      │
│                 AVX-512 / NEON / WASM SIMD128       │
└─────────────────────────────────────────────────────┘
```

Where the code stands today (M3, matrix engine with SIMD kernels):

| layer | files | state |
|---|---|---|
| Codec API | `src/parity_matrix_ref.rs`, `src/parity_matrix.rs`, `src/owned.rs`, `src/driver.rs`, `src/shards.rs`, `src/mask.rs`, `src/error.rs` | complete |
| Dispatch | none | absent by design; there is one engine, so there is nothing to dispatch. Kernel selection is not this layer; it lives inside the kernel layer, below the engine. |
| Engines | `src/matrix.rs`, `src/driver.rs` | matrix engine complete, FFT engine not started |
| Field kernels | `src/gf.rs`, `src/kernel/` | scalar, NEON, SSSE3, AVX2. GFNI and WASM not started. |

The dispatch layer is deliberately not stubbed out. An abstraction with one implementation is a guess about the shape of the second implementation, and the second engine (section 7) may turn out not to exist at all. When there is a real second engine with real measured crossover points, the layer gets written against the facts.

### 2.1 The codec API layer

Three types hold a parity matrix, and they differ only in where the coefficients live:

| type | coefficients | allocator |
|---|---|---|
| `ParityMatrix<CAP>` | built by the compiler, inline, in `.rodata` | not needed |
| `ParityMatrixRef<'a>` | borrowed from wherever they came from | not needed |
| `Codec` | built at runtime into a `Box<[u8]>` | required |

**`ParityMatrixRef` is the real API.** It is a `Copy` view holding the coefficients together with the `(data_shards, parity_shards)` split they were built for. `ParityMatrix::as_ref` produces one, `Codec::as_matrix_ref` produces one, and every encode and decode in the crate goes through its four methods, so there is one checked path rather than three. `src/driver.rs` still holds the drivers, but they are `pub(crate)` now and take the split from the view rather than from the caller. Writing it in this order rather than the other way round is what keeps the no-allocator path a first-class citizen rather than an afterthought that quietly rots.

`Codec` (behind the `alloc` feature) exists purely so callers who have an allocator do not have to build the generator matrix into a buffer themselves. It allocates in its constructor and never again: `encode`, `encode_all`, `reconstruct` and `reconstruct_data` are allocation-free.

It is one type rather than a separate encoder and decoder, and every method takes `&self`. Both of those follow from the same observation: a generator matrix is immutable and shareable, and decode scratch is neither. 0.1.0 had an `Encoder` and a `Decoder` that held identical matrices and differed only in that the decoder also owned a workspace, which forced `reconstruct` to take `&mut self` even though it only ever read the matrix. That made a shared codec undecodable, so sharing one across threads needed a lock around an operation with no shared mutable state, and it charged every encode-only caller for scratch it never touched. Splitting the two lifetimes apart, with the workspace passed at the call site exactly as the no-allocator types already require, removes both costs and collapses the two types into one. The `alloc` layer now mirrors the layer below it instead of being the only one that could not do both directions from a single value.

Shard access goes through the private `ShardSource` trait, implemented once, blanket, for every `[E]` whose element is `AsRef<[u8]>`. It exists so the encode and decode drivers can be written once and reused over any slice of byte containers, and over a mutable shard slice with one element split out for writing. The blanket form is also what the public signatures are generic over, so `&[&[u8]]`, `&[Vec<u8>]`, `&[[u8; N]]` and `&[Box<[u8]>]` all reach the kernels with no adapter slice in between; a signature fixed to `&[&[u8]]` makes a caller whose shards live in a `Vec<Vec<u8>>` allocate a throwaway pointer slice on every call. It is monomorphised at every call site and costs nothing at runtime. The alternative was three near-copies of the driver loop, which is exactly the kind of duplication that lets a bug live in one copy for years.

The trait lives in `src/shards.rs` rather than beside the drivers in `src/driver.rs`, and that is a layering constraint rather than a filing preference. The kernel is the bottom layer, and `Kernel::mul_rows` (section 2.3) reads `data_shards` sources at once, so it needs the trait. Defining it in `codec.rs` would leave the bottom layer importing from the top one. A neutral module both layers depend on keeps the direction honest. `SplitShards`, the one-element-removed view, stays in `codec.rs` and implements the trait from there, because it is a codec concern that no kernel names.

`ShardMask` is a fixed 256-bit `Copy` bitmap rather than a `Vec<bool>` or a `Vec<Option<Vec<u8>>>`. `MAX_SHARDS` is 256, so the bitmap is 32 bytes, which is cheaper to pass by value than a pointer plus a length is to chase.

#### Why the split is part of the type

The no-allocator surface used to be four free functions, `encode_with`, `encode_all_with`, `reconstruct_with` and `reconstruct_data_with`, each taking a bare `&[u8]` of coefficients plus the shard counts, restated at every call. They are gone, and this is the reasoning, because it applies to any future API that is tempted to take coefficients as loose bytes.

A coefficient slice cannot say what shape it is. A 4+2 matrix and a 2+4 matrix are both eight bytes, and nothing in those bytes distinguishes them, so a `data_shards` argument is an assertion no amount of validation can check. It is also an assertion the old API made the caller repeat: the README example restated the same 4+2 split three times, once as a bare integer literal next to a matrix that already knew the answer.

What that costs was measured rather than assumed. Hand the coefficients of a 4+2 code to the decode path with `data_shards = 2`, which is exactly what a transposed or mistyped argument did, and run every erasure pattern the honest code can correct: all 21 return `Ok`, and all 21 return corrupted shards. Not one case in twenty-one produces an error, a panic, or any other signal. The corruption surfaces at the receiver, under loss, in production, which is the failure mode this crate is built to avoid.

Carrying the split does not delete the hazard, because the constructor has no structural evidence beyond the coefficient count: `ParityMatrixRef::new(2, 4, ..)` on 4+2 coefficients still succeeds. What it does is move the assertion to the one place where the answer is actually known, next to the bytes as they are obtained, and make it once. Get it right there and no later call site can get it wrong. Two tests in `src/parity_matrix_ref.rs` pin both halves of that: `a_transposed_split_is_rejected_wherever_the_shard_counts_can_see_it` for what the type does catch, and `a_transposed_split_still_corrupts_the_combined_form_silently` for the residue it cannot.

The change also cost nothing in defensive code. It deleted more checking than it added: 21 error-returning checks in `src/` before, 18 after, while gaining a public constructor. The owned types used to carry their own shard-count validation that had to agree with the free functions' copy of it, and that duplicate is gone; the view validates the split once in `new`, through the same `check_counts` and `check_matrix` every other entry point uses, and each operation then compares shard counts against a number the type owns rather than one it was handed.

### 2.2 The engine layer

`src/matrix.rs` builds the generator matrix and does the linear algebra. Everything in it is a `const fn`. That is not cleverness for its own sake: it means the same code builds a matrix at compile time for `no_std` callers through `ParityMatrix::new` and at runtime for `alloc` callers through `Codec::new`. There is one implementation, not two that can drift apart, and the compile-time path is validated by the same tests as the runtime one.

Matrices are flat row-major `&[u8]` with an explicit column count, not a nested structure. At these sizes indexing arithmetic is free and a flat buffer is the only shape that works in a `const fn` without an allocator.

The generator matrix is evaluated in closed form rather than by inverting anything, which is what makes `Codec::new` quadratic and lets `ParityMatrix::new` reach the largest split the field allows. Section 8a records why, and WIRE_FORMAT.md section 4.1 gives the mathematics.

### 2.3 The field kernel layer

`src/gf.rs` holds the field. Its log and exp tables are built by a `const fn` loop, so they land in `.rodata`, cost nothing to initialise, need no `OnceLock` and no lock, and are visible to the optimiser. This is a real, demonstrable advantage over the alternatives: `reed-solomon-simd` pays several milliseconds on first use building its GF(2^16) tables.

`src/kernel/` holds the three functions that all the time goes into:

```rust
fn mul_slice(c: u8, src: &[u8], dst: &mut [u8]);      // dst  = c * src
fn mul_add_slice(c: u8, src: &[u8], dst: &mut [u8]);  // dst ^= c * src
fn mul_rows(matrix: &[u8], k: usize, srcs: &S, outs: &mut [&mut [u8]]);
// outs[t] = XOR over j < k of matrix[t * k + j] * srcs.shard(j)
```

Everything else in the crate is bookkeeping around these. `src/kernel/scalar.rs` is the reference; see section 6 for why it must never be optimised. `src/kernel/mod.rs` holds the public `Kernel` type and dispatches to the vector implementations in `neon.rs` and `x86.rs`; `src/kernel/tables.rs` holds the split-nibble tables they index. Section 7 covers how they work.

The third operation is a fusion of the first two, not a new one. It is exactly the loop `src/driver.rs` used to run itself for encode, one `mul_slice` for the first term of each output row followed by `k - 1` `mul_add_slice` calls, handed to the kernel whole rather than driven from above. Nothing else about the operation changed: it still assigns rather than accumulates, so the first term needs no zeroed output.

**Why handing over the whole problem is worth a third entry point.** Encode drives `parity_shards` output rows over the same `data_shards` sources. Row by row, each source byte's two nibble vectors are re-derived once per output row, and every output shard is walked once per source, so the traffic is `data_shards * parity_shards` shard-lengths rather than `data_shards + parity_shards`. An implementation that can see all the rows at once holds `G` rows' accumulators in registers, splits each source byte's nibbles once for all of them, and writes each output exactly once. Where the compiler has `eor3` (FEAT_SHA3, which `aarch64-apple-darwin` enables through its default target CPU) the accounting is `4 + 6G` SIMD operations per 32 bytes per data shard against `10G` for the row-major loop; without it, `4 + 8G` against `12G`, so the shape of the argument does not rest on it. The shuffle count is identical either way. Blocking removes the work around the shuffles, not the shuffles.

Note what that accounting does and does not say. The ratio `10G / (4 + 6G)` depends on `G` alone: the per-source cost appears on both sides, so more data shards multiply both equally and do not change the win. What it also leaves out is the fixed cost of setting a group up, which does not shrink with `G` and therefore dominates when `G` is small. Below `G = 4` the setup costs more than the blocking saves, which is why the peel stops at four and the leftover rows are computed row-major. Section 7 has the measurements, including the ones that overturned an earlier and more optimistic reading of this same formula.

**Only NEON blocks.** aarch64 has 32 vector registers, which is what lets `G = 8`, sixteen accumulators plus the source and table vectors, stay resident without spilling; the shipped `G = 8` body disassembles to exactly the ideal `4 + 6G` with no stack traffic. SSE and AVX2 have 16 registers, and below AVX-512's `vpternlogd` x86 has no three-input XOR, so neither half of the accounting carries over and the group width that pays would have to be found rather than assumed. The x86 `mul_rows` is therefore a plain row-major driver over that module's own `mul_slice` and `mul_add_slice`, byte-identical to the old codec loop by construction, and the head of `src/kernel/x86.rs` says so. A blocked x86 path may well be worth writing; it is not worth writing blind, and no x86 measurement of `mul_rows` exists yet in either direction.

**The scalar path deliberately does not block**, and that is a measured decision rather than an omission: the same blocking is a loss on the scalar path at every configuration tried, between 0.62x and 1.00x, never a win. So `scalar::mul_rows` is a driver over the reference pair, which are untouched. That is worth more than the speed it declines to chase. It keeps the reference exactly what section 6 says it is, and it makes the differential oracle for the blocked NEON path exact rather than approximate: the thing the vector path is compared against is the row-major loop itself, not a second clever implementation.

Measured end to end through `Codec::encode_all` on an Apple M1 Pro at 1024-byte shards, blocked against row-major: 1.19x at 4+4, 1.32x at 8+4, 1.43x at 16+8 and 1.47x at 64+16. Splits with fewer than four parity shards take the row-major path and are unchanged, within noise, from before this operation existed: 1.00x at 8+1 and at 4+2. Decode does not use `mul_rows` at all. At one or two erasures there are one or two output rows and nothing to block over, so `reconstruct_impl` still drives `combine` per row.

`Kernel` is a newtype around a private enum, and that is load-bearing rather than stylistic. The SIMD implementations are `unsafe fn`s that require their ISA to be present. If a caller could name a variant directly, they could hand `Codec::with_kernel` an AVX2 kernel on a machine without AVX2 through an entirely safe API, and the crate would be unsound. Every path that produces a `Kernel` (`SCALAR`, `detect`, `supported`) returns only kernels that `is_supported` accepts, and there is no other constructor. Do not add one, and do not make the enum public.

---

## 3. Why decode solves an `e x e` system

This is the single most important decision in the crate and the one most likely to be "simplified" back into a mistake.

The textbook decode, and the one most matrix implementations use, is: take the `n x k` generator matrix `G`, select the `k` rows corresponding to surviving shards, invert that `k x k` matrix, and multiply. It works, and it costs `O(k^3)` for the inversion plus `O(k^2)` shard operations to apply. At 64+16 with one lost shard you invert a 64x64 matrix to recover one shard.

The observation this crate uses instead: the surviving *data* shards are already known. They are not unknowns. Let `M` be the set of missing data shards and `e = |M|`. Pick any `e` surviving parity shards. Each one gives one linear equation whose only unknowns are the `e` missing data shards, with everything else moved to the right-hand side:

```
XOR over u in M of ( P[r_t][u] * d_u )  =  p_{r_t}  XOR  ( XOR over present data i of P[r_t][i] * d_i )
```

That is an `e x e` system. `reconstruct_impl` in `src/driver.rs` builds it as an `e x (e + k)` augmented matrix `[A | C]`, where `C` expresses the right-hand side as a combination of the `k` surviving shards, and Gauss-Jordans it. Reducing `A` to the identity turns `C` into `A^-1 C`, which is the recovery matrix directly: each missing data shard comes out as a linear combination of surviving shards only, so nothing has to be staged through scratch buffers.

`A` is a square submatrix of `P` and the code is MDS, so `A` is non-singular. Whenever at least `k` shards are present, at least `e` surviving parity shards exist, so the system can always be formed.

The cost is `Theta(e^2 * (e + k))` for the elimination and `O(e * k)` shard operations to apply. Recovering two shards out of eighty solves a 2x2 system.

The elimination exponent is worth stating exactly, because `e^3` is the intuitive answer and it is wrong. `gauss_jordan` clears each of `e` pivot columns across all `e` rows, and each of those row operations runs the full width of the augmented matrix, which is `e + k` columns, not `e`: the `C` half is where the answer accumulates and it cannot be skipped. So the field-operation count is `e^2 * (e + k)`, and for `e` well under `k` it is closer to quadratic-times-`k` than to cubic.

Three consequences worth spelling out, because they are easy to lose:

- **Never replace this with a `k x k` inversion**, however much simpler it looks. The asymmetry between `e` and `k` is the whole point.
- **The inverted-matrix cache is largely unnecessary.** The project brief proposed a fixed-size direct-mapped cache keyed by erasure pattern, mirroring the LRU cache matrix implementations conventionally keep. With `e` typically 1 or 2 there is almost nothing left to cache: at `e = 1` the elimination is `1 * (1 + k)` field operations, under 130 even at `k = 128`, against `O(e * k * L)` bytes of shard work. If a cache is ever added, it must be justified by a benchmark showing the elimination is measurable, not by analogy with anyone else's design.
- **`reconstruct_data` is a genuine saving, not a micro-optimisation.** Regenerating a missing parity shard costs `k` full shard operations. A receiver that is about to discard the parity shards should not pay that.

The cache conclusion above is about the erasure counts a real link produces, and the measurements back it there. It is not a claim that the elimination is always small, and near the correction limit it stops being. Measured at 128+128 with 1200-byte shards on an Apple M1 Pro, `reconstruct` costs 4.6 microseconds at one erasure and 3,727 at 128. Shard work is linear in `e`, so it accounts for about `128 * 4.6 = 590` of those microseconds and the elimination for the remaining 3,140, which is 84% of the whole decode. The model is worth trusting on the point: subtract the shard term at `e` = 32, 64 and 128 and what is left is 118, 586 and 3,138 microseconds, a ratio of 1 : 5.0 : 26.7 against the 1 : 4.8 : 25.6 that `e^2 * (e + k)` predicts. That is the one regime where a cache would have something to bite on, and it is also the regime where the matrix engine is the wrong tool anyway; see section 8b.

---

## 4. The zero-dependency policy

`[dependencies]` in `Cargo.toml` is empty and must stay empty. Dev-dependencies are fine and encouraged.

This is not minimalism for its own sake. An erasure codec is a leaf utility: a few thousand lines of field arithmetic that every consumer links and few consumers ever look inside. When such a crate carries frozen dependency pins, its whole transitive tree becomes something downstream inherits and cannot fix, RUSTSEC advisories included. That is the failure mode being avoided, and it is avoided by having nothing to inherit.

Every dependency a codec of this kind conventionally carries has a `std` or const-eval replacement:

| what it is conventionally used for | replacement | status here |
|---|---|---|
| `cpufeatures`, CPU detection | `std::arch::is_x86_feature_detected!` | done, `src/kernel/x86.rs`, behind the `std` feature. aarch64 needs no detection at all: NEON is in the baseline, so `cfg!(target_feature = "neon")` answers it at compile time and `is_aarch64_feature_detected!` is never called. |
| `once_cell` / `lazy_static`, table init | `const fn` tables | done, `src/gf.rs` |
| `lru`, inverted matrix cache | fixed-size direct-mapped array, no allocation | not needed; see section 3 |
| `parking_lot` / `spin`, locking | immutable tables need no locks at all | done; there is no shared mutable state |
| `fixedbitset`, shard presence | `[u64; 4]` bitmask | done, `src/mask.rs` |
| `libm`, `no_std` math | not needed; this is integer-only work | n/a |

Under `no_std` there is no runtime CPU detection, so kernel selection falls back to compile-time `#[cfg(target_feature = ...)]`. On aarch64 this costs nothing, because NEON is in the baseline and is selected either way. On x86 it means a `no_std` consumer gets the scalar kernel unless they build with `-C target-feature=+avx2` or `+ssse3`, or a `-C target-cpu` that implies one. This is documented in the README, in the `Kernel::detect` rustdoc and in the crate-level docs, because it is the one place where a `no_std` build is quietly slower than a `std` build of the same code on the same machine.

### 4.1 Features

`default = ["std"]`, `std = ["alloc"]`, `alloc = []`. With no features the crate is `no_std` and allocation-free and the API is `ParityMatrix`, `ParityMatrixRef`, `Kernel`, `ShardMask` and `workspace_len`. `std` adds runtime CPU feature detection on x86, and nothing else beyond `alloc`.

`workspace_len` is the one operation that stays a free function as well as a method, and the reason is in `src/driver.rs`: a decode workspace has to be reserved before there is a matrix to ask, which is exactly the position firmware is in when the split arrives at runtime.

`ci/no-std-check/` is a standalone crate, built for `thumbv7em-none-eabi` in CI, that exercises `ParityMatrix` for a compiler-built matrix, `ParityMatrixRef` for coefficients loaded as bytes, and a split that is not known until the entry point is called, all with no allocator. It is a `staticlib` on purpose: that forces a real codegen and archive step, so a stray `alloc` path fails the build rather than slipping through a metadata-only check.

---

## 5. Error handling

`Error` is a small non-exhaustive `Copy` enum with no allocation, and it implements `core::error::Error`. The public entry points never panic and never allocate.

The one place panics are used deliberately is `ParityMatrix::new` and the `const fn` helpers it calls. In a `const` context a panic is a compile error, which is exactly the right behaviour for "you asked for 300 shards". At runtime `ParityMatrix::new` would panic instead of returning an error, which is why its documentation points runtime callers at `Codec::new`.

`Error::SingularMatrix` is unreachable through any codec built by this crate: the MDS property rules it out. It exists because `ParityMatrixRef` borrows coefficients this crate did not build, and reporting the condition beats a panic or a silently wrong answer.

---

## 6. The scalar kernel is the specification

`src/kernel/scalar.rs` carries this comment, and it is load-bearing:

> This is the scalar reference implementation. It is written for obvious correctness and is never optimized: it is the specification that every future SIMD path must reproduce byte for byte.

Erasure coding fails silently. A subtly wrong SIMD path produces plausible-looking bytes that only fail when someone actually needs to recover data: under loss, in production, unreproducibly. The defence is a reference implementation simple enough that its correctness is apparent by reading it, plus tests that assert every other path is byte-identical to it.

That defence evaporates the moment someone optimises the reference. If the scalar kernel grows a word-at-a-time trick or a table layout chosen for cache behaviour, it stops being obviously correct, and then the differential tests are comparing two things neither of which is trusted.

So, concretely, for anyone editing `src/kernel/scalar.rs`:

- The `c == 0` and `c == 1` branches are shorthand for the general loop, not approximations of it, and the tests assert that. Adding a similar branch is fine only if it is provably identical.
- Anything that makes the loop faster and less obvious does not belong here. It belongs in a new kernel module with a differential test against this one. Three such kernels now exist, in two modules, and this is exactly how they were added.
- This file is never deleted, not even when every target has a SIMD path. It is the definition of correctness, the fallback for targets nobody thought about, and what `Kernel::SCALAR` exposes so a caller can pin it.

The same discipline applies to `src/gf.rs`: `mul` is table-driven, but the tests check it against a table-free carry-less-multiply reference for all 65536 input pairs. The split-nibble tables in `src/kernel/tables.rs` are derived from `gf::mul` in a `const fn`, so they inherit that check rather than restating it.

### 6.1 The `unsafe` policy

The crate root is `#![deny(unsafe_code)]` and `#![forbid(unsafe_op_in_unsafe_fn)]`. There are exactly five opt-outs, and there should never be a sixth without a paragraph here explaining it:

- `src/kernel/neon.rs` and `src/kernel/x86.rs`, each with a module-level `#![allow(unsafe_code)]`. Intrinsics are the only reason.
- The three dispatch methods `Kernel::mul_slice`, `Kernel::mul_add_slice` and `Kernel::mul_rows`, each with an item-level `#[allow(unsafe_code)]`, purely to enter the above.

**The fifth opt-out, `Kernel::mul_rows`.** The count went from four to five; the obligation did not grow. `mul_rows` enters the same two modules for the same reason and rests on exactly the argument the two dispatch methods beside it rest on, which is why its `// SAFETY:` comments say "as above".

What is new is that the NEON side of it is a different loop rather than a third spelling of the same one, so it is worth saying what it does *not* introduce. It adds no new kind of `unsafe`. Every index into the coefficient rows, the source windows and the outputs is a bounds-checked slice operation; the only `unsafe` blocks are the intrinsic calls, each carrying the same "this function requires `neon`, this window is 32 bytes long" justification the older loops carry. In particular it carries no pairwise-disjointness argument for the `G` outputs it writes at once, because it needs none: the outputs arrive as `&mut [&mut [u8]]` and are peeled off with `split_at_mut`, so the borrow checker has already proved they do not overlap. `take_group` is the one piece of borrow-checker cleverness in the change and it is entirely safe code, which Miri accepts under both Stacked Borrows and Tree Borrows.

That was a deliberate choice with a measured price. A raw-pointer version of the same loop is 1% to 3% faster, and the bounds checks cost no registers: the shipped `G = 8` body still holds all sixteen accumulators in registers with no stack traffic. Buying back one to three percent by hand-proving what the compiler proves for free is not a trade this crate makes. Note also which indexing is cheap and which is not: slicing the `G` coefficient rows once per group is free, while indexing the matrix per element inside the inner loop was not, and an earlier attempt that did the latter is where the belief that bounds checks are expensive here came from.

Every `unsafe` block names, in a `// SAFETY:` comment, the feature check that makes it sound, and every such comment ultimately points at the same argument: the `Isa` enum is private, so a `Kernel` naming an ISA can only have come from `detect` or `supported`, both of which filter on `is_supported`. Section 2.3 explains why that is the whole safety story.

`forbid` rather than `deny` on `unsafe_op_in_unsafe_fn` is deliberate: it means a target-feature function cannot quietly become a place where unsafe operations need no justification. Note that MSRV 1.85 still predates safe `#[target_feature]` functions, which stabilised in 1.86, so the vector entry points are `unsafe fn` and must stay that way until the MSRV moves again.

---

## 7. Trajectory: what M3 built, and M4 through M7

M1 (correct scalar core), M2 (benchmark harness and known-answer vectors) and M3 (split-table SIMD) are done. What M3 turned out to be, and what comes next:

**M3, split-table SIMD. Done.** For a fixed scalar `c`, `lo[i] = mul(c, i)` and `hi[i] = mul(c, i << 4)` for `i` in `0..16` give `mul(c, b) = lo[b & 0xF] ^ hi[b >> 4]`, and each lookup is a single 16-entry byte shuffle.

What was built:

- `src/kernel/tables.rs`: all 256 scalars tabulated up front as one 32-byte entry each, `const fn` evaluated, `#[repr(align(64))]` so an entry never straddles a cache line and each 16-byte half is naturally aligned for a vector load. 8 KiB of `.rodata`. Tabulating every scalar rather than computing two tables per call was the right trade at 8 KiB; it keeps the hot path free of any per-call setup beyond one aligned load.
- `src/kernel/neon.rs`: `vqtbl1q_u8`, 16 bytes per shuffle.
- `src/kernel/x86.rs`: `pshufb` at 16 bytes and `vpshufb` at 32, written as the same algorithm at two widths in one file so they cannot drift. The AVX2 table broadcast is the only step that is not a mechanical widening of the SSSE3 code, and it is commented as such.
- `Kernel` in `src/kernel/mod.rs`: a public, `Copy`, enum-dispatched selector, resolved once per codec and stored in it, never re-detected per shard. `Codec::with_kernel` and `Codec::with_kernel` let a caller pin one; `Kernel::supported` enumerates what is runnable here, which is what the benchmark harness sweeps.

Measured on an Apple M1 Pro at 1200-byte shards, NEON is now worth 19x to 33x on encode depending on the split and 14x to 20x on decode. Encode was in the same 20x band as decode when M3 landed; the blocked kernel below is what moved it. The speedup is much lower on 64-byte shards, where fixed per-call cost dominates, and the benchmark reports per shard size rather than averaging so that this stays visible.

*What changed:* the kernel layer gained modules; the crate gained its first `unsafe`, scoped as described in section 6.1; `std` stopped being a feature that does nothing.
*What deliberately did not change:* the wire format, the public API's existing surface, the dependency count (still zero; detection is `std::arch::is_x86_feature_detected!`), the `O(n^2)` engine, and the scalar kernel, which is untouched and is still the specification.

Two notes on the shape it took. Dispatch is an enum `match` rather than a function pointer, which inlines and which keeps `Kernel` constructible only through checked paths. And `vqtbl4q_u8` on aarch64 (open question 5) is still unexplored: the 4-bit split already reaches roughly 22x on decode at large shard sizes, so the remaining headroom is small.

**After M3, the blocked encode kernel.** The one thing M3 left on the table was that the codec drove the kernel one output row at a time. `Kernel::mul_rows` and the blocked NEON implementation of it (section 2.3) close that, and encode is 1.19x to 1.47x faster from four parity shards up. Nothing else moved: the wire format, the public API, the dependency count, the scalar kernel and the whole decode path are untouched, and the known-answer vectors regenerate byte-identical. The `ShardSource` trait moved to `src/shards.rs` so the kernel layer could name it without importing from the codec.

**How the group widths were chosen, and the reasoning that had to be thrown out.** The first version peeled groups of 8, 4, 2 and 1, on the argument that a parity count which is not a multiple of eight should still get the benefit on its leftover rows. That was wrong, and wrong in the direction that matters: it made encode *slower* than the row-major loop it replaced at exactly the splits with the least headroom. A `G = 1` group cost 50% at 8+1, and a `G = 2` group cost 8% at 4+2; 4+3, which pulls in one of each, cost 25%.

The cause is that a group's fixed cost does not shrink with `G`, so at `G = 1` it is paid in full against a saving of zero: the `4 + 6G` against `10G` accounting in section 2.3 makes `G = 1` a wash *in the loop body* and says nothing about the setup around it.

An earlier draft of this section defended the narrow groups by predicting that they would be most profitable where sources are many and parity rows few, at splits like 64+2 and 128+3. That prediction came from the memory-traffic hypothesis, and it is false. The saving and the row-major cost both scale with the source count, so their ratio is fixed by `G` alone and extra sources change nothing. Measured at 1024-byte shards, blocked against row-major: 64+1 0.68x, 128+1 0.68x, 32+2 1.04x, 64+2 1.06x, 128+2 1.01x, 250+2 0.99x, 128+3 0.89x. Both named counterexamples came out against the prediction, 64+2 by returning 6% rather than a lot and 128+3 by losing 11%.

So the rule is to peel at 8 and 4 and compute the one to three leftover rows row-major. That gives up 6% at 64+2, the most a `G = 2` group ever returns, and in exchange no split anywhere is slower than before the kernel existed. Recovering the 6% would need a threshold on `data_shards` as well as `parity_shards`, applying at `parity_shards == 2` but not at 1 or 3: two interacting thresholds and a third code path, for 6% at one split no known consumer runs. Not taken.

**What this cost to find.** The benchmark sweep started at 4+2 and climbed, so it contained no split with one parity shard and could not see a 50% regression at 8+1. The driving consumer's own documentation recommends 8+1 for bandwidth-constrained links. `benches/bench.rs` and `benches/comparison.rs` now both begin at 8+1 and include 4+4, so the splits that consumer actually runs are the splits a change has to survive.

**M4, GFNI.** `vgf2p8affineqb` computes an arbitrary F2-affine transform on each byte with the matrix packed into a `u64`. Multiplication by a constant in GF(2^8) is F2-linear, so it is exactly such a matrix, *for any reduction polynomial*. GFNI's native `vgf2p8mulb` is hardwired to the AES polynomial `0x11B`, but `vgf2p8affineqb` is not, so `0x11D` is usable. AVX-512 plus GFNI gives 64 bytes per instruction; the VEX-encoded AVX2 plus GFNI path gives 32 and covers consumer Alder Lake and later, which have GFNI but no AVX-512. Do not skip the AVX2 variant.
*Changes:* more kernel modules.
*Does not change:* anything else.

**M5, GF(2^8) FFT engine.** The additive FFT over a Cantor basis from Lin-Chung-Han (FOCS 2014). The important variant is the **8-bit** one, not the 16-bit one every existing Rust crate implements. The GF(2^16) variant needs a Walsh-Hadamard transform over a 65536-point domain to compute the error locator, which is where the 170 microsecond decode floor comes from; the GF(2^8) variant needs only a 256-point transform, roughly 256x cheaper, and still covers the entire 256-shard range. `klauspost/reedsolomon` ships exactly this split; no Rust crate does.

*Changes:* a real engine layer and a real dispatch layer, with crossover thresholds measured per architecture rather than guessed, plus a caller-visible escape hatch to force an engine.
*Constraint that cannot be negotiated:* if both engines can serve the same parameters, they **must** produce identical parity bytes. Otherwise the dispatch threshold becomes part of the wire format, and a sender and receiver that disagree about it silently fail to reconstruct. See WIRE_FORMAT.md section 10. If byte-identical output is not achievable, engine selection must become an explicit, caller-visible, serialized parameter. Section 8b establishes that it is **not** achievable: the additive FFT's generator is a Cauchy code and the systematic Vandermonde this crate ships is not it. So the second branch is the live one, and an M5 engine arrives as a selected format or not at all.

Derive the field polynomial, basis constants and transform structure from the paper and validate against a reference implementation. Get a scalar GF(2^8) FFT passing differential tests against the matrix engine before writing a single intrinsic.

**M6, polish and publish.** Docs, `no_std` verification, MSRV job, WASM path, semver commitment. Partly done: this document, WIRE_FORMAT.md, `ci/no-std-check/` and the CI matrix exist.

**M7 (optional), GF(2^16) FFT** for more than 256 shards, only if a real consumer needs it. This is the one stage that would change the wire format, because it is a different field and a different code. It has to arrive as an explicitly selected, separately versioned format, never as an internal upgrade.

---

## 8. Open questions

These are unresolved. Do not treat any of them as settled by the current code.

1. **Vandermonde versus Cauchy** is decided, and the decision is Vandermonde-systematic. Cauchy is systematic by construction and its submatrices are invertible without a reduction step, which is cleaner to reason about. Vandermonde buys byte-compatibility with the de facto GF(2^8) interchange format, which lets a consumer migrate, or interoperate across languages, without a protocol version bump. Migration path won. This is the one item on the list that is closed, and reopening it is a semver-major change.

   The closed-form construction (section 8a) showed the question was a false dichotomy: they were never rival constructions here. `P[j][c] = a_j * b_c / (y_j ^ c)`, so the systematic Vandermonde parity matrix *is* a generalized Cauchy matrix, `diag(a) * [1 / (y_j ^ c)] * diag(b)`, with `a_j` and `b_c` non-zero. The reduction step is what turns one into the other. So the "cleaner to reason about" half of the argument for Cauchy was already available: every square submatrix of `P` is non-singular because every square submatrix of a Cauchy matrix on distinct nodes is, and non-zero diagonal scaling cannot change that. That is the MDS proof in one line, and WIRE_FORMAT.md section 4.1 now states it. Nothing about the bytes changed; only the argument about them got shorter.

2. **Does the GF(2^8) FFT engine dominate the matrix engine outright?** Answered, and the answer is **no**. It wins encode from parity 6 up and loses decode by an order of magnitude at the erasure counts that actually occur, so the dispatch layer survives and the two engines cannot even emit the same bytes. Section 8b records the evidence and the one residual that is still open. This item is closed; what replaces it is a question about consumers, not about the codes.

3. **Is `reed-solomon-simd` wire-compatible with anything worth matching?** If a GF(2^16) engine is eventually built, compatibility with the existing ecosystem may be worth more than a marginal speed edge. Unanswered, and only relevant at M7.

4. **Const-eval budget for GF(2^16) tables.** Those tables run to hundreds of kilobytes. Const-evaluating them bloats the binary; computing them lazily reintroduces the initialisation cost that is currently a differentiator. Measure both and pick per-table. This tension is another reason to prioritise the GF(2^8) FFT engine.

5. **`vqtbl4q_u8` on aarch64** gives a 64-entry lookup rather than 16. Is there a formulation that beats the 4-bit split there? Still unanswered after M3, and now lower value: the 4-bit split reaches about 22x over scalar at 16 KB and above, so whatever is left is a fraction of a fraction. If anyone revisits it, the case to beat is the 64-byte shard column, where the kernel is call-bound rather than throughput-bound and a wider lookup would not help either.

6. **SVE and SVE2** on server ARM: worth a path, or too fragmented to justify the maintenance? Unanswered.

---

## 8a. Closed: the generator matrix has a closed form

This section used to record a known gap: building the generator matrix inverted the top `k x k` block of the Vandermonde matrix by Gauss-Jordan elimination, which is `O(k^3)`, and M3 had made it the crate's largest un-addressed cost by making everything around it about twenty times faster. It is fixed. What follows is the record of what was done and why, kept because the next person to look at `src/matrix.rs` needs the reasoning, not because anything is outstanding.

### What ships now

`build_parity_matrix` evaluates each entry directly:

```text
P[j][c] = master(y_j) / (y_j ^ c) / denominator(c)
master(y)      = prod over m < k of (y ^ m)
denominator(c) = prod over m < k, m != c of (c ^ m)
```

with `y_j = k + j`. That is `P[j][c] = L_c(y_j)`, the Lagrange basis polynomial for node `x_c = c` evaluated at the parity node `y_j`, which is what `(V * T^-1)[k + j][c]` unfolds to. The cost is `O(k^2)` for the denominators, once, plus `O(1)` per entry after an `O(k)` `master` per parity row.

Measured on an Apple M1 Pro: `Codec::new` at 128+128 went from 4325 microseconds to 102, and at 250 data shards from 19810 to 164. The full range is in the README.

### Why the framing this section used to propose was a detour

The old text proposed recovering `T^-1` through elementary symmetric polynomials. That works, but it solves a harder problem than the one in front of it. **`T^-1` is never needed. Only `V * T^-1` is**, and that product is one Lagrange evaluation per entry, with no polynomial coefficients ever materialised. Chasing the inverse itself would have meant computing and storing `k^2` coefficients, in a `const fn`, in a scratch buffer sized at compile time, to then throw all of them away.

Skipping it pays twice. The build is `O(k^2)` instead of `O(k^3)`, and the apply step disappears: multiplying the bottom `p` rows of `V` through `T^-1` is `O(p * k^2)`, whereas evaluating `L_c(y_j)` directly is `O(p * k)`. The old implementation paid both.

It also removed the scratch buffer, which is why `MAX_CONST_DATA_SHARDS` rose from 128 to 255. The old const constructor had to size the `[T | I]` working array at compile time, `2 * k * k` bytes, so it had to pick a `k` and put 32 KiB on the stack for every caller regardless of the split they asked for. What is left is `MAX_SHARDS` bytes of reciprocal denominators, and the bound it forced is gone: `MAX_CONST_DATA_SHARDS` is now simply `MAX_SHARDS - 1`, which is every value the crate can express. `ParityMatrix::new` stays inside the compiler's const-eval step budget at every split the field admits, about 0.6 seconds at 255 data shards, with no `#![allow(long_running_const_eval)]` needed anywhere. It used to exceed the budget past roughly 83 data shards.

### Why this is safe to have done

The rule from section 6, that the obvious implementation stays as the specification, was followed exactly as M3 followed it for the kernels: the fast path went somewhere new, the Gauss-Jordan version stayed as `build_parity_matrix_by_inversion` behind `#[cfg(test)]`, and a test asserts they agree.

The output is not an approximation of the inversion, it is the same matrix. `T^-1` is unique, so anything computing `V * T^-1` exactly agrees byte for byte. That is proved, not sampled: `closed_form_matches_the_reference_at_every_shard_count` covers all 32,640 supported splits by exploiting the fact that both builders produce row `j` from `k + j` alone, so the widest split at each `k` contains every row any narrower split can ask for. An `#[ignore]`d test compares all 32,640 outright. The known-answer vectors regenerate unchanged.

### What remains

Nothing here. The zero-initialisation-cost target is met across the whole range, and the remaining performance gaps are the `O(n^2)` engine, which is M5's problem.

---

## 8b. Closed: the FFT engine does not dominate

Open question 2 asked whether a GF(2^8) additive-FFT engine dominates the matrix engine outright, because if it did, the dispatch layer and half the crate could go. It does not. This section is the record, so that nobody re-opens it on the strength of the encode half alone.

**What the answer rests on.** A scalar Lin-Chung-Han additive FFT over a Cantor basis, verified three ways: against an independent `O(n^2)` oracle at 2,040 cases, by exhaustive MDS brute force at small splits, and by an analytic proof that the generator is Cauchy. The comparison itself counts **shard operations**, not wall-clock time. No SIMD FFT exists to time, and a count is the honest thing to compare when one side would otherwise be measured with a kernel the other side does not have. Everything below is therefore a statement about how much work each engine asks for, not about how fast either runs today.

That prototype is in the repository, as `examples/fft_proto.rs`, and it is the only thing here that can re-derive any of this. It is not compiled into the library and borrows only `gf`. Run it three ways:

```text
cargo run --release --example fft_proto            # encode analysis
cargo run --release --example fft_proto decode     # decode op counts
cargo run --release --example fft_proto decode2    # decode, cost-weighted
```

Every run re-checks itself before reporting: the truncated transforms against the untruncated ones, exhaustive erasure round-trips, and the analytic generator against what the encoder actually computes. CI runs the first two on every push, so this section cannot rot into an assertion without the build going red. **Do not refactor that file for style**; the reason to keep it is that it is checked, and edits spend that.

Two of the numbers below are cost-weighted rather than pure counts, and those drift a few percent between runs because the weighting is measured live on the machine doing the running. Where that matters it is said so.

### Encode favours the FFT, from parity 6 up

Counting shard operations, the FFT is ahead from `parity_shards >= 6` and the lead grows: 2.36x at 64+16, 4.92x at 64+64, 8.53x at 128+128. Below parity 6 the matrix engine wins at every `k`.

The important part is not the numbers but the shape. The ratio tends to `m / (log2(ceilPow2(m)) + 2)`, which is a function of the **parity count alone**. `k` does not appear. So the crossover is not a threshold in total shard count, and framings of the form "below roughly 32+16 you want matrix code" are the wrong shape, not merely the wrong number. A split with eight parity shards falls on the same side of it whether `k` is 16 or 128.

### Decode reverses it, decisively, at the erasure counts that occur

At one lost shard, with each operation weighted by its measured cost at 1200-byte shards, the matrix engine is ahead by 11.1x at 8+4, 11.8x at 16+8, 13.5x at 32+16, 15.0x at 64+16, 18.4x at 64+64 and 18.7x at 128+128 (`decode2`). Those are the drifting figures: the matrix side is wall-clock and the weights are measured live, so a rerun moves them by a few percent in either direction. The conclusion does not move, and the unweighted counts behind them do not drift at all.

These are leopard-parity numbers. An earlier pass omitted two optimizations that `klauspost/reedsolomon`'s `leopard8.go` has, a truncated IFFT and `errorBitfield` pruning, and putting them in cut the FFT's counts by 22% to 29%. That correction is worth stating because it is the difference between an honest comparison and a strawman: without it the same tables read 17.4x at 8+4 rather than 11.1x, flattering this crate by half again.

The reason is structural. Leopard decode is `Omega(n log n)` whatever `e` is, because it transforms the whole codeword either way. It is *not* constant in `e` (an earlier claim to that effect was an artifact of the unpruned implementation), but it grows only weakly: about 22% to 24% from one erasure to the correction limit. The matrix engine's decode, by section 3, is linear in `e`. One of these starts near zero and the other starts near its maximum.

Even at the correction limit the FFT is still behind in shard operations at 8+4 (3.91x), 16+8 (2.45x), 32+16 (1.42x) and 64+16 (1.54x). It only overtakes when a quarter to a half of everything is gone, which is a rebuild-from-archive workload, not a lossy-link one.

### The two engines cannot emit the same bytes

This settles the M5 constraint rather than leaving it to be discovered later. The FFT code is a different MDS code, not a different evaluation of the same one: its generator is `G[i][d] = omega_{1 + d/M} / (omega_i ^ omega_{M + d})`, a Cauchy code over the FFT's evaluation order, and the systematic Vandermonde this crate ships is not that matrix. Verified by direct comparison at 4+2, not assumed.

So an FFT engine cannot be an internal upgrade behind a dispatch threshold. It can only arrive as an explicitly selected, separately serialized format, which is exactly what section 7's M5 entry already requires in the case where byte-identical output is unachievable. That branch is now the live one.

### Coverage is a real constraint, and a small one

The leopard layout needs `k + ceilPow2(m) <= 256`, so some splits this crate supports cannot be expressed at all. The size of that is worth stating exactly rather than as a percentage: at `parity_shards <= 8` exactly **seven** splits in the whole field are inexpressible (249+5, 250+5, 251+5, 250+6, 251+6, 251+7 and 253+3); at `parity_shards <= 16`, thirty-five. An earlier report called this "33% of splits" and "the whole archival region", which is arithmetically true across all splits and misleading as a summary, because 94% of the inexpressible ones have `parity_shards >= 65`.

### The residual, which is open

One assumption in the combined-workload comparison is not safe: it weighted one decode per encode. On a lossy link only the blocks that actually lose a shard decode at all, so the real weighting is the block-loss rate, and at 1% block loss the FFT wins the combined workload at every split with `parity_shards >= 6`.

That is the strongest remaining case for building it. It is now mostly, but not entirely, closed. The driving consumer's own documentation recommends three splits, 8+1 for bandwidth-constrained links, 4+2 for general use and 4+4 for lossy ones. All three sit below the parity-6 crossover, so at the parameters that consumer actually advertises the FFT never wins the combined workload at any loss rate, and the matrix engine wins both halves outright.

What keeps it open is that the same consumer permits `parity_shards` up to 16. A user who configures 8 or 16 parity shards against a lossy link lands squarely in the region where the FFT wins, and nothing stops them. So the case for an FFT engine now rests on how far real deployments stray above the recommended parity counts, which is a narrower question than "what is the consumer's parity count" but still one about consumers rather than about codes. That is why open question 2 stays closed and this paragraph is not folded back into it.

---

## 9. Testing philosophy

Testing is deliberately disproportionate to the code size, for the reason in section 6: this is a library whose failures are silent and delayed.

- **Two independent constructions of the generator matrix.** The closed form that ships (one Lagrange evaluation per entry, `O(k^2)`) is asserted byte-identical to a Gauss-Jordan transcription of the textbook construction (`O(k^3)`, test-only) for all 32,640 supported shard splits. They share no code, so this is a genuine cross-check on the coefficients rather than a change-detector: a wrong closed form fails it, not merely a different one. This is what the known-answer vectors rest on, and it runs on every `cargo test`.
- **Exhaustive small-parameter sweep**, tiered. The brief's estimate of "a few million cases for total at most 32" is wrong by about five orders of magnitude: 16+16 alone has 2,362,232,013 erasure patterns. What is actually run is 4,830,487 cases in about 3.5 seconds: *every* erasure pattern within the correction limit for *every* split up to 18 total shards, then structured plus sampled patterns from 19 to 32, then boundary splits up to 255+1. A further 83,623,932 cases (exhaustive to 22 total) are behind `#[ignore]` and take 95 seconds. Where coverage is sampled rather than exhaustive, the test says so.
- **Cross-kernel equivalence**, arrived with M3 and now the main defence for the `unsafe` code: every kernel `Kernel::supported` reports against the scalar reference, byte-identical, for all 256 field scalars, over shard lengths chosen to cover every vector width and every tail remainder (1, 2, 3, 7, 8, 15, 16, 17, 31, 32, 33, 47, 48, 49, 63, 64, 65, 95, 96, 127, 128, 129, 255, 256, 257, 1200). A second test compares each kernel against `gf::mul` directly rather than against the scalar kernel, so a kernel and the reference cannot be wrong together. A third writes a canary past the end of the destination and checks it survives, because a vector kernel that rounds the length up would corrupt memory rather than produce wrong bytes.

  `mul_rows` gets the same treatment in its own shape, because a blocked kernel has failure modes the single-output sweep cannot reach: it peels groups of 8, 4, 2 and 1, so the parity counts swept (1, 2, 3, 4, 5, 7, 8, 9, 15, 16, 17) hit every group width exactly, one over and one under, at every tail remainder in the length list. The canary test is repeated per output row, because a kernel computing eight rows at once has eight chances to round a length up and seven of them are in rows the narrower test never creates. Source counts rotate through {1, 2, 5} along the sweep rather than crossing it: the cross reaches no path the rotation misses, and CI interprets both of these tests under Miri, where the cross costs about 37 minutes against 12.

  Those tests sweep only what the machine supports, which is right on a laptop and wrong in CI: a runner that quietly lost AVX2 would shrink the sweep and everything would still pass. `RS_REQUIRE_KERNELS` is set per CI job to the ISAs that runner is expected to provide, and a test turns a missing one into a failure. The benchmark harness runs the same agreement check before it times anything.
- **Closed form against the obvious implementation.** `src/matrix.rs` keeps the Gauss-Jordan construction as a `#[cfg(test)]` reference and asserts the shipped closed form agrees with it byte for byte at every shard count, which covers all 32,640 supported splits for the reason section 8a gives. Same discipline as the kernels: the fast path is checked against a transcription of the definition, never against itself.
- **Property tests** over random parameters, shard sizes and loss patterns.
- **Fuzzing** on the decode path, which parses caller-supplied presence masks and is the most attacker-adjacent surface.
- **Miri** over the scalar path and all non-SIMD logic.
- **Known-answer vectors** as the regression barrier for the wire format.

---

## 10. Toolchain policy

There is deliberately **no `rust-toolchain.toml`** in this repository.

A pinned toolchain file overrides the toolchain for every contributor and every CI job in the directory, which fights directly with a CI matrix that has to build on both stable and the declared MSRV, and it silently downgrades a contributor's `cargo clippy` to whatever version was pinned. The MSRV is declared where it belongs, in `rust-version` in `Cargo.toml`, where Cargo enforces it for consumers, and CI tests it explicitly. Contributors should use current stable.

If a future toolchain requirement genuinely needs pinning, for example a nightly feature in a build script, revisit this. Today it would only take choice away without buying anything.
