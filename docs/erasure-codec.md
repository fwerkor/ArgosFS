# Reed–Solomon codec compatibility and dispatch

ArgosFS owns a native Reed–Solomon codec under `src/data/ec/` with a
byte-stable GF(2^8) systematic Vandermonde matrix and polynomial 0x11D.
The test-only erase 4.0.0 oracle checks persisted parity and recovery.

## Deployment and CPU compatibility

The engine chooses GFNI-512, GFNI-256, AVX-512BW, AVX2, SSSE3, ARM64 NEON or the portable scalar reference
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
rather than GFNI's AES-field multiplication instruction.

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
single-threaded encode and one-erasure data recovery measurements against
the original vendored implementation. It builds the original source from
the pinned Git revision in a temporary directory; the baseline is not a
dependency of ArgosFS. Throughput is data bytes for encode and recovered
bytes for decode, in GiB/s. Results depend on CPU, stripe geometry and cache
residency; they are not end-to-end disk throughput.
