# Read-side decoded block cache hot path

Borg verify-read benchmarks in [Issue #231](https://github.com/fwerkor/ArgosFS/issues/231#issuecomment-6949031846) identify repeated full-block copies and rehashing as a major source of ArgosFS FUSE worker CPU after #252 removed read-time checkpoint storms.

## Behavior

For normal, unencrypted logical file reads, `decode_inode_range_with_cache_policy_locked` first tries a validated cached subrange. A cache hit checks the immutable cached block's **admission-time** hash against the block's expected BLAKE3/SHA-256 digest and total length. It copies only the requested `[start,end)` range into the caller buffer. The special 1+0 uncompressed direct-range path continues to read and verify physical shard subblocks on every request, even if a prior full decode populated the cache; it is deliberately excluded from this range shortcut. A miss falls back to existing verified raw-shard or erasure decode paths. Invalid block length or content hash evicts the cache entry (and L2 copy) instead of returning stale data.

The L1 cache owns immutable `Arc<[u8]>` values. Once inserted, callers cannot mutate their storage. The existing full-block `get()` interface stays available to other callers, but it now uses the same checked digest metadata. SHA-256 for legacy hashes is calculated at most once per L1 entry. The LRU has per-entry indexed timestamps: hits, replacement, and invalidation avoid scanning every live key. Tick rollover is handled without changing eviction order. In-flight L2 promotions track mutations by cache key (and prefix invalidations), so unrelated block admissions do not cancel valid reads. Tracking entries disappear when their last in-flight read finishes; concurrent replacement or invalidation cannot resurrect a stale disk copy. Admission-time hashing is performed outside the global cache mutex.

L2 cache reads still verify the full on-disk payload before admission. Encrypted blocks never enter the decoded-block fast path. Physical scrub and repair **bypass reconstructed data caches**, still read/validate member shards and may perform parity reconstruction and repair. On a miss, no weakened checksum/repair path is introduced.

## Reproducing tests

```bash
cargo test --lib data::cache::tests -- --test-threads=1
cargo test --lib read_cache_range_tests -- --test-threads=1
cargo test --all-targets -- --test-threads=1
cargo clippy --all-targets --all-features -- -D warnings
```

An opt-in, non-CI timing comparison uses the original full-block `clone` + content hash and the new subrange API, each for 1,024 reads of 128 KiB from an immutable 1 MiB block:

```bash
cargo test --lib cache_subrange_microbench -- --ignored --nocapture
```

This microbenchmark isolates **only the decoded-block cache hotspot**. It is not a prediction of end-to-end Borg throughput; a reliable field comparison needs the same nine-member pool, benchmark repository, cold/warm cache state, and release binary, with daemon CPU, physical reads, and checkpoint counts captured. Other remaining costs include per-request metadata locking/snapshot copies and full metadata journal rollovers.


## Profiling the remaining warm/cold FUSE path (#270)

Since this follow-up, both `/metrics` and the metadata-lock-independent
`/commit-metrics` report decoded-block cache telemetry:

| Series | Interpretation |
| --- | --- |
| `argosfs_read_cache_requests_total{result="memory_hit"}` | Verified L1 hits (including direct subranges) |
| `argosfs_read_cache_requests_total{result="l2_hit"}` | Verified L2 promotions |
| `argosfs_read_cache_requests_total{result="miss"}` | Misses or rejected L2 objects |
| `argosfs_read_cache_direct_copy_bytes_total` | Verified bytes copied into a caller's read buffer using the direct-hit fast path |
| `argosfs_read_cache_l2_read_bytes_total` | Full encoded L2 cache-file bytes loaded before integrity validation |
| `argosfs_read_cache_memory_bytes` / `_items` | Currently resident L1 payload and entries |
| `argosfs_read_cache_l2_bytes` / `_items` | Currently indexed L2 disk payload and entries |

All counters reset on volume open. The copy-byte counter covers only successful
`copy_range` calls; it excludes misses, the physical shard reads, data assembled
after RS reconstruction, and FUSE/kernel copies. A cache hit still checks the
expected immutable checksum and length. Encrypted files and uncompressed
single-shard direct-range reads intentionally bypass this fast path.

To reproduce the physical Borg measurement, pin identical releases, pool
geometry (including the 4+4 layout), repository, FUSE options and worker
count; collect samples before and after **each** workload rather than comparing
cumulative mount totals. For cold runs start with an empty decoded-block L1/L2
cache and a controlled OS page cache on a dedicated test host; for warm runs
prime both builds with the same verified repository. Never drop the shared host
page cache on a production machine. Collect `/proc/diskstats` sector-read
deltas for **all physical members**, the daemon's per-thread and total
`/proc/<pid>/stat` CPU, peak RSS (`VmHWM`), wall time and cache-metric deltas.
Repeat runs in alternating build order and report medians and variance. Keep
`fsck`, `verify-journal`, degraded reconstruction and encrypted remount
checks separate from performance runs.

The cache metrics enable attributing a warm CPU regression to L1, L2 or
verified decode activity; they do **not** themselves constitute a physical
Borg performance comparison. Physical-device numbers require the reporter's
test hardware and fixture.

## L2 placement on real devices

Raw-device pools default to `/var/cache/argosfs/blocks/<pool-uuid>` when
running as root. Unprivileged raw pools use `$XDG_CACHE_HOME/argosfs/blocks`
(or `$HOME/.cache/argosfs/blocks`) with the same UUID suffix. This is stable
across device aliases and avoids accidentally storing gigabytes of decoded
blocks on `/dev/disk/by-partlabel` (normally devtmpfs). Loop-image pools keep
their colocated cache. `ARGOSFS_BLOCK_CACHE_DIR=/path/to/cache` takes precedence
for both backends; point it at an independent host SSD/NVMe filesystem, not
at the ArgosFS mount itself. `ARGOSFS_L2_CACHE_BYTES` overrides the configured
capacity in bytes, and `ARGOSFS_DISABLE_L2_CACHE=1` disables the disk tier.
Cache placement does not change the on-disk pool format or authoritative data.
An old cache directory is not automatically migrated or deleted; remove only
its disposable cache files after unmounting if no longer needed. L2 is optional
if the selected filesystem is unwritable. On embedded/read-only-root systems,
set an explicit writable cache directory or disable L2.
