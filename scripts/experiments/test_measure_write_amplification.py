#!/usr/bin/env python3
"""Pure parser and ratio regression tests for the live write-amplification sampler."""

import io
import json
import subprocess
import unittest
from unittest.mock import patch
from measure_write_amplification import deltas, main, parse_diskstats, parse_metrics


class SamplerTests(unittest.TestCase):
    def test_parse_metric_family_and_reject_incomplete_scrapes(self):
        raw = "\n".join(
            f'argosfs_data_plane_bytes_total{{kind="{kind}"}} {i * 1024}'
            for i, kind in enumerate((
                "range_accepted", "range_aligned_append", "range_rewrite_input", "shard_payload_written"
            ), 1)
        )
        self.assertEqual(parse_metrics(raw)["range_rewrite_input"], 3072)
        with self.assertRaisesRegex(ValueError, "missing"):
            parse_metrics(raw.replace('kind="shard_payload_written"', 'kind="other"'))
        with self.assertRaisesRegex(ValueError, "invalid"):
            parse_metrics(raw.replace(' 2048', ' nan'))

    def test_diskstats_uses_sectors_written_and_requires_all_disks(self):
        raw = " 8 16 sdb 11 1 20 3 12 3 40 5 0 2 7\n 8 32 sdc 0 0 0 0 2 0 10 8 0 0 0\n"
        self.assertEqual(parse_diskstats(raw, {"sdb", "sdc"}), {"sdb": 20480, "sdc": 5120})
        with self.assertRaisesRegex(ValueError, "absent"):
            parse_diskstats(raw, {"sda"})

    def test_signal_exit_uses_shell_status_but_reports_original_signal(self):
        counters = {
            "range_accepted": 1,
            "range_aligned_append": 1,
            "range_rewrite_input": 0,
            "shard_payload_written": 2,
        }
        with (
            patch("measure_write_amplification.fetch_metrics", side_effect=[counters, counters]),
            patch("measure_write_amplification.diskstats_snapshot", side_effect=[{"sdx": 1}, {"sdx": 1}]),
            patch("measure_write_amplification.subprocess.run", return_value=subprocess.CompletedProcess(["fake"], -15)),
            patch("sys.stdout", new_callable=io.StringIO) as output,
        ):
            status = main(["--endpoint", "http://localhost:9100/commit-metrics", "--device", "sdx", "--", "fake"])
        self.assertEqual(status, 143)
        report = json.loads(output.getvalue())
        self.assertEqual(report["exit_code"], -15)
        self.assertEqual(report["diskstats_written_total_bytes"], 0)

    def test_workload_output_cannot_contaminate_json_stdout(self):
        counters = {
            "range_accepted": 100,
            "range_aligned_append": 100,
            "range_rewrite_input": 0,
            "shard_payload_written": 200,
        }
        captured = {}

        def simulated_workload(command, *, stdout, check):
            print("workload progress", file=stdout)
            captured["command"] = command
            captured["check"] = check
            return subprocess.CompletedProcess(command, 0)

        with (
            patch("measure_write_amplification.fetch_metrics", side_effect=[counters, counters]),
            patch("measure_write_amplification.diskstats_snapshot", side_effect=[{"sdx": 9}, {"sdx": 9}]),
            patch("measure_write_amplification.subprocess.run", side_effect=simulated_workload),
            patch("sys.stdout", new_callable=io.StringIO) as report_stdout,
            patch("sys.stderr", new_callable=io.StringIO) as log_stderr,
        ):
            status = main(["--endpoint", "http://localhost:9100/commit-metrics", "--device", "sdx", "--", "echo", "hello"])
            self.assertEqual(status, 0)
            self.assertEqual(captured["command"], ["echo", "hello"])
            self.assertFalse(captured["check"])
            report = json.loads(report_stdout.getvalue())
            self.assertEqual(report["exit_code"], 0)
            self.assertIn("workload progress", log_stderr.getvalue())
            self.assertNotIn("workload progress", report_stdout.getvalue())

    def test_counter_reset_fails_closed(self):
        self.assertEqual(deltas({"x": 10, "y": 20}, {"x": 11, "y": 21}), {"x": 1, "y": 1})
        with self.assertRaisesRegex(ValueError, "decreased"):
            deltas({"x": 10}, {"x": 9})


if __name__ == "__main__":
    unittest.main()
