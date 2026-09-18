"""The acceptance gate uses both daily latency thresholds, not either one."""
import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location(
    "comparison", Path(__file__).resolve().parents[1] / "scripts/compare_shared_layout.py"
)
comparison = importlib.util.module_from_spec(spec)
spec.loader.exec_module(comparison)


class LatencyAcceptanceTests(unittest.TestCase):
    def test_daily_threshold_requires_both_relative_and_absolute_regression(self):
        self.assertTrue(comparison.within_latency_limit(2, 2.9))
        self.assertTrue(comparison.within_latency_limit(100, 104))
        self.assertFalse(comparison.within_latency_limit(20, 21.1))
        self.assertTrue(comparison.within_latency_limit(20, 21))

    def test_complex_cases_have_no_absolute_exception(self):
        self.assertFalse(comparison.within_latency_limit(1, 1.2, True))
        self.assertTrue(comparison.within_latency_limit(10, 11.49, True))

    def test_subset_uses_case_identity_instead_of_list_position(self):
        usage = dict.fromkeys(("resident_bytes", "physical_footprint_bytes",
                               "peak_physical_footprint_bytes", "pageins", "minor_faults",
                               "major_faults", "cpu_seconds", "disk_bytes_read",
                               "disk_bytes_written", "logical_writes"), 0)
        def record(milliseconds):
            return {"metadata_digest": "corpus", "restore_ms": 1, "restored": usage,
                    "after_queries": usage, "queries": [{"case": 19, "digest": "rows",
                        "uncached": {"samples_ms": [milliseconds]},
                        "warm": {"samples_ms": [milliseconds]}}]}
        summary = comparison.summarize_comparison(
            {"baseline": [record(1)], "candidate": [record(1.2)]}, 1, 1)
        self.assertEqual(summary["queries"][0]["case"], 19)
        self.assertFalse(summary["queries"][0]["uncached"]["passes"])


if __name__ == "__main__":
    unittest.main()
