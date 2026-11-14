# Metadata commit telemetry (#231)

Use `argosfs mount-root ... --metrics-listen 127.0.0.1:9109` for a loop/raw
pool, or `argosfs mount <root> <mountpoint> --metrics-listen 127.0.0.1:9109`
for a host volume. `GET /metrics` includes the existing volume metrics
and the new commit phase counters. `GET /commit-metrics` reads **only
per-volume atomics**: it remains responsive while a metadata writer holds
the global namespace lock.
Both paths are Prometheus text exposition; do not expose the server to
untrusted networks without access control. The separate `argosfs prometheus`
command creates its own volume handle and cannot observe counters in an
independently running FUSE mount. The optional `--metrics-listen` listener
binds before mounting and stops when the mount exits.

The three series, labeled by `phase`, are:

- `argosfs_commit_phase_duration_seconds_count`: total timed operations;
- `argosfs_commit_phase_duration_seconds_sum`: aggregate seconds;
- `argosfs_commit_phase_duration_seconds_max`: longest operation since open.

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
| `journal_member_io` | One member's journal append/write (excluding deferred flush and quorum certificate) |
| `checkpoint` | Full checkpoint quorum operation on the deferred group-commit path |
| `checkpoint_encode` | Shared JSON/compressed checkpoint and page-index preparation |
| `checkpoint_member_io` | Per-member paired checkpoint writes and flushes; includes journal reset on rollover |
| `journal_rollover` | Rollover checkpoint, member fanout, journal reset and certificate |
| `sync_lock_wait` | Waiting for the metadata write guard in `ArgosFs::sync` |
| `sync_lock_hold` | Time inside that guard in `ArgosFs::sync` (not other namespace writers) |

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
