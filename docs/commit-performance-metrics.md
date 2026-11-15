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
binds before mounting and stops when the mount exits.

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
