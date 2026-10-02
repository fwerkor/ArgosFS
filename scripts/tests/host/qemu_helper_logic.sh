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

console_log="$tmp/qemu-console.log"
console_input="$tmp/qemu-console.input"
printf '%s\n' '[   72.391219] procd: - init -' >"$console_log"
exec 9>"$console_input"
(
	sleep 2
	printf '%s\n' "login[1322]: root login on 'ttyAMA0'" >>"$console_log"
) &
console_writer_pid=$!
arch=arm64 ARGOSFS_QEMU_CONSOLE_WAKE_INTERVAL=1 \
	argosfs_qemu_wait_console_ready "$console_log" 1 5 "" "arm64 console readiness" 9
wait "$console_writer_pid"
exec 9>&-
if [ ! -s "$console_input" ]; then
	echo "QEMU console readiness helper did not nudge arm64 serial input" >&2
	exit 1
fi

probe_log="$tmp/qemu-console-probe.log"
probe_fifo="$tmp/qemu-console-probe.input"
mkfifo "$probe_fifo"
printf '%s\n' '[   80.000000] procd: - init -' >"$probe_log"
(
	while IFS= read -r -d $'\r' input; do
		if [[ "$input" == *"ARGOSFS_QEMU_CONSOLE_PROBE_READY_1"* ]]; then
			printf '%s\n' ARGOSFS_QEMU_CONSOLE_PROBE_READY_1 >>"$probe_log"
			break
		fi
	done <"$probe_fifo"
) &
probe_reader_pid=$!
exec 7>"$probe_fifo"
arch=arm64 ARGOSFS_QEMU_CONSOLE_WAKE_INTERVAL=1 \
	argosfs_qemu_wait_console_ready "$probe_log" 1 5 "" "arm64 execution probe" 7
exec 7>&-
wait "$probe_reader_pid"
argosfs_qemu_log_has_marker "$probe_log" ARGOSFS_QEMU_CONSOLE_PROBE_READY_1

phase_probe_log="$tmp/qemu-console-phase-probe.log"
phase_probe_input="$tmp/qemu-console-phase-probe.input"
{
	printf '%s\n' '[   80.000000] procd: - init -'
	printf '%s\n' ARGOSFS_QEMU_CONSOLE_PROBE_READY_1
	printf '%s\n' ARGOSFS_QEMU_CONSOLE_PROBE_READY_1
} >"$phase_probe_log"
exec 6>"$phase_probe_input"
start_seconds="$SECONDS"
set +e
arch=arm64 ARGOSFS_QEMU_CONSOLE_WAKE_INTERVAL=1 \
	argosfs_qemu_wait_console_ready "$phase_probe_log" 2 2 "" "second boot probe isolation" 6
phase_probe_status=$?
set -e
elapsed=$((SECONDS - start_seconds))
exec 6>&-
[ "$phase_probe_status" -ne 0 ]
if [ "$elapsed" -lt 1 ]; then
	echo "QEMU console readiness accepted duplicate probe markers from a prior boot phase" >&2
	exit 1
fi

prompt_log="$tmp/qemu-console-prompt.log"
printf '%s\n' 'Please press Enter to activate this console.' >"$prompt_log"
argosfs_qemu_wait_console_ready "$prompt_log" 1 2 "" "prompt readiness"

second_boot_log="$tmp/qemu-second-boot.log"
second_boot_input="$tmp/qemu-second-boot.input"
{
	printf '%s\n' '[   72.391219] procd: - init -'
	printf '%s\n' "login[1300]: root login on 'ttyAMA0'"
} >"$second_boot_log"
exec 8>"$second_boot_input"
(
	sleep 1
	if [ -s "$second_boot_input" ]; then
		touch "$tmp/second-boot-early-wake"
	fi
	printf '%s\n' '[   73.000000] procd: - init -' >>"$second_boot_log"
	sleep 1
	printf '%s\n' "login[1400]: root login on 'ttyAMA0'" >>"$second_boot_log"
) &
second_boot_writer_pid=$!
arch=arm64 ARGOSFS_QEMU_CONSOLE_WAKE_INTERVAL=1 \
	argosfs_qemu_wait_console_ready "$second_boot_log" 2 5 "" "second arm64 console readiness" 8
wait "$second_boot_writer_pid"
exec 8>&-
if [ -e "$tmp/second-boot-early-wake" ] || [ ! -s "$second_boot_input" ]; then
	echo "QEMU console readiness helper did not respect the requested reboot phase" >&2
	exit 1
fi

printf 'QEMU helper marker tests passed\n'
