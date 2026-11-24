# Reed–Solomon codec compatibility and dispatch

ArgosFS owns a native Reed–Solomon codec under `src/data/ec/` with a
byte-stable GF(2^8) systematic Vandermonde matrix and polynomial 0x11D.
The test-only erase 4.0.0 oracle checks persisted parity and recovery.

## Deployment and CPU compatibility

The engine chooses GFNI-512, GFNI-256, AVX-512BW, AVX2, SSSE3, Linux AArch64 SVE/SVE2, ARM64 NEON or portable scalar
using capabilities of the running CPU. The default build does not set
-march=native or require a C compiler for Reed–Solomon. Portable binaries
must be built without external target-cpu=native / target-feature flags:
explicitly targeting a newer CPU can bypass runtime feature detection.
Codecs are immutable; each worker reuses a bounded single-entry recovery plan
keyed by geometry and presence mask, with scratch sized to the actual number
of missing data shards. Data-only reads do not allocate missing parity.
Encoding uses owned shard buffers and reuses the matrix across a write.
On x86, cache-sized tiles (4–64 KiB) keep source and output data local;
GFNI applies a precomputed affine transform over the legacy 0x11D field
rather than GFNI's AES-field multiplication instruction. For four or more
parity rows and at least four sources, AVX-512BW and both GFNI backends
compute four outputs together using two vector accumulators per row.
This shares source loads and writes each complete output vector once.
SSSE3 and AVX2 share nibble indices across two output rows. NEON uses
fixed-geometry 2+1 and 4+2 register kernels, plus groups of eight, four
and two rows. The portable path uses a compile-time 64 KiB multiplication
table. Multi-data-erasure recovery packs the cached recovery matrix and
uses the same multi-output kernels on disjoint shard borrows. No extra
payload buffer is allocated.
Remainder rows and partial vectors use slice operations.

On Linux AArch64, SVE2 or SVE is selected when supported and the current
thread has a vector length of at least 32 bytes; otherwise NEON is preferred.
SVE uses predicated loads/stores and byte table lookup; SVE2 combines three
XOR operands in one instruction. The stable Rust toolchain lacks scalable
vector intrinsics, so these two leaf routines use AAPCS64 assembly without
a separate compiler or library. All sixteen legal vector lengths
(128–2048 bits in 128-bit increments) and the NEON fallback are tested
by `scripts/ci/test-erasure-sve.sh` using QEMU. Real SVE hardware
performance remains unmeasured; dispatch is width-based, not autotuned.

The optimized arithmetic and SIMD kernels were adapted from
reed-solomon-engine 0.2.0. See the retained provenance and license notices in
`src/data/ec/THIRD_PARTY.md` and [wire format](erasure-wire-format.md).

## Disk format and upgrade safety

Raw superblock optional feature_flags bits 8..15 encode an erasure codec ID:

| ID | Meaning |
| -- | -- |
| 0 | Legacy implicit GF(2^8) Vandermonde (old superblocks) |
| 1 | Explicit GF(2^8) Vandermonde (new superblocks) |

Both IDs have identical shard bytes, parity and erasure-reconstruction
semantics. The raw format version and geometry do not change; historical
superblocks decode without migration. Existing pools retain ID 0 when
their superblocks are rewritten. New pools set ID 1, which older binaries
can still read since the byte format is unchanged and the field is optional.
Unknown IDs fail closed in new binaries. The codec ID is shown by
argosfs inspect-device in its superblock JSON.

**Do not** assign an incompatible future algorithm a new optional ID alone.
A genuinely incompatible codec also needs a mandatory feature bit, a
versioned migration plan, and downgrade fencing before any writer changes
stored shard bytes. Never auto-migrate on mount or silently reinterpret a stripe.

## Validation and limitations

The ArgosFS suite compares parity against erase 4.0.0 for 1+1, 2+1, 3+2,
4+4, 5+5, 6+6, all single/double erasures, exhaustive erasures up to m at
selected lengths, odd lengths, and SIMD vector/tail boundaries. It also
compares each runtime-supported SIMD backend against scalar and legacy.
Raw superblock tests cover ID 0, ID 1 and unknown identifiers; loop-backed
2+1 tests validate a legacy-ID pool after a clean reopen, writes, and another
reopen.

Both implementations reject empty parity-bearing shards and invalid
shard counts/lengths. Parity-free (m=0) layouts retain the existing bypass.
Corruption detection and verified recovery stay in the ArgosFS data plane;
the codec must never treat a checksum mismatch as a valid present shard.

No Borg or USB throughput improvement is claimed. On the reported SD-card
pool the bottleneck is mostly outside Reed–Solomon; the primary benefits are
portable runtime dispatch and removal of C SIMD toolchain maintenance.


## Packaging and maintenance

The native codec is included in both the checkout and the crates.io archive;
there is no production Reed–Solomon crate dependency. CI runs the independent
legacy oracle on the source and packaged crate. Changes must preserve the
wire contract, validate each supported SIMD backend, and exercise recovery
and fuzz tests. Benchmark changes on representative stripe geometries.

## Performance measurements

Run `scripts/experiments/bench-erasure-kernels.sh` for isolated,
single-threaded encode and one-/multiple-erasure data recovery measurements against
the original vendored implementation. It builds the original source from
the pinned Git revision in a temporary directory; the baseline is not a
dependency of ArgosFS. Throughput is data bytes for encode and recovered
bytes for decode, in GiB/s. The benchmark includes every runtime-supported
backend, including scalar and SSSE3. Results depend on CPU, stripe geometry and cache
residency; they are not end-to-end disk throughput.

### Representative results

Paired single-thread warm-buffer microbenchmarks (GiB/s, 1 MiB per shard,
rustc 1.94, optimized native and vendored builds, median of three samples).
Encode counts data bytes; decode counts recovered bytes. These are not
disk-throughput claims.

| CPU | Geometry | Operation | Vendor | Native | Backend |
| --- | --- | --- | ---: | ---: | --- |
| Xeon Gold 6138 | 16+8 | Encode | 1.254 | 4.685 | AVX-512BW |
| Xeon Gold 6138 | 16+8 | Recover 4 | 0.611 | 2.315 | AVX-512BW |
| Ryzen AI Max+ 395 | 8+4 | Encode | 14.505 | 49.299 | GFNI-512 |
| Ryzen AI Max+ 395 | 8+4 | Recover 4 | 7.382 | 27.193 | GFNI-512 |
| Ryzen AI Max+ 395 | 16+8 | Encode | 7.214 | 23.177 | GFNI-512 |
| Kunpeng 920 | 2+1 | Encode | 6.164 | 7.800 | NEON |
| Kunpeng 920 | 4+2 | Encode | 2.933 | 4.136 | NEON |
| Kunpeng 920 | 4+2 | Recover 2 | 1.359 | 2.160 | NEON |
| Kunpeng 920 | 16+8 | Recover 4 | 0.317 | 0.537 | NEON |

No claim that every CPU, geometry, shard size or operation is faster.
