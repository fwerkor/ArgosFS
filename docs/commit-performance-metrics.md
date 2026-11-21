# Metadata commit telemetry (#231)

Use `argosfs mount-root ... --metrics-listen 127.0.0.1:9109` for a loop/raw
pool, or `argosfs mount <root> <mountpoint> --metrics-listen 127.0.0.1:9109`
for a host volume. `GET /metrics` includes the existing volume metrics
and the new commit phase counters. `GET /commit-metrics` reads per-volume
atomics and a small independent device-label registry; it **does not acquire
the namespace lock** and remains responsive while a metadata writer holds it.
Both paths are Prometheus text exposition; do not expose the server to
untrusted networks without access control. The separate `argosfs prometheus`
command creates its own volume handle and cannot observe counters in an
independently running FUSE mount. The optional `--metrics-listen` listener
binds before mounting. During unmount it stops accepting clients and waits
for already-accepted request threads to finish before releasing the mounted
volume handle and marking the pool clean. Read and write socket timeouts are
2 seconds; a request already inside the volume may wait for an existing
metadata operation to release its lock.

The three series, labeled by `phase` (and additionally by `disk` for
per-member journal/checkpoint I/O), are:

- `argosfs_commit_phase_duration_seconds_count`: total timed operations;
- `argosfs_commit_phase_duration_seconds_sum`: aggregate seconds;
- `argosfs_commit_phase_duration_seconds_max`: longest operation since open.

Unlabelled phase samples are global totals; disk-labeled samples are subsets
for the corresponding member, not additional operations to add to global totals.
The per-disk registry is limited to the devices encountered by the mount; its
short-lived lock is separate from the metadata writer lock and is never held
while performing I/O. A disk's `*_max` can identify an outlier member but is
not a causal diagnosis of a failing card or USB hub.

Counters start at zero when the volume opens and include failed attempts.
Use `rate(..._sum[5m]) / rate(..._count[5m])` to estimate recent average
phase duration; neither the counters nor the lifetime maximum represent
p95/p99. Run a separate latency histogram/workload recorder to study stalls.
These measurements are wall time (not CPU consumption) and nested phases
must not be added together as if independent.

| Phase | Instrumentation boundary |
|---|---|
| `group_commit` | Deferred group commit, including error rollback and post-write state updates |
| `integrity` | Full-state v1 or incremental v2 integrity preparation |
| `durability_validation` | Precommit data-durability verification |
| `data_flush` | Deferred member data flushes and repeat validation |
| `journal` | Journal append, quorum confirmation and crash-injection checks |
| `journal_delta` | Encoding changed-metadata delta before journal record construction |
| `journal_member_io` | One member's journal append/write (excluding deferred flush and quorum certificate), with additional per-disk labels |
| `journal_flush` | A single deferred journal member flush, including failures; *per-device durations* sum to more than wall time if parallelized in future |
| `journal_quorum` | Quorum commit certificate preparation excluded; timer covers certificate fanout/write/flush and final quorum decision |
| `journal_tip` | Reading/checking one member's current journal tip before appending a new transaction |
| `checkpoint` | Full checkpoint quorum operation on the deferred group-commit path |
| `checkpoint_encode` | Shared JSON/compressed checkpoint and page-index preparation |
| `checkpoint_member_io` | Per-member paired checkpoint writes and flushes; includes journal reset on rollover; also labeled by disk |
| `journal_rollover` | Rollover checkpoint, member fanout, journal reset and certificate |
| `sync_lock_wait` | Waiting for the metadata write guard in `ArgosFs::sync` |
| `sync_lock_hold` | Time inside that guard in `ArgosFs::sync` (not other namespace writers) |

### What the BorgBackup field tests show (2026-11-14)

[The reporter's v1 and v2 experiments](https://github.com/fwerkor/ArgosFS/pull/239)
used the same nine-member 4+4 pool, with near-identical ~15.4 GB source
backup workloads and a fresh Borg repository each time. These are *field
measurements*, not a hardware-independent performance guarantee:

| Workload-only | Legacy v1 | Upgraded delta integrity v2 |
|---|---:|---:|
| Wall time | 5692 s | 3298 s (1.73x speedup) |
| Deferred commits | 273 | 278 |
| Explicit full checkpoints | 244 | 0 |
| Journal rollovers | 2 | 27 |
| Checkpoint time | 3088 s | 0 s |
| Data flush time | 1098 s | 1076 s |
| Journal phase time | 27 s | 702 s |
| Total sync lock wait | 2760 s | 1029 s |

A **v1 pool without `raw-delta-integrity-v2`** takes a full-checkpoint path
for each dirty metadata group commit. Upgraded v2 pools commit incremental
journal records instead. The v2 8 MiB journal still rolls over and writes a
full member checkpoint, about once per **216 MB of new backup data** in this
run; its rollover member I/O was 787 cumulative seconds across devices, but
rollover wall time was 341 s (concurrent writes overlap). The explicit
checkpoint path achieved higher effective member-I/O overlap. Improving the
rollover and deferred data flush paths is therefore a separate goal of #231.
The offline integrity upgrade is one-way; do not run it on a production pool
without verifying the backup and mixed-version recovery plan.

`/metrics` blocks on the normal metadata snapshot. During v1 and v2 heavy
writes, 52/90 (58%) and 22/53 (41.5%) attempts with a 5-second timeout
failed respectively, with the longest observed commit around 61 and 57
seconds. `/commit-metrics` succeeded on **1074/1074** and **629/629** scrapes.
Use `/commit-metrics` for incident alerts and latency diagnosis; do not
interpret a `/metrics` scrape timeout during a long commit as proof that the
FUSE mount is down. The full endpoint has **no promised latency bound** under
write load.

`journal_delta` measures delta *construction*, including on v1 append paths;
it does not mean that a v1 pool has enabled the v2 hash scheme. The nested
`checkpoint_member_io` also counts full-checkpoint writes done **inside
`journal_rollover`**, not only `checkpoint` writes. Summing nested per-member
latencies, phase totals, and wall time double-counts parallel and nested work.
A single scrape can mix phases from an in-flight operation because the atomic
count, sum and max are published separately; use snapshots before/after a
completed workload for aggregate interpretation.

The v2 report also identified two recoverable shard checksum errors on one
physical member in an older Borg repository. Full Borg verification passed and
subsequent fsck was clean. Their cause was not established; no inference that
the integrity upgrade damaged data should be made from these observations.

**Coverage limitations:** The fine-grained raw-store phases currently cover
normal deferred group commits; manual repair, import initialization, replay,
and immediate transaction paths are not included in those per-phase counters.
The `sync_lock_*` values cover only the explicit sync entry point. For
concurrent FUSE read/stat/write tail latency, use an external request-level
workload and pair the observations with `/commit-metrics` rather than treating
these counters as request-latency measurements. Timers never hold an
additional lock or change the persistence, replay or quorum formats.

Comparison protocol: record the exact source SHA, device members, integrity
scheme (v1 vs offline-upgraded v2), kernel/FUSE configuration, metadata inode
count, dirty transaction batch size, and flush settings. Reuse the same pool
fixture and same operation ordering on baseline and candidate builds. Sample
per-request p50/p95/p99 and wall time separately from counters above, and
repeat across 10k, 100k and 500k inodes. Mixed-version recovery, degraded
members and crash injection must pass before marking #231 complete.


### Write-amplification counters for Issue #241

The lock-independent `/commit-metrics` endpoint (and the ordinary `/metrics`
endpoint) exposes `argosfs_data_plane_bytes_total{kind="..."}`. All four
counter series are cumulative since the volume was opened and require a
**before/after delta** around an isolated workload:

| `kind` | What is counted | Caveats |
|---|---|---|
| `range_accepted` | Successful `write_inode_range*` logical bytes | Does not include `write_file`/whole-file replacement, repair, or rebalance |
| `range_aligned_append` | Successful range writes on the stripe-aligned EOF fast path | Subset of `range_accepted`, not an independent numerator |
| `range_rewrite_input` | Successfully committed stripe-window input bytes re-encoded due to a non-aligned/overwriting range write | Includes old bytes carried forward and sparse zeros; multiple partial writes may re-encode the same window |
| `shard_payload_written` | Bytes of member shard payload successfully written by data-plane operations | Includes *all* writers, including repair/import/rebalance and failed transactions with successfully written but later unreferenced shards; excludes journal, flush metadata, device alignment, filesystem block allocation and host OS overhead |

In a **quiet foreground range-write-only** test, the ratio
`Δshard_payload_written / Δrange_accepted` is a lower-bound *payload-level*
write-amplification approximation. It does **not** capture total USB/device
physical bytes, raw metadata/checkpoint traffic, filesystem block allocation,
or retries that fail before a complete write. Use `/proc/diskstats` or direct
hardware counters for end-to-end physical write amplification. To find
incomplete-stripe churn, examine `Δrange_rewrite_input / Δrange_accepted`
and whether `Δrange_aligned_append` is low relative to accepted writes.

An unchanged 2+1, uncompressed, single 8192-byte full-stripe append writes
12,288 shard bytes, corresponding to 1.5x data payload redundancy. Appending
1,024 bytes to a new aligned stripe appends 1,024 raw bytes (1,536 shard
bytes), while the next 1,024-byte append to the same partial stripe re-encodes
its 2,048-byte window and adds another 3,072 shard bytes. The 4+4
pool used for #231 should approach a 2.0x shard-payload ratio for complete,
uncompressed stripes, but field totals depend on workload, encryption,
compression, and recovery activity. Do not interpret these counters as a
measured throughput speedup.

For a matched Borg run on an unshared set of physical members, the optional
sampler `scripts/experiments/measure_write_amplification.py` captures both
ArgosFS's new byte counters and Linux `/proc/diskstats` before and after a
single explicitly supplied command:

```bash
python3 scripts/experiments/measure_write_amplification.py \
  --endpoint http://127.0.0.1:9100/commit-metrics \
  --device sdX --device sdY --output /tmp/argosfs-write-amp.json \
  -- <your-borg-workload-command-and-arguments>
```

Replace `sdX`/`sdY` with **all** actual backing devices (and use the real
endpoint). The example placeholders are not executable as written. The
workload must complete its file fsync/close (and any deferred durability
boundary) before returning. Only run on scratch/test storage or with a
validated backup, and never combine a parent disk and its partitions because
that counts the same traffic twice. `diskstats` writes are 512-byte sectors,
not necessarily hardware NAND flash writes. The script exits with the
workload exit code, emits JSON including both byte-amplification estimates,
and never silently suppresses missing/negative counters. Workload stdout is
forwarded to stderr so stdout remains a single parseable JSON report. If the workload
terminates on a signal, its raw negative subprocess return code is retained in
JSON and the script returns the conventional shell status `128 + signal`.
It doesn't mount,
format or repair a volume itself.

### Advisory read telemetry versus durable metadata

On raw/LoopBlock pools, successful file reads and measured device-read
latencies update live inode `access_count`, `read_bytes`, `last_accessed_at`,
`workload_score` and disk EWMA counters. These measurements are **advisory**:
they may revert to their most recent durable values after a crash. They do
not independently make a read-only workload eligible for periodic deferred
metadata commits, journal writes or replicated full checkpoints. This avoids
turning `borg check --verify-data` into a metadata-write workload (#251).

When an actual file/namespace or allocator transaction commits, any live
advisory counters are included while retaining the last trusted durable
integrity hash as the previous journal state. Mandatory recovery checkpoints
and write-fencing remain unchanged. At clean unmount, advisory-only changes
are rehashed before the final required replicated metadata copies so the
checkpoint cannot contain stale integrity information. The ordinary
`/commit-metrics` counters remain runtime diagnostics, not durable storage.

### Concurrent read-side advisory accounting (#255)

Raw and LoopBlock FUSE reads now decode directly from a **shared** namespace
metadata guard instead of constructing one cloned decoder `Metadata` for
every request. They accumulate access/read-byte/workload-score changes and
per-device latency EWMAs in 64 separately locked runtime buckets, without
taking the global namespace write lock or the deferred-commit mutex. The
accounting is bounded by the number of distinct accessed inodes and members,
not the number of FUSE reads; a compact EWMA composition preserves the effect
of each latency sample. Explicit metadata snapshots project current pending
counters, while the next real namespace transaction merges them into its
integrity-checked journal/checkpoint image. Clean unmount merges and rehashes
pending advisory changes before writing its final checkpoint. Background
rebalance merges the current read heat before classifying inode placement.

This preserves verified data-shard reads, fail-closed corruption handling,
exclusive self-heal, and the read-only no-checkpoint guarantee from #251.
It does not change FUSE `max_read`, the raw on-disk format, checkpoint quorum,
or data durability. Physical Borg throughput still requires a matched device
retest to confirm the CPU gain; no speedup is inferred from unit tests.
