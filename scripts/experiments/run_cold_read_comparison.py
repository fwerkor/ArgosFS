#!/usr/bin/env python3
"""Interleaved read A/B with explicit, recorded cache preparation.

The configuration is trusted local input. Hooks are argv arrays, never shell
strings; an operator supplies pool-specific mount and cache reset commands.
Only the verify command is timed, and failed preparation never yields a sample.
"""

from __future__ import annotations

import argparse
import json
import subprocess
import time
from pathlib import Path

CACHE_STATES = {
    "argosfs": {"cleared-l2", "disabled-l2", "warm-l2", "warm"},
    "zfs": {"empty-arc", "warm"},
}
COLD = {"cleared-l2", "empty-arc"}


def validate(config):
    cases = config["cases"]
    if len(cases) != 2 or {c["filesystem"] for c in cases} != set(CACHE_STATES):
        raise ValueError("exactly one ArgosFS and one ZFS case are required")
    for case in cases:
        if case["cache_state"] not in CACHE_STATES[case["filesystem"]]:
            raise ValueError("invalid cache state")
        for field in ("prepare", "cleanup"):
            hooks = case[field]
            if not isinstance(hooks, list) or any(not valid_argv(hook) for hook in hooks):
                raise ValueError(f"{field} must contain argv arrays")
        if not case["prepare"] or not valid_argv(case["verify"]):
            raise ValueError("explicit preparation and verification are required")
    if {c["cache_state"] for c in cases} != COLD:
        raise ValueError("headline cold A/B requires cleared L2 and empty ARC")
    return cases


def valid_argv(argv):
    return isinstance(argv, list) and bool(argv) and all(isinstance(a, str) and a for a in argv)


def hooks(commands):
    for command in commands:
        subprocess.run(command, check=True)


def measure(case, round_number):
    record = {
        "round": round_number,
        "filesystem": case["filesystem"],
        "cache_state": case["cache_state"],
        "prepare": case["prepare"],
        "verify": case["verify"],
        "cleanup": case["cleanup"],
    }
    try:
        hooks(case["prepare"])
        start = time.monotonic()
        result = subprocess.run(case["verify"], check=False)
        record["wall_sec"] = time.monotonic() - start
        record["returncode"] = result.returncode
        record["status"] = "ok" if result.returncode == 0 else "failed"
    except (OSError, subprocess.CalledProcessError) as error:
        record.update(status="failed", error=str(error))
    finally:
        try:
            hooks(case["cleanup"])
        except (OSError, subprocess.CalledProcessError) as error:
            record.update(status="failed", cleanup_error=str(error))
    return record


def run_comparison(config, rounds, output):
    cases = validate(config)
    if rounds < 2:
        raise ValueError("at least two rounds are required for A/B and B/A")
    # Do not silently append to a prior benchmark or mix configurations.
    with output.open("x", encoding="utf-8") as stream:
        for number in range(1, rounds + 1):
            ordered = cases if number % 2 else list(reversed(cases))
            for case in ordered:
                record = measure(case, number)
                stream.write(json.dumps(record, sort_keys=True) + "\n")
                stream.flush()
                if record["status"] != "ok":
                    raise RuntimeError("comparison aborted after failed hook or verify")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("config", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--rounds", type=int, default=4)
    args = parser.parse_args()
    run_comparison(json.loads(args.config.read_text()), args.rounds, args.output)


if __name__ == "__main__":
    main()
