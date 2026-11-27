"""Median, spread, and previous-run delta for the scale-factor engine bar."""

import importlib.util
import io
import json
import os
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def load_report():
    spec = importlib.util.spec_from_file_location(
        "engine_bar_report",
        ROOT / "scripts" / "engine-bar" / "sf" / "report.py",
    )
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def scratch():
    return Path(tempfile.mkdtemp(prefix=f"engine-bar-report.{os.getpid()}."))


def write_results(directory, scale, rows, host_extra=None):
    host = {
        "scale": scale,
        "cpus": 4,
        "memory_bytes_host": 16765374464,
        "job_memory_bytes": 4294967296,
        "engine_memory_bytes": 268435456,
        "job_timeout_ms": 1800000,
    }
    if host_extra:
        host.update(host_extra)
    directory.mkdir(parents=True, exist_ok=True)
    (directory / "host.json").write_text(json.dumps(host) + "\n")
    (directory / "runs.jsonl").write_text("".join(json.dumps(row) + "\n" for row in rows))


def ok(query, engine, duration_ms, peak, trial=None, version=None):
    row = {
        "scale": "1",
        "query": query,
        "engine": engine,
        "status": "ok",
        "engine_version": version or ("duckdb 1.5.6" if engine == "duckdb_sql" else "mizuchi 0.1.0"),
        "duration_ms": duration_ms,
        "memory_peak_bytes": peak,
    }
    if trial is not None:
        row["trial"] = trial
    return row


def failed(query, engine, error, trial=None):
    row = {
        "scale": "1",
        "query": query,
        "engine": engine,
        "status": "failed",
        "error": error,
    }
    if trial is not None:
        row["trial"] = trial
    return row


class MedianOfThree(unittest.TestCase):
    def setUp(self):
        self.report = load_report()
        self.dir = scratch()

    def test_median_duration_max_peak_and_min_max_spread(self):
        # Outlier 900 must not become the wall clock; peak 300 must win over 100.
        write_results(
            self.dir,
            "1",
            [
                ok("q", "duckdb_sql", 100, 10, 1),
                ok("q", "duckdb_sql", 120, 12, 2),
                ok("q", "duckdb_sql", 110, 11, 3),
                ok("q", "mizuchi", 200, 100, 1),
                ok("q", "mizuchi", 900, 300, 2),
                ok("q", "mizuchi", 220, 110, 3),
            ],
        )
        (self.dir / "q-duckdb_sql.csv").write_text("a\n1\n")
        (self.dir / "q-mizuchi.csv").write_text("a\n1\n")
        summary = self.report.score(self.dir)
        [row] = summary["queries"]
        self.assertEqual(row["current_ms"], 110)
        self.assertEqual(row["candidate_ms"], 220)
        self.assertEqual(row["current_peak_bytes"], 12)
        self.assertEqual(row["candidate_peak_bytes"], 300)
        self.assertEqual(row["current_ms_min"], 100)
        self.assertEqual(row["current_ms_max"], 120)
        self.assertEqual(row["candidate_ms_min"], 200)
        self.assertEqual(row["candidate_ms_max"], 900)
        self.assertAlmostEqual(row["ratio"], 2.0)
        text = self.report.markdown(summary)
        self.assertIn("| q | 110 (100-120) | 220 (200-900) | 2.00 |", text)
        self.assertIn("Trials per query: 3.", text)

    def test_one_failed_trial_is_incomplete_and_drops_the_ratio(self):
        write_results(
            self.dir,
            "1",
            [
                ok("q", "duckdb_sql", 100, 10, 1),
                ok("q", "duckdb_sql", 110, 11, 2),
                ok("q", "duckdb_sql", 120, 12, 3),
                ok("q", "mizuchi", 200, 100, 1),
                failed("q", "mizuchi", "limit: memory: this aggregate cannot spill", 2),
                ok("q", "mizuchi", 220, 110, 3),
            ],
        )
        summary = self.report.score(self.dir, require_outputs=False)
        [row] = summary["queries"]
        self.assertEqual(row["candidate_status"], "failed")
        self.assertIsNone(row["ratio"])
        self.assertFalse(summary["verdict"]["correctness"])
        self.assertFalse(summary["verdict"]["performance"])
        self.assertIn("q/mizuchi", summary["failures"])
        self.assertTrue(summary["verdict"]["resource"])

    def test_legacy_single_sample_keeps_the_recorded_sf1_geo_mean(self):
        snapshot = scratch()
        snapshot_runs = ROOT / "docs" / "engine-bar" / "sf1-runs.jsonl"
        snapshot_host = ROOT / "docs" / "engine-bar" / "sf1-host.json"
        (snapshot / "runs.jsonl").write_text(snapshot_runs.read_text())
        (snapshot / "host.json").write_text(snapshot_host.read_text())
        summary = self.report.score(snapshot, require_outputs=False)
        self.assertAlmostEqual(summary["geo_mean_ratio"], 4.65, places=2)
        self.assertAlmostEqual(summary["max_ratio"], 6.24, places=2)
        self.assertFalse(summary["verdict"]["correctness"])
        self.assertFalse(summary["verdict"]["performance"])
        self.assertTrue(summary["verdict"]["resource"])
        text = self.report.markdown(summary)
        self.assertIn("| tpcds_store_sales | 170 | 537 | 3.16 |", text)
        self.assertNotIn("(170-170)", text)

    def test_previous_delta_names_geo_mean_change_and_each_bar(self):
        current = scratch()
        previous = scratch()
        write_results(
            previous,
            "1",
            [
                ok("q", "duckdb_sql", 100, 10),
                ok("q", "mizuchi", 200, 100),
            ],
        )
        write_results(
            current,
            "1",
            [
                ok("q", "duckdb_sql", 100, 10, 1),
                ok("q", "duckdb_sql", 100, 10, 2),
                ok("q", "duckdb_sql", 100, 10, 3),
                ok("q", "mizuchi", 300, 100, 1),
                ok("q", "mizuchi", 300, 100, 2),
                ok("q", "mizuchi", 300, 100, 3),
            ],
        )
        (current / "q-duckdb_sql.csv").write_text("a\n1\n")
        (current / "q-mizuchi.csv").write_text("a\n1\n")
        summary = self.report.score(current)
        earlier = self.report.score(previous, require_outputs=False)
        summary["previous"] = {
            "geo_mean_ratio": earlier["geo_mean_ratio"],
            "max_ratio": earlier["max_ratio"],
            "verdict": earlier["verdict"],
        }
        text = self.report.markdown(summary)
        self.assertIn("Vs previous: geometric mean 2.00 -> 3.00 (delta +1.00)", text)
        self.assertIn("correctness pass->pass", text)
        self.assertIn("performance pass->fail", text)
        self.assertIn("resource pass->pass", text)

    def test_cli_writes_previous_into_summary_json(self):
        current = scratch()
        previous = scratch()
        write_results(
            previous,
            "1",
            [
                ok("q", "duckdb_sql", 100, 10),
                ok("q", "mizuchi", 200, 100),
            ],
        )
        write_results(
            current,
            "1",
            [
                ok("q", "duckdb_sql", 100, 10),
                ok("q", "mizuchi", 200, 100),
            ],
        )
        (current / "q-duckdb_sql.csv").write_text("a\n1\n")
        (current / "q-mizuchi.csv").write_text("a\n1\n")
        with redirect_stdout(io.StringIO()):
            self.report.main(["--previous", str(previous), str(current)])
        summary = json.loads((current / "summary.json").read_text())
        self.assertAlmostEqual(summary["previous"]["geo_mean_ratio"], 2.0)
        self.assertEqual(summary["previous"]["verdict"]["performance"], True)


if __name__ == "__main__":
    unittest.main()
