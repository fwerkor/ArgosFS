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
