#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

# Load the real device handoff functions without running the initramfs main.
# Mock only mount operations and device creation; no elevated privileges needed.
sed '$d' "$repo/integrations/capos/initramfs/argosfs-root.sh" >"$tmp/rootscript.sh"
(
  . "$tmp/rootscript.sh"
  sysroot="$tmp/sysroot"
  mkdir -p "$sysroot/dev"

  is_mounted() {
    [ "$1" = "$sysroot/dev" ] && [ -e "$tmp/mounted" ]
  }
  mount() {
    [ "$1" = -t ] || return 1
    [ "$2" = tmpfs ] || return 1
    : >"$tmp/mounted"
  }
  chmod() { :; }
  populate_block_device_nodes() { :; }
  mknod() { ln -s /dev/null "$1"; }

  printf 'bad node\n' >"$sysroot/dev/console"
  prepare_new_root_dev
  [ -e "$tmp/mounted" ]
  for node in console tty ttyS0 null zero random urandom fuse; do
    [ -c "$sysroot/dev/$node" ] || exit 1
  done

  # A missing mount must never make us populate device nodes directly on
  # the persistent root filesystem and proceed to switch_root.
  rm -f "$tmp/mounted"
  mount() { return 1; }
  if prepare_new_root_dev; then
    echo 'device handoff accepted an unmounted /dev' >&2
    exit 1
  fi

  # A successful mount is still unsafe if a critical character node cannot
  # be created; this previously failed silently.
  rm -f "$sysroot/dev/console"
  mount() { : >"$tmp/mounted"; }
  mknod() { return 1; }
  if prepare_new_root_dev; then
    echo 'device handoff accepted a missing /dev/console' >&2
    exit 1
  fi

  # An apparently successful mount command that left /dev unmounted must
  # also be rejected.
  rm -f "$tmp/mounted"
  mount() { :; }
  if prepare_new_root_dev; then
    echo 'device handoff accepted a mount without a mountpoint' >&2
    exit 1
  fi
)

echo 'CapOS initramfs device handoff tests passed'
