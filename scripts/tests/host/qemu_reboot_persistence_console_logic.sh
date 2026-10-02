#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT

kernel="$tmp/kernel"
rootfs="$tmp/rootfs"
: >"$kernel"
: >"$rootfs"

cat >"$tmp/qemu-lost-prompts" <<'QEMU'
#!/usr/bin/env bash
set -euo pipefail

phase=1
ready=0
line=""

printf '%s\n' '[   80.000000] procd: - init -'

handle_line() {
  local input="$1"
  local marker

  if [ "$ready" -eq 0 ]; then
    printf "login[%d]: root login on 'ttyAMA0'\n" "$((1300 + phase))"
    printf '%s\n' 'BusyBox v1.37.0 built-in shell (ash)'
    ready=1
    return
  fi

  if [[ "$input" =~ echo[[:space:]]+(ARGOSFS_QEMU_SHELL_READY_[0-9]+) ]]; then
    marker="${BASH_REMATCH[1]}"
    printf '%s\n' "$marker"
  fi
  if [[ "$input" =~ echo[[:space:]]+(ARGOSFS_QEMU_UPLOAD_READY_[0-9]+) ]]; then
    marker="${BASH_REMATCH[1]}"
    printf '%s\n' "$marker"
  fi

  if [[ "$input" == *"sh '/tmp/argosfs-qemu-reboot-phase1.sh'"* ]]; then
    printf '%s\n' 'ARGOSFS_QEMU_REBOOT_PHASE1_BEGIN'
    printf '%s\n' 'ARGOSFS_ROOT_MOUNT_PHASE1 argosfs fuse.argosfs rw,relatime'
    printf '%s\n' 'ARGOSFS_REBOOT_ROOT_MARKER_PHASE1_OK'
    printf '%s\n' 'ARGOSFS_REBOOT_REQUESTED'
    phase=2
    ready=0
    printf '%s\n' '[   81.000000] procd: - init -'
  elif [[ "$input" == *"sh '/tmp/argosfs-qemu-reboot-phase2.sh'"* ]]; then
    printf '%s\n' 'ARGOSFS_QEMU_REBOOT_PHASE2_BEGIN'
    printf '%s\n' 'ARGOSFS_ROOT_MOUNT_PHASE2 argosfs fuse.argosfs rw,relatime'
    printf '%s\n' 'ARGOSFS_REBOOT_ROOT_MARKER_PHASE2_OK'
    printf '%s\n' 'ARGOSFS_REBOOT_PERSISTENCE_OK'
    printf '%s\n' 'ARGOSFS_QEMU_REBOOT_DONE'
    exit 0
  fi
}

while IFS= read -r -n1 ch; do
  if [ "$ch" = $'\r' ] || [ "$ch" = $'\n' ]; then
    handle_line "$line"
    line=""
  else
    line+="$ch"
  fi
done
QEMU
chmod +x "$tmp/qemu-lost-prompts"

artifacts="$tmp/artifacts"
mkdir -p "$artifacts"

ARGOSFS_QEMU_ARCH=arm64 \
ARGOSFS_QEMU_BIN="$tmp/qemu-lost-prompts" \
ARGOSFS_QEMU_KERNEL="$kernel" \
ARGOSFS_QEMU_ROOTFS="$rootfs" \
ARGOSFS_TEST_ARTIFACTS="$artifacts" \
ARGOSFS_QEMU_TIMEOUT=20 \
ARGOSFS_QEMU_REBOOT_LOGIN_DELAY=5 \
ARGOSFS_QEMU_REBOOT_DELAY=5 \
ARGOSFS_QEMU_CONSOLE_WAKE_INTERVAL=1 \
ARGOSFS_QEMU_SHELL_READY_TIMEOUT=3 \
ARGOSFS_QEMU_SCRIPT_READY_TIMEOUT=3 \
  "$repo/scripts/qemu/reboot_persistence.sh"

log="$artifacts/qemu-reboot-arm64.log"
if grep -Fq 'Please press Enter to activate this console.' "$log"; then
  echo "fake QEMU unexpectedly emitted an activation prompt" >&2
  exit 1
fi
[ "$(grep -Fc "root login on 'ttyAMA0'" "$log")" -eq 2 ]
grep -Fxq 'ARGOSFS_QEMU_REBOOT_DONE' "$log"

cat >"$tmp/qemu-retry-first-console" <<'QEMU'
#!/usr/bin/env bash
set -euo pipefail
count=0
[ ! -s "$FAKE_QEMU_COUNT" ] || count="$(cat "$FAKE_QEMU_COUNT")"
count=$((count + 1))
printf '%s\n' "$count" >"$FAKE_QEMU_COUNT"
if [ "$count" -eq 1 ]; then
  printf '%s\n' '[   80.000000] procd: - init -'
  cat >/dev/null
  exit 0
fi
exec "$FAKE_QEMU_WORKING" "$@"
QEMU
chmod +x "$tmp/qemu-retry-first-console"

retry_artifacts="$tmp/retry-artifacts"
retry_count="$tmp/retry-count"
retry_output="$tmp/retry-output"
mkdir -p "$retry_artifacts"
FAKE_QEMU_COUNT="$retry_count" \
FAKE_QEMU_WORKING="$tmp/qemu-lost-prompts" \
ARGOSFS_QEMU_ARCH=arm64 \
ARGOSFS_QEMU_BIN="$tmp/qemu-retry-first-console" \
ARGOSFS_QEMU_KERNEL="$kernel" \
ARGOSFS_QEMU_ROOTFS="$rootfs" \
ARGOSFS_TEST_ARTIFACTS="$retry_artifacts" \
ARGOSFS_QEMU_TIMEOUT=20 \
ARGOSFS_QEMU_REBOOT_LOGIN_DELAY=1 \
ARGOSFS_QEMU_REBOOT_DELAY=5 \
ARGOSFS_QEMU_PRE_SCRIPT_CONSOLE_ATTEMPTS=2 \
ARGOSFS_QEMU_CONSOLE_WAKE_INTERVAL=1 \
ARGOSFS_QEMU_SHELL_READY_TIMEOUT=3 \
ARGOSFS_QEMU_SCRIPT_READY_TIMEOUT=3 \
  "$repo/scripts/qemu/reboot_persistence.sh" >"$retry_output" 2>&1

[ "$(cat "$retry_count")" -eq 2 ]
[ -f "$retry_artifacts/qemu-reboot-arm64-console-attempt-1.log" ]
grep -q 'retrying pre-script startup (1/2)' "$retry_output"
grep -Fxq 'ARGOSFS_QEMU_REBOOT_DONE' "$retry_artifacts/qemu-reboot-arm64.log"

printf 'QEMU reboot persistence lost-prompt and retry tests passed\n'
