#!/usr/bin/env python3
"""Measure a single explicit workload against ArgosFS's live byte counters.

The workload command must itself force the expected fsync/close/flush before
returning. Raw /proc/diskstats includes unrelated traffic and host metadata;
its result is meaningful only with exclusive devices and an isolated run.
"""

from __future__ import annotations

import argparse
import json
import math
import subprocess
import sys
import time
import urllib.request
from pathlib import Path
from typing import Mapping

COUNTER = 'argosfs_data_plane_bytes_total{kind="'
KINDS = (
    "range_accepted",
    "range_aligned_append",
    "range_rewrite_input",
    "shard_payload_written",
)


def parse_metrics(raw: str) -> dict[str, int]:
    values: dict[str, int] = {}
    for line in raw.splitlines():
        if not line.startswith(COUNTER):
            continue
        kind, sep, tail = line[len(COUNTER) :].partition('"}')
        if not sep or kind not in KINDS or not tail.lstrip().strip():
            continue
        parts = tail.split()
        if len(parts) != 1:
            raise ValueError(f"unexpected data-plane sample: {line!r}")
        number = float(parts[0])
        if not math.isfinite(number) or number < 0 or not number.is_integer():
            raise ValueError(f"invalid data-plane counter: {line!r}")
        values[kind] = int(number)
    missing = set(KINDS) - values.keys()
    if missing:
        raise ValueError(f"missing ArgosFS byte counters: {sorted(missing)}")
    return values


def parse_diskstats(raw: str, devices: set[str]) -> dict[str, int]:
    sectors: dict[str, int] = {}
    for line in raw.splitlines():
        fields = line.split()
        if len(fields) > 9 and fields[2] in devices:
            # Linux /proc/diskstats field 10: 512-byte sectors written.
            sectors[fields[2]] = int(fields[9])
    missing = devices - sectors.keys()
    if missing:
        raise ValueError(f"devices absent from /proc/diskstats: {sorted(missing)}")
    return {device: sector_count * 512 for device, sector_count in sectors.items()}


def fetch_metrics(url: str, timeout: float) -> dict[str, int]:
    with urllib.request.urlopen(url, timeout=timeout) as response:
        return parse_metrics(response.read().decode("utf-8"))


def diskstats_snapshot(devices: set[str]) -> dict[str, int]:
    return parse_diskstats(Path("/proc/diskstats").read_text(), devices)


def deltas(before: Mapping[str, int], after: Mapping[str, int]) -> dict[str, int]:
    difference = {key: after[key] - old for key, old in before.items()}
    if any(delta < 0 for delta in difference.values()):
        raise ValueError("counter decreased (device reset or volume reopened during benchmark)")
    return difference


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--endpoint", required=True, help="full /commit-metrics HTTP URL")
    parser.add_argument("--device", action="append", default=[], help="diskstats name (repeat for every device)")
    parser.add_argument("--timeout", type=float, default=8, help="metrics request timeout in seconds")
    parser.add_argument("--settle", type=float, default=0, help="post-command delay for deferred I/O; workload must fsync before returning")
    parser.add_argument("--output", help="optional path to the JSON report")
    parser.add_argument("command", nargs=argparse.REMAINDER, help="explicit workload command after --")
    args = parser.parse_args(argv)
    if not args.endpoint.startswith("http://") and not args.endpoint.startswith("https://"):
        parser.error("--endpoint must be an absolute HTTP(S) URL")
    if not (args.timeout > 0 and math.isfinite(args.timeout)) or not (args.settle >= 0 and math.isfinite(args.settle)):
        parser.error("--timeout and --settle must be finite, with timeout > 0 and settle >= 0")
    if not args.device or len(set(args.device)) != len(args.device):
        parser.error("specify unique --device names from /proc/diskstats; do not combine a parent disk with its partitions")
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command:
        parser.error("supply the explicit workload command after --")
    devices = set(args.device)
    before_metrics = fetch_metrics(args.endpoint, args.timeout)
    before_disk = diskstats_snapshot(devices)
    started = time.monotonic()
    # Preserve stdout as a single machine-parseable JSON document. Forward
    # the workload's normal stdout to the sampler's stderr (where the
    # workload's own stderr already goes), even when --output is omitted.
    result = subprocess.run(command, stdout=sys.stderr, check=False)
    if args.settle:
        time.sleep(args.settle)
    elapsed = time.monotonic() - started
    after_disk = diskstats_snapshot(devices)
    after_metrics = fetch_metrics(args.endpoint, args.timeout)
    bytes_delta = deltas(before_metrics, after_metrics)
    disk_delta = deltas(before_disk, after_disk)
    accepted = bytes_delta["range_accepted"]
    report = {
        "command": command,
        "exit_code": result.returncode,
        "wall_seconds": round(elapsed, 6),
        "volume_counter_deltas": bytes_delta,
        "diskstats_written_bytes": disk_delta,
        "diskstats_written_total_bytes": sum(disk_delta.values()),
        "shard_payload_write_amplification": (
            bytes_delta["shard_payload_written"] / accepted if accepted else None
        ),
        "stripe_reencoding_ratio": (
            bytes_delta["range_rewrite_input"] / accepted if accepted else None
        ),
        "diskstats_write_amplification": (
            sum(disk_delta.values()) / accepted if accepted else None
        ),
        "caveats": (
            "Ratios require isolated range-write workload on exclusive member devices; "
            "metadata, retries, other writers and filesystem IO affect physical diskstats. "
            "The workload must flush/fsync/close before returning. "
            "Do not add parent disk and partition counters together."
        ),
    }
    output = json.dumps(report, indent=2, sort_keys=True) + "\n"
    if args.output:
        Path(args.output).write_text(output)
    print(output, end="")
    # subprocess uses negative values for signals; sys.exit(-15) is 241,
    # which hides SIGTERM from shell-based benchmark automation. Preserve
    # the signal number in JSON, but use conventional shell status 128+signal.
    return 128 - result.returncode if result.returncode < 0 else result.returncode


if __name__ == "__main__":
    sys.exit(main())
