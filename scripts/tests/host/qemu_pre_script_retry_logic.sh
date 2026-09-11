#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

kernel="$tmp/kernel"
rootfs="$tmp/rootfs"
: >"$kernel"
: >"$rootfs"

cat >"$tmp/qemu-img" <<'QEMU_IMG'
#!/usr/bin/env bash
set -euo pipefail
[ "$#" -eq 5 ]
[ "$1" = create ]
[ "$2" = -f ]
[ "$3" = raw ]
: >"$4"
QEMU_IMG
chmod +x "$tmp/qemu-img"

cat >"$tmp/qemu-no-console" <<'QEMU'
#!/usr/bin/env bash
cat >/dev/null
QEMU
chmod +x "$tmp/qemu-no-console"

cat >"$tmp/qemu-second-console" <<'QEMU'
#!/usr/bin/env bash
set -euo pipefail
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

suite_console_timeout_var() {
  case "$1" in
    full_guest.sh) printf '%s\n' ARGOSFS_QEMU_FULL_CONSOLE_TIMEOUT ;;
    block_lifecycle_stress.sh) printf '%s\n' ARGOSFS_QEMU_LIFECYCLE_CONSOLE_TIMEOUT ;;
    *) return 2 ;;
  esac
}

run_no_retry_case() {
  local script="$1"
  local name="${script%.sh}"
  local timeout_var
  local artifacts="$tmp/no-retry-$name"
  local output="$tmp/no-retry-$name.out"
  local start status elapsed

  timeout_var="$(suite_console_timeout_var "$script")"
  mkdir -p "$artifacts"
  start=$SECONDS
  set +e
  env \
    PATH="$tmp:$PATH" \
    ARGOSFS_QEMU_ARCH=arm64 \
    ARGOSFS_QEMU_BIN="$tmp/qemu-no-console" \
    ARGOSFS_QEMU_KERNEL="$kernel" \
    ARGOSFS_QEMU_ROOTFS="$rootfs" \
    ARGOSFS_TEST_ARTIFACTS="$artifacts" \
    ARGOSFS_QEMU_TIMEOUT=12 \
    ARGOSFS_QEMU_PRE_SCRIPT_CONSOLE_ATTEMPTS=1 \
    "$timeout_var=1" \
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
  [ "$(grep -c 'timed out waiting for .* console prompt' "$output" || true)" -eq 1 ]
  [ "$(grep -c 'retrying pre-script startup' "$output" || true)" -eq 0 ]
}

run_retry_boundary_case() {
  local script="$1"
  local name="${script%.sh}"
  local timeout_var
  local artifacts="$tmp/retry-$name"
  local output="$tmp/retry-$name.out"
  local count_file="$tmp/retry-$name.count"
  local status

  timeout_var="$(suite_console_timeout_var "$script")"
  mkdir -p "$artifacts"
  set +e
  env \
    PATH="$tmp:$PATH" \
    FAKE_QEMU_COUNT="$count_file" \
    ARGOSFS_QEMU_ARCH=arm64 \
    ARGOSFS_QEMU_BIN="$tmp/qemu-second-console" \
    ARGOSFS_QEMU_KERNEL="$kernel" \
    ARGOSFS_QEMU_ROOTFS="$rootfs" \
    ARGOSFS_TEST_ARTIFACTS="$artifacts" \
    ARGOSFS_QEMU_TIMEOUT=20 \
    ARGOSFS_QEMU_PRE_SCRIPT_CONSOLE_ATTEMPTS=3 \
    ARGOSFS_QEMU_SHELL_READY_TIMEOUT=1 \
    "$timeout_var=2" \
    "$repo/scripts/qemu/$script" >"$output" 2>&1
  status=$?
  set -e

  [ "$status" -ne 0 ]
  [ "$(cat "$count_file")" -eq 2 ]
  [ "$(find "$artifacts" -maxdepth 1 -name '*console-attempt-1.log' | wc -l)" -eq 1 ]
  [ "$(find "$artifacts" -maxdepth 1 -name '*console-ready' | wc -l)" -eq 1 ]
  [ "$(grep -c 'retrying pre-script startup' "$output" || true)" -eq 1 ]
  [ "$(grep -c 'timed out waiting for QEMU guest shell' "$output" || true)" -eq 1 ]
}

for script in full_guest.sh block_lifecycle_stress.sh; do
  run_no_retry_case "$script"
  run_retry_boundary_case "$script"
done

printf 'QEMU pre-script console retry tests passed\n'
