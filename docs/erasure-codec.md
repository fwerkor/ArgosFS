# Reed–Solomon codec compatibility and dispatch

ArgosFS uses a vendored copy of reed-solomon-engine 0.2.0
(https://crates.io/crates/reed-solomon-engine/0.2.0), with a byte-stable
GF(2^8) systematic Vandermonde generator matrix and reduction polynomial
0x11D. This replaces the previous production dependency on erase 4.0.0.
The old implementation remains a test-only compatibility oracle so future
changes cannot silently alter stored parity or reconstructed bytes.

## Deployment and CPU compatibility

The engine chooses AVX2, SSSE3, ARM64 NEON or the portable scalar reference
using capabilities of the running CPU. The default build does not set
-march=native or require a C compiler for Reed–Solomon. Portable binaries
must be built without external target-cpu=native / target-feature flags:
explicitly targeting a newer CPU can bypass runtime feature detection.
Codecs are immutable; each reconstruction owns its scratch buffer, so
readers need no shared codec mutex.

Source provenance: reed-solomon-engine 0.2.0, crates.io archive SHA-256:
5d902386f55ac0890d3d0f56c932daa0b80b55bcf56631565eed76f0f7f54b6f.
The vendored copy includes license notices, upstream tests and wire-format
documentation. Cargo uses the local path with an exact version requirement;
the source is normalized using Rust 1.94 rustfmt for CI.

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
