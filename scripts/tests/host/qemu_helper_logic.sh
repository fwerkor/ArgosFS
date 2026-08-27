#!/usr/bin/env bash
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
# shellcheck source=scripts/qemu/lib/common.sh
. "$repo/scripts/qemu/lib/common.sh"

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
log="$tmp/serial.log"

cat >"$log" <<'EOF'
> echo ARGOSFS_EVENT_DONE
root@CapOS:~# echo ARGOSFS_EVENT_DONE
awk '$2=="/"{print "ARGOSFS_ROOT_MOUNT " $1 " " $3; exit}' /proc/mounts
EOF

if argosfs_qemu_log_has_marker "$log" ARGOSFS_EVENT_DONE; then
	echo "marker helper accepted uploaded command text" >&2
	exit 1
fi
if [ "$(argosfs_qemu_log_marker_count "$log" ARGOSFS_EVENT_DONE)" -ne 0 ]; then
	echo "marker counter accepted uploaded command text" >&2
	exit 1
fi
if argosfs_qemu_log_has_root_mount "$log"; then
	echo "root mount helper accepted an unevaluated command" >&2
	exit 1
fi

{
	printf 'ARGOSFS_EVENT_DONE\r\n'
	printf 'ARGOSFS_ROOT_MOUNT argosfs fuse.argosfs rw,relatime\r\n'
	printf 'ARGOSFS_STRESS_WORKER_1_DONE rounds=42\r\n'
} >>"$log"

argosfs_qemu_log_has_marker "$log" ARGOSFS_EVENT_DONE
[ "$(argosfs_qemu_log_marker_count "$log" ARGOSFS_EVENT_DONE)" -eq 1 ]
argosfs_qemu_log_has_root_mount "$log"
argosfs_qemu_log_has_marker_prefix "$log" ARGOSFS_STRESS_WORKER_1_DONE

feeder_ok() {
	printf 'hello from feeder\n'
}

feeder_fail() {
	return 23
}

success_log="$tmp/qemu-helper-success.log"
# shellcheck disable=SC2016 # The inner shell must expand $line, not this test shell.
argosfs_qemu_run_with_feeder "$success_log" 10 feeder_ok \
	bash -c 'IFS= read -r line; [ "$line" = "hello from feeder" ]'
[ "$ARGOSFS_QEMU_FEEDER_STATUS" -eq 0 ]
[ "$ARGOSFS_QEMU_STATUS" -eq 0 ]

failure_log="$tmp/qemu-helper-failure.log"
start_seconds="$SECONDS"
argosfs_qemu_run_with_feeder "$failure_log" 30 feeder_fail \
	bash -c 'while :; do sleep 1; done'
elapsed=$((SECONDS - start_seconds))
[ "$ARGOSFS_QEMU_FEEDER_STATUS" -eq 23 ]
[ "$ARGOSFS_QEMU_STATUS" -ne 0 ]
if [ "$elapsed" -ge 5 ]; then
	echo "QEMU helper did not terminate emulator promptly after feeder failure: ${elapsed}s" >&2
	exit 1
fi

hard_timeout_log="$tmp/qemu-helper-hard-timeout.log"
start_seconds="$SECONDS"
ARGOSFS_QEMU_KILL_AFTER=1 argosfs_qemu_run_with_feeder "$hard_timeout_log" 1 feeder_ok \
	bash -c 'IFS= read -r _; trap "" TERM; while :; do sleep 1; done'
elapsed=$((SECONDS - start_seconds))
[ "$ARGOSFS_QEMU_FEEDER_STATUS" -eq 0 ]
[ "$ARGOSFS_QEMU_STATUS" -ne 0 ]
if [ "$elapsed" -ge 5 ]; then
	echo "QEMU helper hard timeout did not kill a TERM-resistant emulator promptly: ${elapsed}s" >&2
	exit 1
fi

printf 'QEMU helper marker tests passed\n'
