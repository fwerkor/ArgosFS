"""Unit tests for the optional native-filesystem benchmark paths."""

from __future__ import annotations

import contextlib
import csv
import io
import json
import subprocess
import sys
import tempfile
import unittest
from collections import defaultdict
from pathlib import Path
from unittest.mock import patch

import render_rootfs_perf_report as report
import run_rootfs_perf as runner


class FilesystemBenchmarkTests(unittest.TestCase):
    def test_image_sizes_allow_xfs_minimum(self) -> None:
        mib = 1024 * 1024
        self.assertEqual(runner.filesystem_image_size("quick", "ext4"), 128 * mib)
        self.assertEqual(runner.filesystem_image_size("quick", "xfs"), 384 * mib)
        self.assertEqual(runner.filesystem_image_size("full", "xfs"), 512 * mib)
        self.assertEqual(runner.filesystem_image_size("quick", "zfs"), 512 * mib)
        self.assertEqual(runner.filesystem_image_size("full", "zfs"), 1024 * mib)

    def test_mkfs_failure_is_recorded_as_failure_not_skip(self) -> None:
        with tempfile.TemporaryDirectory() as dirname:
            work = Path(dirname)
            output = work / "results.jsonl"

            def fake_run(args: list[str], **kwargs: object) -> subprocess.CompletedProcess[str]:
                if args[0] == "mkfs.xfs":
                    return subprocess.CompletedProcess(args, 1, "", "test formatting error")
                return subprocess.CompletedProcess(args, 0, "", "")

            with patch.object(runner, "command", return_value="/usr/bin/tool"), patch.object(
                runner, "run", side_effect=fake_run
            ):
                runner.run_xfs_loop(output, "quick", work, 16, 256)

            record = json.loads(output.read_text().strip())
            self.assertEqual(record["scenario"], "xfs-loop")
            self.assertEqual(record["status"], "failed")
            self.assertIn("formatting error", record["reason"])

    def test_missing_zfs_kernel_module_is_a_skip(self) -> None:
        with tempfile.TemporaryDirectory() as dirname:
            output = Path(dirname) / "results.jsonl"
            module_error = subprocess.CompletedProcess(
                ["modprobe", "zfs"], 1, "", "Module zfs not found"
            )
            with patch.object(runner, "command", return_value="/usr/bin/tool"), patch.object(
                runner, "run", return_value=module_error
            ):
                runner.run_zfs_pool(output, "quick", Path(dirname), 16, 256)
            record = json.loads(output.read_text().strip())
            self.assertEqual(record["status"], "skipped")
            self.assertIn("kernel module unavailable", record["reason"])

    def test_zfs_pool_destroyed_when_workload_fails(self) -> None:
        with tempfile.TemporaryDirectory() as dirname:
            work = Path(dirname)
            output = work / "results.jsonl"
            calls: list[list[str]] = []

            def fake_run(args: list[str], **kwargs: object) -> subprocess.CompletedProcess[str]:
                calls.append(args)
                return subprocess.CompletedProcess(args, 0, "", "")

            with patch.object(runner, "command", return_value="/usr/bin/tool"), patch.object(
                runner, "run", side_effect=fake_run
            ), patch.object(
                runner, "workload", side_effect=OSError("simulated I/O error")
            ):
                runner.run_zfs_pool(output, "quick", work, 16, 256)

            records = [json.loads(line) for line in output.read_text().splitlines()]
            self.assertEqual(records[0]["scenario"], "zfs-pool")
            self.assertEqual(records[0]["status"], "failed")
            self.assertTrue(
                any("destroy" in cmd for cmd in calls),
                "a failed ZFS workload must destroy its temporary pool",
            )

    def test_require_all_only_exempts_optional_skips(self) -> None:
        with tempfile.TemporaryDirectory() as dirname:
            root = Path(dirname)
            base = [
                "run_rootfs_perf.py", "--mode", "quick", "--iterations", "2",
                "--scenarios", "zfs-pool", "--workdir", str(root / "work"),
                "--output", str(root / "metrics.jsonl"), "--require-all",
            ]
            with patch.object(runner, "command", return_value=None):
                with patch.object(sys, "argv", base + [
                    "--optional-scenarios", "zfs-pool"
                ]):
                    self.assertEqual(runner.main(), 0)
                with patch.object(sys, "argv", base):
                    with contextlib.redirect_stderr(io.StringIO()):
                        self.assertEqual(runner.main(), 1)

    def test_report_keeps_skipped_filesystem_visible(self) -> None:
        record = {
            "status": "passed",
            "iteration": 1,
            "large_write_mib_s": 100.0,
            "large_read_mib_s": 200.0,
            "small_create_files_s": 300.0,
            "small_stat_files_s": 400.0,
        }
        records = [
            {"scenario": "environment", "status": "info", "mode": "quick"},
            {"scenario": "ext4-loop", **record},
            {"scenario": "argosfs-loop-fuse-default", **record},
            {"scenario": "xfs-loop", **record},
            {"scenario": "zfs-pool", "status": "skipped",
             "iteration": 1, "reason": "ZFS module unavailable"},
        ]
        aggregates = defaultdict(lambda: defaultdict(list))
        for item in records:
            if item.get("status") == "passed":
                for metric, _, _ in report.METRICS:
                    aggregates[item["scenario"]][metric].append(item[metric])

        with tempfile.TemporaryDirectory() as dirname:
            md = Path(dirname) / "report.md"
            csv_path = Path(dirname) / "report.csv"
            report.render_markdown(records, aggregates, md, "abcdef")
            report.render_csv(records, aggregates, csv_path)
            content = md.read_text()
            self.assertIn("| OpenZFS file-vdev pool | skipped | 0 | n/a", content)
            self.assertIn("ZFS module unavailable", content)
            with csv_path.open(newline="") as stream:
                rows = {row["scenario"]: row for row in csv.DictReader(stream)}
            self.assertEqual(rows["zfs-pool"]["status"], "skipped")
            self.assertEqual(rows["zfs-pool"]["samples"], "0")
            self.assertEqual(rows["zfs-pool"]["large_write_mib_s_median"], "")
            self.assertEqual(rows["ext4-loop"]["large_write_mib_s_vs_ext4"], "1.0")


if __name__ == "__main__":
    unittest.main()
