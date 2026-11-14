#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
export ARGOSFS_COMPAT_SUITE="${ARGOSFS_COMPAT_SUITE:-mounted-fuse-compat}"

if [ "${ARGOSFS_COMPAT_RUN_AS_ROOT:-0}" = "1" ]; then
  "$repo_root/scripts/compat/with_fuse_mount.sh" \
    bash -c 'sudo env ARGOSFS_REQUIRE_CROSS_USER="${ARGOSFS_REQUIRE_CROSS_USER:-0}" ARGOSFS_COMPAT_CHECK="${ARGOSFS_COMPAT_CHECK:-}" python3 "$1" "$ARGOSFS_COMPAT_MOUNTPOINT"' _ \
    "$repo_root/scripts/compat/mounted_fuse_ops.py"
else
  "$repo_root/scripts/compat/with_fuse_mount.sh" \
    bash -c 'python3 "$1" "$ARGOSFS_COMPAT_MOUNTPOINT"' _ \
    "$repo_root/scripts/compat/mounted_fuse_ops.py"
fi

# Issue #228: saturate both filesystem execution workers with concurrent
# fsync writes while statvfs keeps arriving on independent FUSE readers.
# Keep this on the same conditional mounted-FUSE CI path, so environments
# without /dev/fuse continue to skip instead of failing the general suite.
ARGOSFS_FUSE_REQUEST_THREADS=2 ARGOSFS_FUSE_IO_WORKERS=2 \
  "$repo_root/scripts/compat/with_fuse_mount.sh" \
    bash -c 'python3 "$1" --rounds 12 --max-statfs-ms 500' _ \
    "$repo_root/tests/fuse_request_starvation.py"
