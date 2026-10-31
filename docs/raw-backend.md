# Raw and Loop Block Backend

ArgosFS supports three storage backends:

- `HostFsBackend`: compatibility and development mode. Shards are normal files
  under `.argosfs/devices/*/shards`.
- `LoopBlockBackend`: unprivileged block-semantics mode over fixed-size image
  files. Images are accessed only through `read_at`, `write_at`, `flush`, and
  capacity checks.
- `RawBlockBackend`: block-device mode for real devices or partitions. It uses
  the same raw layout as loop images and does not create ext4, XFS, btrfs, tar,
  sqlite, or a host directory tree inside the device.

## Layout

All multi-byte fields are explicitly little-endian. The current v1 layout is
4 KiB aligned:

```text
0                 protective header
4 KiB             primary superblock
64 KiB            device label
1 MiB             journal region
4 MiB             metadata region, two checkpoint slots
12 MiB            allocator region
16 MiB            data extent region
end - 1 MiB       backup superblock and backup label
```

Devices of at least 128 MiB use a larger layout: an 8 MiB journal and a
metadata region of `clamp(capacity / 64, 64 MiB, 1 GiB)`.  The metadata region
is split into two checkpoint slots, so the larger layout reserves between
32 MiB and 512 MiB for the live inode table.  The metadata is JSON, so a pool
with a very large number of small files can still exhaust the checkpoint slot;
when that happens writes fail with `ENOSPC` rather than retrying forever.
This capacity-based allocation applies only when formatting a **new** pool.
Existing devices retain the region descriptors stored in their superblocks;
mounting with a newer binary does not resize or move their metadata, journal,
or data regions. Existing pools with full checkpoints need an offline backup and
reformat to gain a larger metadata area. Do not rewrite superblock offsets in
place on a pool containing data.

The superblock records magic, format version, minimum compatible version,
endianness marker, pool UUID, device UUID, logical disk id, `k/m`, chunk size,
generation, clean state, feature flags, region descriptors, checksum algorithm,
alignment, label, and backup-superblock offset. A SHA-256 checksum covers the
encoded superblock with the checksum field zeroed.

Scan and open read the primary superblock first. If it fails checksum, magic, or
version validation, ArgosFS reads the backup superblock from the end-reserved
region. Duplicate device UUIDs, duplicate disk ids, mixed pool UUIDs, and disk
generation conflicts are rejected instead of guessed around.

## Metadata and Journal

Loop/raw metadata is persisted inside the raw metadata region, not in
`.argosfs/meta.json`. Each checkpoint stores a fixed header, metadata length,
metadata generation, SHA-256 of the logical JSON metadata payload, and a
page-indexed checksum tree for the checkpoint body. The index records each raw
metadata page offset, length, and SHA-256 digest, and the header commits the
index root hash after the body pages have been flushed. Two slots are written on
every member device, and readers still accept legacy single-payload checkpoints
for existing images.

The raw journal region stores length-prefixed JSON transaction records with an
entry checksum and an internal record hash. Records normally carry metadata
deltas and periodic full snapshots, so open can replay a newer committed state
when checkpoint copies are stale. Rw open marks member superblocks dirty. A
clean unmount writes fresh metadata checkpoint copies before marking the member
superblocks clean. A pool counts as cleanly unmounted when at least a metadata
quorum of its members is clean, not only when every member is clean: a card
that failed during the unmount (or missed a later cleanup) can stay dirty
without forcing every future writable mount to replay or refuse the pool.

A member that is absent from an open scan is recorded as `Offline`. Raw pools
have no administrative mark-offline command, so that state only means "this
member was missing the last time the pool was opened". Opening the pool again
marks every member that is now visible as `Online`, even if its reader was
only away for a moment and has since been power-cycled. Without that
reconciliation a USB reader that dropped off the bus once would stay offline
forever, and every later scrub, mount or layout change would report too few
online disks even after the card was back.

New loop/raw volumes created by the CLI use bounded group commit by default.
Ordinary buffered updates are accumulated in memory and persisted as one journal
transaction after either of these limits is reached:

- `--deferred-commit-interval-ms` (default `5000` milliseconds); or
- `--deferred-commit-max-transactions` (default `128` metadata transactions).

The FUSE mount owns the periodic timer, so an idle dirty filesystem cannot
remain uncommitted beyond the configured interval. A continuously busy
filesystem is also bounded by the transaction limit. `fsync`, `fdatasync`,
`O_SYNC`, `O_DSYNC`, explicit `ArgosFs::sync()`, and clean unmount bypass the
timer and synchronously commit the current group.

The default policy batches journal and data flushes together with metadata group
commit. `--defer-journal-flush`, `--defer-metadata-commit`, and
`--defer-data-flush` remain available for explicit legacy policy selection. Use
`--strict-durability` when per-transaction data and journal durability is
required. Existing volumes keep the policy stored in their metadata; opening an
older volume does not silently change its durability behavior.

Raw extents replaced or deleted inside an uncommitted group remain reserved.
ArgosFS releases them only at the same durability boundary that commits the new
metadata, preventing a later write from overwriting data still referenced by
the last durable checkpoint. A crash can therefore lose at most the configured
group window, while recovery continues to use checksum-valid, quorum-supported
metadata and journal records. Strict mode remains available as an explicit
compatibility and diagnostic mode.

## Device I/O deadline

A failing USB card can leave a `pwrite` or `fsync` blocked for tens of
seconds per command.  Raw and loop I/O is therefore guarded per device: every
read, write and flush runs on a device worker with a deadline (default **10 000 ms**,
override with `ARGOSFS_IO_TIMEOUT_MS`, clamped to 100–120 000 ms).  A device
that misses the deadline is marked failed for the rest of the mount, later
operations fail immediately with `DeviceFailed`, and the quorum code records
the member as unavailable.  The pool keeps running on its remaining members
when the `k+m` layout still has redundancy; if it does not, writes are fenced
but reads remain available.  This keeps a sick card from holding the metadata
lock and freezing FUSE `lookup`/`statfs` while the block layer retries it.

A worker thread is reused per device and pending operations are bounded. A
request waiting too long to start, or exceeding the queue capacity, fails
transiently without marking a healthy member failed. A running I/O that times
out cannot safely be interrupted in the kernel: the member remains fenced,
but its outstanding write may finish later. Do not reintroduce a timed-out
member without verifying and recovering it against a valid metadata quorum.

The deadline is a per-operation upper bound, not a throughput target.  A very
slow but healthy card can be fenced if it cannot finish one read/write/flush
inside the deadline; raise `ARGOSFS_IO_TIMEOUT_MS` for such hardware.  The
default is deliberately much shorter than the Linux SCSI timeout so a failed
member is isolated quickly, and much longer than a normal SD-card operation.

## Read-after-write verification

**Disabled by default**; enable on writable raw/loop mounts with
`ARGOSFS_VERIFY_WRITE_AFTER_MS` (10 ms–1 h; e.g. `60000`). The verifier records
each shard's physical extent and generation and schedules a delayed read. A
background worker flushes the member (also for deferred/bulk writes), requests
`POSIX_FADV_DONTNEED` for the aligned range, and checks its checksum. This
extra `fsync` can reduce throughput when the option is enabled. Cache eviction
is advisory, and removable media can still answer reads from internal cache:
a successful check is **not** a guarantee of power-loss durability.

Failed reads or mismatched checksums are retried after another delay. Set
`ARGOS_VERIFY_WRITE_RETRIES` (or `ARGOSFS_VERIFY_WRITE_RETRIES`, default 3,
range 0–10) for additional attempts. Before a failed member is quarantined,
ArgosFS checks that the original extent is still referenced by current
metadata. Deleting or rewriting a shard invalidates old checks so recycled
space cannot cause a false alarm. Once confirmed, a member is quarantined and
remaining data durability is validated; writes are fenced when required shards
would become unavailable. Scrub remains the recovery and repair path.

The queue holds up to 8192 pending verifications and drops oldest entries
under pressure. Unfinished entries are also discarded at unmount rather than
blocking indefinitely for their configured delay. `verify_written_skipped()`
reports such unverified work; `verify_written_failures()` counts confirmed
final failures. Unsupported cache eviction counts as skipped, not a device
failure. All counters are runtime-only; the process log reports confirmed
failures. Long-running soak tests should monitor skipped counts and run scrub
periodically, particularly after unmount or heavy write bursts.

## USB mode and shard-read retries

A USB card or its bridge can occasionally return a corrupt block once and the
correct data on the next read.  ArgosFS can retry a shard whose length or
checksum does not match before treating it as damaged:

- `ARGOSFS_READ_RETRIES` (0–10, default 0) is the number of extra reads.
  A final mismatch is still reported as a checksum error, so scrub can repair
  it.  I/O errors are not retried here; the guarded backend has already
  fenced that device.
- The retry applies to full-shard reads, which is what scrub and normal
  whole-block reads use.  It does not hide persistent corruption.
- Scrub's report (JSON and the human summary) includes
  `shard_read_retries` (extra reads after a mismatch) and
  `shard_read_recovered` (shards that read correctly on a retry).  A
  non-zero `shard_read_retries` with `shard_read_recovered` equal to it means
  every retry was a transient USB/controller error rather than bad media.

`ARGOSFS_USB=1` is a convenience profile for USB card jukeboxes.  It changes
the defaults of several options at once (an explicitly set option always
wins):

| option | normal default | `ARGOSFS_USB=1` |
|--------|----------------|-----------------|
| `ARGOSFS_IO_TIMEOUT_MS` | 10 000 | 120 000 |
| `ARGOSFS_READ_RETRIES` | 0 | 1 |
| `ARGOSFS_VERIFY_WRITE_AFTER_MS` | 0 (off) | 60 000 |
| `ARGOS_VERIFY_WRITE_RETRIES` | 3 | 3 |

For example, to mount a raw pool with all USB-friendly defaults:

```sh
ARGOSFS_USB=1 argosfs mount-root --devices /dev/... --target /mnt --mode rw
```

The read-after-write verifier costs an extra flush per checked shard, so USB
mode is not a free speed-up; it trades some throughput for more tolerance of
transient card/controller errors.  Scrub still performs the full check and
repair.

## Data Extents

Host shards use `ShardLocation::HostPath`. Loop/raw shards use
`ShardLocation::RawExtent { disk_id, offset, length, generation }`. The extent
length is allocator-aligned; the logical shard size and checksum still cover
only the actual encoded shard bytes.

## Safety

Raw mkfs refuses existing ArgosFS signatures or common filesystem/partition
signatures unless `--force` is explicit. `scan` and `inspect-device` are
readonly. `fsck` is readonly unless repair flags are passed. Recovery mounts are
readonly by default.

Transaction audit reports include per-member raw journal status and a majority
quorum flag. This makes partial fanout, unreadable member journals, and member
tail corruption visible to preflight tooling instead of hiding them behind only
the aggregate invalid-entry count.

Crash injection for loop/raw can be driven with `ARGOSFS_CRASH_POINT` using
`before-data-write`, `after-data-write-before-flush`,
`after-data-flush-before-journal-commit`,
`after-journal-commit-before-metadata-commit`,
`after-metadata-commit-before-superblock-update`, or `during-replay`.
