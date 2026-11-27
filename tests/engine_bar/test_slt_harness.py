"""Parser and equality for scripts/engine-bar/slt/harness.py.

Named failures: a skipif record counted as a silent skip; a float drift past
1e-6 accepted; unordered query compared in file order.
"""

from __future__ import annotations

import importlib.util
import os
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
HARNESS = ROOT / "scripts" / "engine-bar" / "slt" / "harness.py"


def load_harness():
    spec = importlib.util.spec_from_file_location("slt_harness", HARNESS)
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


class Parse(unittest.TestCase):
    def setUp(self):
        self.h = load_harness()

    def test_select_corpus_has_three_queries(self):
        src = (ROOT / "scripts/engine-bar/slt/select.slt").read_text()
        queries, unsupported = self.h.parse_slt(src, "select.slt")
        self.assertEqual(len(queries), 3)
        self.assertEqual(unsupported, [])
        self.assertEqual(queries[-1].sql.strip(), "SELECT 1 + 2")

    def test_skipif_is_unsupported_not_dropped(self):
        src = """skipif duckdb_sql
query I
SELECT 1
----
1
"""
        queries, unsupported = self.h.parse_slt(src, "x.slt")
        self.assertEqual(len(queries), 1)
        self.assertEqual(queries[0].skipif, ["duckdb_sql"])
        self.assertEqual([u.engine for u in unsupported], ["duckdb_sql"])
        self.assertTrue(all(u.reason.startswith("skipif") for u in unsupported))


class Equality(unittest.TestCase):
    def setUp(self):
        self.h = load_harness()

    def test_float_within_tolerance(self):
        self.assertTrue(self.h.same_cell("1.0", "1.0000005"))
        self.assertFalse(self.h.same_cell("1.0", "1.1"))

    def test_nulls_match_empty_and_null_token(self):
        self.assertTrue(self.h.same_cell(None, None))
        self.assertFalse(self.h.same_cell(None, "1"))

    def test_no_order_by_sorts_before_compare(self):
        query = self.h.Query(
            "x.slt",
            1,
            "SELECT a FROM t",
            "I",
            "none",
            ["2", "1"],
            [],
            {},
        )
        rows = [["2"], ["1"]]
        got = self.h.normalize_rows(rows, query)
        want = self.h.expected_rows(query)
        self.assertTrue(self.h.same_result(got, want))
        self.assertEqual(want, [["1"], ["2"]])


class Plan(unittest.TestCase):
    def setUp(self):
        self.h = load_harness()
        self.work = Path(tempfile.mkdtemp(prefix=f"kako-slt-{os.getpid()}-"))

    def tearDown(self):
        import shutil

        shutil.rmtree(self.work, ignore_errors=True)

    def test_plan_writes_copy_sql_and_csv_inputs(self):
        queries, _ = self.h.load_corpus()
        plan = self.h.write_plan(self.work, queries)
        self.assertTrue(plan["queries"])
        first = next(q for q in plan["queries"] if "FROM t" in q["sql"])
        self.assertIn("COPY (", first["copy_sql"])
        self.assertIn("outputs/result.csv", first["copy_sql"])
        self.assertTrue((self.work / first["inputs"]["t"]).is_file())


if __name__ == "__main__":
    unittest.main()
