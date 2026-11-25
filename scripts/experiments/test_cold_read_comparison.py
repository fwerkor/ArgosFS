import json
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from run_cold_read_comparison import measure, run_comparison, validate


def config():
    command = [sys.executable, "-c", "pass"]
    return {"cases": [
        {"filesystem": "argosfs", "cache_state": "cleared-l2", "prepare": [command],
         "verify": command, "cleanup": [command]},
        {"filesystem": "zfs", "cache_state": "empty-arc", "prepare": [command],
         "verify": command, "cleanup": [command]},
    ]}


class ColdReadComparisonTests(unittest.TestCase):
    def test_interleaved_and_recorded(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "results.jsonl"
            run_comparison(config(), 2, path)
            rows = [json.loads(line) for line in path.read_text().splitlines()]
            self.assertEqual([row["filesystem"] for row in rows],
                             ["argosfs", "zfs", "zfs", "argosfs"])
            self.assertTrue(all(row["status"] == "ok" for row in rows))
            self.assertTrue(all(row["wall_sec"] >= 0 for row in rows))
            with self.assertRaises(FileExistsError):
                run_comparison(config(), 2, path)

    def test_rejects_warm_l2_as_cold(self):
        value = config()
        value["cases"][0]["cache_state"] = "warm-l2"
        with self.assertRaises(ValueError):
            validate(value)

    def test_failed_prepare_does_not_time_verify_and_runs_cleanup(self):
        case = config()["cases"][0]
        case["prepare"] = [[sys.executable, "-c", "raise SystemExit(7)"]]
        with patch("run_cold_read_comparison.hooks", wraps=__import__(
                "run_cold_read_comparison").hooks) as calls:
            result = measure(case, 1)
        self.assertEqual(result["status"], "failed")
        self.assertNotIn("wall_sec", result)
        self.assertEqual(calls.call_count, 2)


if __name__ == "__main__":
    unittest.main()
