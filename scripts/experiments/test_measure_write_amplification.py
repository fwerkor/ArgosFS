#!/usr/bin/env python3
"""Pure parser and ratio regression tests for the live write-amplification sampler."""

import unittest
from measure_write_amplification import deltas, parse_diskstats, parse_metrics


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

    def test_counter_reset_fails_closed(self):
        self.assertEqual(deltas({"x": 10, "y": 20}, {"x": 11, "y": 21}), {"x": 1, "y": 1})
        with self.assertRaisesRegex(ValueError, "decreased"):
            deltas({"x": 10}, {"x": 9})


if __name__ == "__main__":
    unittest.main()
