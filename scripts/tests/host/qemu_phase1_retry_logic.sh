#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

kernel="$tmp/kernel"
rootfs="$tmp/rootfs"
: >"$kernel"
: >"$rootfs"

cat >"$tmp/qemu-no-console" <<'QEMU'
#!/usr/bin/env bash
cat >/dev/null
QEMU
chmod +x "$tmp/qemu-no-console"

run_no_console_case() {
  local script="$1"
  local name="${script%.sh}"
  local artifacts="$tmp/no-console-$name"
  local output="$tmp/no-console-$name.out"
  local start status elapsed

  mkdir -p "$artifacts"
  start=$SECONDS
  set +e
  ARGOSFS_QEMU_ARCH=arm64 \
    ARGOSFS_QEMU_BIN="$tmp/qemu-no-console" \
    ARGOSFS_QEMU_KERNEL="$kernel" \
    ARGOSFS_QEMU_ROOTFS="$rootfs" \
    ARGOSFS_TEST_ARTIFACTS="$artifacts" \
    ARGOSFS_QEMU_TIMEOUT=12 \
    ARGOSFS_QEMU_CRASH_CONSOLE_TIMEOUT=1 \
    ARGOSFS_QEMU_CHAOS_CONSOLE_TIMEOUT=1 \
    ARGOSFS_QEMU_PHASE1_CONSOLE_ATTEMPTS=1 \
    "$repo/scripts/qemu/$script" >"$output" 2>&1
  status=$?
  set -e
  elapsed=$((SECONDS - start))

  [ "$status" -ne 0 ]
  if [ "$elapsed" -ge 6 ]; then
    echo "$script did not fail fast after console timeout: ${elapsed}s" >&2
    cat "$output" >&2
    exit 1
  fi
  if grep -q 'timed out waiting for QEMU guest shell' "$output"; then
    echo "$script feeder continued after console timeout" >&2
    cat "$output" >&2
    exit 1
  fi
  [ "$(cat "$artifacts"/*phase1-feeder.status)" -eq 1 ]
}

cat >"$tmp/qemu-second-console" <<'QEMU'
#!/usr/bin/env bash
count=0
[ ! -s "$FAKE_QEMU_COUNT" ] || count="$(cat "$FAKE_QEMU_COUNT")"
count=$((count + 1))
printf '%s\n' "$count" >"$FAKE_QEMU_COUNT"
if [ "$count" -ge 2 ]; then
  printf 'Please press Enter to activate this console.\n'
fi
cat
QEMU
chmod +x "$tmp/qemu-second-console"

run_retry_boundary_case() {
  local script="$1"
  local name="${script%.sh}"
  local artifacts="$tmp/retry-$name"
  local output="$tmp/retry-$name.out"
  local count_file="$tmp/retry-$name.count"
  local status retries

  mkdir -p "$artifacts"
  set +e
  FAKE_QEMU_COUNT="$count_file" \
    ARGOSFS_QEMU_ARCH=arm64 \
    ARGOSFS_QEMU_BIN="$tmp/qemu-second-console" \
    ARGOSFS_QEMU_KERNEL="$kernel" \
    ARGOSFS_QEMU_ROOTFS="$rootfs" \
    ARGOSFS_TEST_ARTIFACTS="$artifacts" \
    ARGOSFS_QEMU_TIMEOUT=20 \
    ARGOSFS_QEMU_CRASH_CONSOLE_TIMEOUT=2 \
    ARGOSFS_QEMU_CHAOS_CONSOLE_TIMEOUT=2 \
    ARGOSFS_QEMU_SHELL_READY_TIMEOUT=1 \
    ARGOSFS_QEMU_PHASE1_CONSOLE_ATTEMPTS=2 \
    "$repo/scripts/qemu/$script" >"$output" 2>&1
  status=$?
  set -e

  [ "$status" -ne 0 ]
  [ "$(cat "$count_file")" -eq 2 ]
  find "$artifacts" -maxdepth 1 -name '*console-attempt-1.log' -print -quit | grep -q .
  find "$artifacts" -maxdepth 1 -name '*phase1-console-ready' -print -quit | grep -q .
  retries="$(grep -c 'retrying phase1' "$output" || true)"
  [ "$retries" -eq 1 ]
  [ "$(grep -c 'timed out waiting for QEMU guest shell' "$output" || true)" -eq 1 ]
}

for script in crash_recovery.sh mixed_chaos.sh; do
  run_no_console_case "$script"
  run_retry_boundary_case "$script"
done

printf 'QEMU phase1 fail-fast/retry tests passed\n'
