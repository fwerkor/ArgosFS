# Oversized raw metadata checkpoints (#212, #200)

The existing raw/loop checkpoint format stores compact JSON as separately
hashed 4 KiB pages, with two fully written and fsynced metadata slots on each
member and a quorum commit certificate. Its capacity is bounded by half the
metadata region on the smallest member (up to 512 MiB per slot).

ArgosFS now keeps format 1 (uncompressed JSON, v1 wire-compatible) for any
checkpoint that fits on every member. If the uncompressed checkpoint would
exceed a slot, the writer compresses the identical JSON with zstd level 3
and stores it as checkpoint format 2. The page hash/index and two-phase
publication remain identical to the original format. The new header includes
the original JSON length and SHA-256 digest; raw payload and every stored
page have their own SHA-256 digests as before. The reader validates all
checksums, requires the decompressed length/hash to match, and bounds
expanded metadata to 4 GiB to avoid unbounded decompression.

## Bounded-memory recovery (#221)

Format-2 recovery validates the claimed expanded length and SHA-256 in a
bounded zstd read pass, then decompresses again directly into the serde
deserializer. It no longer retains a full expanded JSON `Vec` while building
the inode map. The second pass trades decompression CPU for lower peak memory;
page and compressed-payload integrity checks remain unchanged.

Journal deltas are applied to individual inode/disk objects and small header
fields on a persistent COW metadata snapshot instead of allocating a complete
`serde_json::Value` tree for each record. The original full-JSON fallback is
retained for uncommon root or whole-map replacements in old journal records.
Writable `mount-root` also releases its read-only preflight snapshot before
opening the pool for writing.

The full active inode index still resides in memory, and integrity hashing
still serializes the metadata. These changes reduce recovery transients; they
are not a guarantee of a fixed RSS bound for arbitrary workloads.

An opt-in 144,550-inode recovery-memory test can be run on a disposable host
with sufficient resources (the synthetic dataset has eight shards per inode):

    ARGOSFS_SCALE_INODES=144550 ARGOSFS_SCALE_UNIQUE_SHARDS=1 \
      ARGOSFS_SCALE_PROFILE_RECOVERY=1 \
      cargo test --lib metadata_scale_274k_eight_shard_inodes -- --ignored --nocapture

Use a process memory limit when qualifying host-specific recovery budgets.

Compatibility: this code reads existing raw v1 checkpoints unchanged, and
continues writing v1 until a specific checkpoint no longer fits. **Once a
pool has written format 2, do not downgrade the binary to v1.2.0 or earlier.**
Older readers do not understand format 2. Restoring an old binary may fail
to mount, and unsupported recovery paths must never be used to override the
newly committed state. Keep a current binary available during recovery.

The writer still serializes the full inode table to prepare a checkpoint;
this does not provide true disk-paged inode lookup or constant-memory
checkpoint generation. Compressibility varies with inode names, extended
attributes, block count and shard checksums. An extremely large or mostly
incompressible checkpoint can still return ENOSPC, and journal capacity is
separate. This is not a replacement for a future bounded on-demand metadata
engine with a migration protocol.

Coverage:
- Oversize-to-format-2 and v1 format-preserving decisions
- Quorum write/read, mirror recovery after one corrupt page
- Invalid expanded-length rejection and page digest checks
- Existing raw journal crash-consistency and compatibility integration tests
- Opt-in 274,103 inode / eight-shard synthetic benchmark can additionally
  report the zstd checkpoint size via ARGOSFS_SCALE_COMPRESS_CHECKPOINT=1

## Reproducible scale characterization

An opt-in run with 274,103 synthetic file inodes and eight shards per file,
using unique deterministic shard hashes and paths, produced 1,015.1 MiB of
uncompressed checkpoint JSON and 109.7 MiB at zstd level 3 (9.25x).
With the maintained imbl backing map, the inode table occupied about
1.46 GiB RSS; cloning the metadata snapshot
did not materially increase it. These numbers are from a constructed
workload, not the reporter's actual SD cards, directory tree or FUSE writes.

Reproduce using:
    ARGOSFS_SCALE_UNIQUE_SHARDS=1 ARGOSFS_SCALE_COMPRESS_CHECKPOINT=1
    cargo test --lib metadata_scale_274k -- --ignored --nocapture

The 109.7 MiB payload fits a metadata slot on a sufficiently large
device, but the smallest member's region layout remains authoritative.
