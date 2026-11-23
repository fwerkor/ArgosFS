# Physical shard repair

A scrub or self-healing read can reconstruct an erasure-coded stripe without
re-encoding the file. The repair preserves every healthy shard's extent and the
existing stripe ID, layout, compression, encrypted payload, nonce and AAD.

## Repair transaction

1. The inode is pinned against writers by the metadata write lock (and by the
   inode lock in scrub/fsck). The target stripes are selected by the scan.
2. Read and verify **all** available shards, bypassing the reconstructed cache.
   An Offline member inside its grace period is not eligible for replacement;
   Failed/Removed members and checksum-invalid shards are.
3. Require at least `k` verified distinct slots, reconstruct missing slots
   using the recorded stripe's Reed–Solomon layout, and validate the complete
   recovered plaintext against `raw_sha256`. For an existing damaged shard,
   additionally compare the reconstructed shard against its recorded SHA-256.
4. Allocate new COW extents or unique host paths only for slots requiring
   replacement, excluding devices already holding a live slot. For a damaged
   Online member, another allocation on the same member is safe because its
   destination is distinct from the old extent/path.
5. Publish all replacement references for that inode in **one** journal
   transaction. Reclaim the old references with the existing staged/deferred
   block-reclamation mechanism. A failure before commit cleans unpublished host
   files, restores allocator metadata, and leaves the original inode in place.
   An indeterminate post-journal failure fences further writes and relies on
   normal journal recovery rather than deleting potentially committed shards.

If there are too few verified slots, insufficient healthy capacity, or a
transient second member failure, repair is deferred and reported. It must not
partially publish a repaired stripe or overwrite the only valid copy.

## Devices returning or being replaced

Failed or retired member slots are relocated onto available Online members,
provided each stripe retains distinct member placements. Once a relocation
commits, the old raw allocator extent is unreferenced and free in the current
metadata generation, even if its disk is physically absent. Its stale on-device
bytes cannot become a live shard without a new journal allocation/reference.
Host files are deleted after commit if accessible; a returning disconnected
host member may contain unreferenced files that `fsck --remove-orphans` can
reclaim. A member that returns during the grace interval is not relocated.

The existing metadata schema and on-disk format are unchanged; no pool
migration or reader-version bump is required.

## Reporting and validation

`scrub` and `fsck` reports include:

- `reconstructed_shards`: replacement physical slots published successfully
- `repair_bytes_read`: verified shard payload bytes read by successful
  reconstructions (excludes the initial scrub pass and retries)
- `repair_bytes_written`: newly written shard payload bytes
- `repair_elapsed_ms`: accumulated successful repair time
- `full_file_rewrites`: count of whole-file repair rewrites (zero in the
  physical-shard repair path)
- `repaired_files`: count of files containing one or more repaired slots;
  this is not a count of whole-file rewrites

Repair counts are interval deltas from per-volume monotonic counters, like the
existing retry counters; concurrent repair runs may be included in the same
report interval.

Unit tests cover offline/removed members, checksum-invalid data and parity,
COW location preservation, encryption nonce preservation, absent spare devices,
failed host journal commits, raw write failure/retry, allocator reconciliation
and restart/member return. Full data verification must still be run on
production pools, including `fsck`, `verify-journal`, and application-level
verification (e.g. Borg verify).

For a reproducible matched **9-member 4+4 / 151-file** reconstruction workload:

```sh
cargo test --test repair_benchmark -- --ignored --nocapture --test-threads=1
```

The benchmark prints a `BENCH_RESULT` line containing the elapsed wall time,
number of files and stripes affected, number of physical shard allocations,
and new shard payload bytes. The `ARGOSFS_REPAIR_BENCH_FILES` environment
variable sets a shorter test count (151 by default).

Measured **2026-11-21**, identical local runner and payload generator, 9
host-backed members, 4+4 layout, `ARGOSFS_REPAIR_BENCH_FILES=16`, 64 KiB/file:

| Metric | main before fix | shard-repair patch |
| --- | ---: | ---: |
| Rebuild elapsed | 35.828 s | 7.763 s |
| Damaged stripes | 59 | 60 |
| Replaced physical shards | 472 | 60 |
| Shard payload bytes written | 1,933,312 | 245,760 |
| Files touched by repair | 16 | 16 |
| Entire stripes re-encoded | 59 | 0 |

This benchmark uses `fs.scrub()` (`fsck --repair`), whose old path re-encoded
whole **affected stripes**; the incremental scrub/read-repair path previously
rewrote whole files and is separately covered by regression tests.

Placement depends on a randomly generated pool UUID, so the damaged stripe
count differs by one even with identical source data. This is a *near-matched*
comparison, not an exact extent-for-extent replay. Repair time fell about 78%
and physical write volume about 87% in this sample. The synthetic 64-KiB
files are **not** equivalent to Vincecate's 151-file, 33.4-minute field
workload, and no Borg/full-pool production benchmark has yet been performed.

