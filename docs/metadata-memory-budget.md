# Metadata inode budget and live RSS benchmark

ArgosFS's JSON metadata backend keeps all inodes and shard records resident.
Raw and loop pools now conservatively admit new inodes based on the minimum
of physical memory and cgroup memory quota. The **default limit** reserves
30% of that memory for estimated metadata RSS (160 KiB per inode, based on
issue #212's observed worst-case workload). The guard refuses *new inodes*
with ENOSPC rather than risking OOM. It does not stop growth from appending
shards to existing files, and is **not a guarantee against OOM**.

ARGOSFS_METADATA_MAX_INODES=N overrides the limit for the daemon;
N=0 explicitly disables it. Host volumes have no automatic limit.
The inode estimate is deliberately conservative and workload-dependent;
health --json and inspect-pool --json report metadata_inodes,
metadata_estimated_rss_bytes, and metadata_inode_limit.

To measure actual RSS, use a disposable loop pool mounted via FUSE, record
the long-lived argosfs mount-root PID and run:

    python3 scripts/experiments/measure_metadata_rss.py \
      --mountpoint /mnt/argosfs-test --pid 12345 \
      --output target/metadata-rss.csv --counts 1000,10000,100000

This script **creates files** and leaves them in the test pool for inspection.
Use a fresh disposable pool. Capture VmRSS/VmHWM and compare against inode count;
these process-level measures include allocator retention and transaction
temporaries, not just live Metadata allocation.
