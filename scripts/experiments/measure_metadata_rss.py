#!/usr/bin/env python3
"""Measure daemon RSS while creating tiny files on a disposable FUSE mount."""
import argparse
import csv
import os
from pathlib import Path
import time


def proc_memory(pid):
    values = {}
    for line in Path(f"/proc/{pid}/status").read_text().splitlines():
        key, sep, value = line.partition(":")
        if sep and key in {"VmRSS", "VmHWM"}:
            values[key] = int(value.strip().split()[0]) * 1024
    return values


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--mountpoint", type=Path, required=True)
    ap.add_argument("--pid", type=int, required=True, help="PID of the argosfs mount-root process")
    ap.add_argument("--output", type=Path, required=True)
    ap.add_argument("--counts", default="1000,10000,100000", help="cumulative file counts")
    ap.add_argument("--bytes-per-file", type=int, default=4096)
    args = ap.parse_args()

    targets = sorted(set(int(n) for n in args.counts.split(",")))
    if not targets or targets[0] < 1 or args.bytes_per_file < 0:
        ap.error("file counts must be positive, payload size must be nonnegative")
    if not args.mountpoint.is_mount():
        ap.error("mountpoint must be a mounted filesystem")
    try:
        initial = proc_memory(args.pid)
    except (FileNotFoundError, ProcessLookupError) as exc:
        ap.error(f"cannot sample ArgosFS daemon: {exc}")

    benchmark = args.mountpoint / f"issue212-rss-{os.getpid()}"
    benchmark.mkdir()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    payload = b"x" * args.bytes_per_file
    began = time.monotonic()
    created = 0
    with args.output.open("w", newline="") as out:
        writer = csv.DictWriter(out, fieldnames=[
            "files", "elapsed_seconds", "rss_bytes", "hwm_bytes",
            "rss_growth_bytes", "rss_growth_per_inode_bytes", "mountpoint",
        ])
        writer.writeheader()
        for count in targets:
            while created < count:
                group = benchmark / f"d-{created // 1000:05d}"
                if created % 1000 == 0:
                    group.mkdir()
                (group / f"file-{created:07d}").write_bytes(payload)
                created += 1
            metrics = proc_memory(args.pid)
            rss_growth = metrics["VmRSS"] - initial["VmRSS"]
            writer.writerow({
                "files": count,
                "elapsed_seconds": round(time.monotonic() - began, 2),
                "rss_bytes": metrics["VmRSS"],
                "hwm_bytes": metrics.get("VmHWM", 0),
                "rss_growth_bytes": rss_growth,
                "rss_growth_per_inode_bytes": round(rss_growth / count),
                "mountpoint": str(args.mountpoint),
            })
            out.flush()
            print(f"{count} files: RSS={metrics['VmRSS'] / 1048576:.1f} MiB, "
                  f"growth={rss_growth / 1048576:.1f} MiB", flush=True)
    print(f"Files left in {benchmark}; remove only after verifying measurements.")


if __name__ == "__main__":
    main()
