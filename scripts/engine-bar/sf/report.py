#!/usr/bin/env python3
"""Score scale-factor engine-bar runs against docs/engine-requirements.md.

Usage: report.py <results-dir>...

Each directory is one run.sh output. Prints a Markdown section per scale and
writes <results-dir>/summary.json. Exits 0 even when a bar fails: the report
records the verdict, it does not gate CI.
"""

import csv
import json
import math
import sys
from pathlib import Path

CANDIDATE = "mizuchi"
CURRENT = "duckdb_sql"
REL_TOL = 1e-6
GEO_MEAN_MAX = 2.0
QUERY_MAX = 5.0


def read_csv(path):
    with open(path, newline="") as f:
        return list(csv.reader(f))


def same_cell(a, b):
    try:
        x, y = float(a), float(b)
    except ValueError:
        return a == b
    return math.isclose(x, y, rel_tol=REL_TOL, abs_tol=1e-9)


def same_result(left, right):
    """Every query orders its rows, so rows compare in order."""
    if len(left) != len(right):
        return False
    return all(
        len(a) == len(b) and all(same_cell(x, y) for x, y in zip(a, b))
        for a, b in zip(left, right)
    )


def mib(value):
    return "" if value is None else f"{value / 1048576:.0f}"


def score(results):
    host = json.loads((results / "host.json").read_text())
    runs = [json.loads(line) for line in (results / "runs.jsonl").read_text().splitlines()]
    by = {(run["query"], run["engine"]): run for run in runs}
    rows, ratios, failures, mismatches = [], [], [], []
    for query in sorted({run["query"] for run in runs}):
        current, candidate = by.get((query, CURRENT)), by.get((query, CANDIDATE))
        ok = [r for r in (current, candidate) if r and r["status"] == "ok"]
        failures += [f"{query}/{r['engine']}" for r in (current, candidate) if r and r["status"] != "ok"]
        match = None
        ratio = None
        if len(ok) == 2:
            match = same_result(
                read_csv(results / f"{query}-{CURRENT}.csv"),
                read_csv(results / f"{query}-{CANDIDATE}.csv"),
            )
            if not match:
                mismatches.append(query)
            ratio = max(candidate["duration_ms"], 1) / max(current["duration_ms"], 1)
            ratios.append(ratio)
        rows.append(
            {
                "query": query,
                "current_ms": current.get("duration_ms") if current else None,
                "candidate_ms": candidate.get("duration_ms") if candidate else None,
                "ratio": ratio,
                "current_peak_bytes": current.get("memory_peak_bytes") if current else None,
                "candidate_peak_bytes": candidate.get("memory_peak_bytes") if candidate else None,
                "match": match,
                "current_status": current["status"] if current else "missing",
                "candidate_status": candidate["status"] if candidate else "missing",
            }
        )
    geo_mean = math.exp(sum(map(math.log, ratios)) / len(ratios)) if ratios else None
    verdict = {
        "correctness": not failures and not mismatches,
        "performance": bool(ratios)
        and len(ratios) == len(rows)
        and geo_mean <= GEO_MEAN_MAX
        and max(ratios) <= QUERY_MAX,
        "resource": not failures,
    }
    versions = sorted({run["engine_version"] for run in runs if run.get("engine_version")})
    return {
        "scale": host["scale"],
        "host": host,
        "engine_versions": versions,
        "queries": rows,
        "geo_mean_ratio": geo_mean,
        "max_ratio": max(ratios) if ratios else None,
        "failures": failures,
        "mismatches": mismatches,
        "verdict": verdict,
    }


def markdown(summary):
    host = summary["host"]
    lines = [
        f"### Scale factor {summary['scale']}",
        "",
        f"Host: {host['cpus']} CPUs, {host['memory_bytes_host'] / 1073741824:.1f} GiB, "
        f"{host['kernel']}. Engines: {', '.join(summary['engine_versions'])}.",
        "",
        "| query | duckdb ms | mizuchi ms | ratio | duckdb peak MiB | mizuchi peak MiB | results match |",
        "| --- | ---: | ---: | ---: | ---: | ---: | --- |",
    ]
    for row in summary["queries"]:
        cur = row["current_ms"] if row["current_status"] == "ok" else row["current_status"]
        cand = row["candidate_ms"] if row["candidate_status"] == "ok" else row["candidate_status"]
        ratio = "" if row["ratio"] is None else f"{row['ratio']:.2f}"
        match = {True: "yes", False: "no", None: "n/a"}[row["match"]]
        lines.append(
            f"| {row['query']} | {cur} | {cand} | {ratio} | "
            f"{mib(row['current_peak_bytes'])} | {mib(row['candidate_peak_bytes'])} | {match} |"
        )
    geo = summary["geo_mean_ratio"]
    verdict = summary["verdict"]
    lines += [
        "",
        f"Geometric mean ratio (mizuchi / duckdb): {'n/a' if geo is None else f'{geo:.2f}'}"
        f" (bar {GEO_MEAN_MAX}); worst query: "
        f"{'n/a' if summary['max_ratio'] is None else f'{summary['max_ratio']:.2f}'} (bar {QUERY_MAX}).",
        "",
        "| bar | result |",
        "| --- | --- |",
    ]
    lines += [f"| {name} | {'pass' if passed else 'fail'} |" for name, passed in verdict.items()]
    return "\n".join(lines) + "\n"


def main(dirs):
    if not dirs:
        sys.exit(__doc__)
    for raw in dirs:
        results = Path(raw)
        summary = score(results)
        (results / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
        print(markdown(summary))


if __name__ == "__main__":
    main(sys.argv[1:])
