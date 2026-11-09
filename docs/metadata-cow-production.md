# Copy-on-write metadata and journal mutation tracking (#212)

ArgosFS retains the v1 raw/loop/host on-disk metadata layout. The
production inode index is now a persistent copy-on-write HAMT. Normal
Metadata::clone operations and the durable snapshot share immutable map
nodes; mutating an inode copies only its path to a new value. A separately
maintained dirty-inode index tracks mutations, deletions and additions.

## Journal correctness

Normal transactions compare dirty inode IDs against the last committed
snapshot and create the established v1 JSON-pointer journal delta format.
Snapshots recovered independently (or with different mutation baselines)
fall back to checking all inode IDs. A missing dirty-tracking hint must
never be confused with a durable commit: tracking is reset only after
successful durability, and committed-with-error outcomes also establish
a fresh baseline. Explicit metadata snapshots can be compared safely.

JSON map keys are serialized numerically sorted to preserve canonical
metadata hashes and the existing v1 checkpoint encoding. -0.0 is handled
as distinct from 0.0 during journal construction because numeric equality
in serde_json does not model byte-for-byte canonical hash equality.

## Memory guard update

The preceding PR uses a conservative 160 KiB/inode at 30% of physical/cgroup
RAM as a temporary fail-safe. With copy-on-write metadata, an opt-in synthetic
stress run allocated 274,103 inodes with 8 realistic shard records each:
RSS rose by 1,944.8 MiB, or about 7.1 KiB per inode, and cloning a full
metadata snapshot did not visibly increase RSS. This test exercised the same
Rust metadata model, not 274k FUSE file writes, journal checkpoints or
power-loss recovery.

The post-COW memory guard uses a 32 KiB/inode estimate and reserves
up to 65% of RAM for metadata. This ~4.5x measured allocation envelope
is workload-dependent and does not impose a hard process RSS ceiling. The
override ARGOSFS_METADATA_MAX_INODES retains its meaning.

cargo test --lib metadata_scale_274k -- --ignored --nocapture runs this
stress regression, and ARGOSFS_SCALE_INODES controls its scale. Use it only
on a machine with sufficient RAM.

## Limits and operational safety

- The active inode set is still fully resident: this is a production COW
  metadata index, not a lazy disk-paged namespace. Disk access, raw member
  quorum and journal replay continue to use the same proven v1 format.
- Very large directories, multi-stripe files, telemetry, checkpoint
  serialization and replay can exceed the average bytes/inode estimate.
- The raw checkpoint's fixed-size capacity limit is not removed here.
- For full qualification, run a 274k-file real FUSE backup on a disposable
  block pool with 15 GiB RAM, degraded-member replay and crash injection,
  and validate recovery against old-format images. Do not treat this synthetic
  stress result as proof of those cases.

The 274k workload should no longer require multiple full resident inode
snapshots. It remains necessary to characterize process RSS and checkpoint
capacity on the reporter's actual devices.
