# Performance report

The `Performance report` GitHub Actions workflow compares the loop-backed
ArgosFS FUSE frontend with ext4, XFS, Btrfs, F2FS, and OpenZFS on the same
Ubuntu runner. It also measures a regular directory on the runner filesystem.

| Scenario | Backing storage | Setup |
| --- | --- | --- |
| `argosfs-loop-default` | Image file | ArgosFS default group-commit policy, FUSE frontend |
| `ext4-loop` | Image file | Linux ext4 mounted as a loop device |
| `xfs-loop` | Image file (at least 384 MiB) | Linux XFS mounted as a loop device |
| `btrfs-loop` | Image file | Linux Btrfs mounted as a loop device |
| `f2fs-loop` | Image file | Linux F2FS mounted as a loop device |
| `zfs-pool` | Single file vdev | OpenZFS pool, `compression=off`, `ashift=12` |
| `host-directory` | Runner's filesystem | Regular directory, no loop image |

The runner supports additional ArgosFS durability scenarios via
`--scenarios argosfs-loop-strict,argosfs-loop-deferred,argosfs-loop-batched`.
The normal CI job benchmarks the default policy.

The benchmarks measure sequential write throughput including a final
`fsync`, sequential cached read throughput, small-file creation rate, and
small-file `stat` rate. Small-file creation does **not** fsync each file.

PRs and main use the quick workload with three iterations. Weekly scheduled
runs use the full workload with five iterations. Scenario order rotates between
iterations, and loop images are removed after each iteration to bound disk use.

Each run writes a table to the Actions job summary and uploads:

- `rootfs-perf.jsonl`: every raw measurement and any failures or skips.
- `performance-summary.csv`: status, sample counts, medians, ratios to ext4,
  and coefficients of variation.
- `performance-report.md`: table, variability, skipped scenarios, limits.
- `environment.txt`: runner, filesystem and ZFS module details.

## Environment and failure policy

The benchmark requires loop-device access, mount privileges, kernel filesystem
support, and the corresponding `mkfs` tools. OpenZFS additionally needs a
loadable `zfs` kernel module. Installing `zfsutils-linux` alone does not
provide the module on every runner kernel.

CI requires ArgosFS, ext4, XFS, Btrfs, F2FS, and host-directory measurements
to complete. **ZFS may be skipped only if its tools or kernel module are
unavailable**; the report contains an explicit `skipped` status and the
workflow emits a warning. Once ZFS is available, pool creation, benchmark
execution and teardown failures are treated as failures, not as skips.
All successful ZFS pools are destroyed in the cleanup path.

If strict coverage of ZFS is needed, run on a machine with OpenZFS kernel
support and omit `--optional-scenarios zfs-pool`.

## Interpretation

All measurements use the **same runner** and an image file (or ZFS file vdev)
on the runner's storage. These are not standalone physical disks, RAID setups,
or cold-cache tests. ext4, XFS, Btrfs, and F2FS use kernel drivers; ArgosFS
uses FUSE; OpenZFS uses its kernel module. The results show end-to-end behavior,
not the isolated cost of a filesystem algorithm. Caching, copy-on-write,
journaling and metadata durability policies differ. In particular, ZFS
performance is sensitive to its ARC cache and vdev setup.

The output is diagnostic, **not** a performance-regression gate; hosted runner
hardware and load vary. Stable regression gating needs dedicated hardware and
a historical baseline distribution.

Run locally with supported filesystems installed:

```bash
cargo build --release
python3 scripts/experiments/run_rootfs_perf.py \
  --mode quick \
  --iterations 3 \
  --require-all \
  --optional-scenarios zfs-pool \
  --output target/rootfs-perf/rootfs-perf.jsonl
python3 scripts/experiments/render_rootfs_perf_report.py \
  target/rootfs-perf/rootfs-perf.jsonl \
  --markdown target/rootfs-perf/performance-report.md \
  --csv target/rootfs-perf/performance-summary.csv \
  --commit "$(git rev-parse HEAD)"
```

Run as root or with passwordless `sudo` for mounts, ownership transfer,
ZFS pool creation, and teardown. The script only creates disposable image
files and its own uniquely named ZFS pool; it never formats physical devices.

### Fair cold ArgosFS / ZFS reads

Do not label an ArgosFS restart with a populated persistent L2 as a cold
read. Keep these four measurements distinct: empty persistent L2 with
admission enabled, L2 disabled, populated persistent L2 with OS page cache
dropped, and immediate fully warm reread. For ZFS cold, export/import the
test pool (and ensure its ARC/L2ARC is empty) before dropping the OS page
cache. Use only disposable test pools when dropping caches or exporting
pools. Record logical bytes, member reads/writes, cache counters, pool
geometry, commit ID and verification exit status alongside wall time.

Use `scripts/experiments/run_cold_read_comparison.py` to alternate A/B and
B/A on the same host and day. The JSON configuration contains a `cases`
array with two objects, one per filesystem, e.g.:

```json
{
  "cases": [
    {
      "filesystem": "argosfs",
      "cache_state": "cleared-l2",
      "prepare": [["/path/to/prepare-argosfs-cold-test"]],
      "verify": ["borg", "check", "--verify-data", "/test/argosfs/repo"],
      "cleanup": [["/path/to/cleanup-argosfs-test"]]
    },
    {
      "filesystem": "zfs",
      "cache_state": "empty-arc",
      "prepare": [["/path/to/prepare-zfs-cold-test"]],
      "verify": ["borg", "check", "--verify-data", "/test/zfs/repo"],
      "cleanup": [["/path/to/cleanup-zfs-test"]]
    }
  ]
}
```

The operator-provided preparation hooks must perform the actual cache reset
and mount operations and exit nonzero if they cannot establish the stated
cache conditions. For ArgosFS, use a dedicated fresh `ARGOSFS_BLOCK_CACHE_DIR`
for the test daemon, clear its test L2 before mounting and drop the OS page
cache before verification. Never clear an active production cache. For ZFS,
verify ARC/L2ARC emptiness after export/import; disable persistent L2ARC
for the comparison or explicitly clear it. Keep matched repository content,
cache sizes and drive state fixed between rounds. The driver cannot itself
certify the cache state on arbitrary storage hosts.

Run:

```sh
python3 scripts/experiments/run_cold_read_comparison.py cold-config.json results.jsonl --rounds 4
```

The output contains each preparation and verification command, cache-state
label, round, elapsed verification time and exit status. Preparation and
cleanup time is excluded. A failed hook or verification aborts the comparison
instead of silently adding a successful sample. Record device and cache
counters in the host-specific hook or verification wrapper, outside the timed
path wherever possible. Report all valid runs and their spread; do not
compare a populated L2 to an empty ARC.
