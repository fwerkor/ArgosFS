# L2 placement on real devices

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
