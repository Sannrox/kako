#!/usr/bin/env python3
"""Score scale-factor engine-bar runs against docs/engine-requirements.md.

Usage: report.py [--previous DIR] <results-dir>...

Each directory is one run.sh output. Prints a Markdown section per scale and
writes <results-dir>/summary.json. Wall clock is the median of the trials in
runs.jsonl; peak memory is the maximum; spread is min-max. --previous DIR
scores an earlier run.sh output (or a committed snapshot copied into that
layout) and adds the delta to the report. Exits 0 even when a bar fails: the
report records the verdict, it does not gate CI.
"""

import argparse
import csv
import json
import math
import statistics
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


def typed_limit(error):
    """A clean `Failure::Limit` from the engine, not a kill or a crash."""
    return "limit: memory:" in error and "oom" not in error and "signal" not in error


def failure_kind(error):
    if "oom" in error:
        return "killed by the job memory cap"
    return "failed without a typed limit"


def mib(value):
    return "" if value is None else f"{value / 1048576:.0f}"


def unique(items):
    seen = set()
    out = []
    for item in items:
        if item not in seen:
            seen.add(item)
            out.append(item)
    return out


def group_trials(runs):
    grouped = {}
    for run in runs:
        grouped.setdefault((run["query"], run["engine"]), []).append(run)
    return grouped


def summarize_engine(trials):
    """Collapse trials for one query and engine.

    Status is ok only when every trial succeeded. Duration is the median of
    the successful trials; peak is their maximum. Spread is min-max.
    """
    if not trials:
        return {
            "status": "missing",
            "n": 0,
            "n_ok": 0,
            "duration_ms": None,
            "duration_min": None,
            "duration_max": None,
            "memory_peak_bytes": None,
            "engine_version": None,
            "error": "",
        }
    ok = [trial for trial in trials if trial["status"] == "ok"]
    failed = [trial for trial in trials if trial["status"] != "ok"]
    durations = [trial["duration_ms"] for trial in ok if trial.get("duration_ms") is not None]
    peaks = [trial["memory_peak_bytes"] for trial in ok if trial.get("memory_peak_bytes") is not None]
    return {
        "status": "ok" if ok and not failed else ("failed" if failed else "missing"),
        "n": len(trials),
        "n_ok": len(ok),
        "duration_ms": statistics.median(durations) if durations else None,
        "duration_min": min(durations) if durations else None,
        "duration_max": max(durations) if durations else None,
        "memory_peak_bytes": max(peaks) if peaks else None,
        "engine_version": next(
            (trial.get("engine_version") for trial in ok if trial.get("engine_version")),
            None,
        ),
        "error": failed[-1].get("error", "") if failed else "",
    }


def fmt_ms(value):
    if value is None:
        return ""
    if isinstance(value, float) and not value.is_integer():
        return f"{value:.1f}"
    return str(int(value))


def fmt_ms_cell(status, median, low, high):
    if status != "ok":
        return status
    if median is None:
        return "missing"
    if low is None or high is None or low == high:
        return fmt_ms(median)
    return f"{fmt_ms(median)} ({fmt_ms(low)}-{fmt_ms(high)})"


def score(results, require_outputs=True):
    host = json.loads((results / "host.json").read_text())
    runs = [json.loads(line) for line in (results / "runs.jsonl").read_text().splitlines() if line]
    grouped = group_trials(runs)
    queries = sorted({run["query"] for run in runs})
    rows, ratios, failures, mismatches = [], [], [], []
    for query in queries:
        current = summarize_engine(grouped.get((query, CURRENT), []))
        candidate = summarize_engine(grouped.get((query, CANDIDATE), []))
        if current["status"] != "ok":
            failures.append(f"{query}/{CURRENT}")
        if candidate["status"] != "ok":
            failures.append(f"{query}/{CANDIDATE}")
        match = None
        ratio = None
        if current["status"] == "ok" and candidate["status"] == "ok":
            left = results / f"{query}-{CURRENT}.csv"
            right = results / f"{query}-{CANDIDATE}.csv"
            if left.exists() and right.exists():
                match = same_result(read_csv(left), read_csv(right))
                if not match:
                    mismatches.append(query)
            elif require_outputs:
                match = False
                mismatches.append(query)
            ratio = max(candidate["duration_ms"], 1) / max(current["duration_ms"], 1)
            ratios.append(ratio)
        rows.append(
            {
                "query": query,
                "n": max(current["n"], candidate["n"]),
                "current_ms": current["duration_ms"],
                "current_ms_min": current["duration_min"],
                "current_ms_max": current["duration_max"],
                "candidate_ms": candidate["duration_ms"],
                "candidate_ms_min": candidate["duration_min"],
                "candidate_ms_max": candidate["duration_max"],
                "ratio": ratio,
                "current_peak_bytes": current["memory_peak_bytes"],
                "candidate_peak_bytes": candidate["memory_peak_bytes"],
                "match": match,
                "current_status": current["status"],
                "candidate_status": candidate["status"],
            }
        )
    # A typed `limit:` failure is allowed by the resource bar; a cgroup kill,
    # an untyped crash or a peak above the job cap is not. The peak is the
    # whole worker cgroup, so it is compared with the job cap; the engine's
    # internal cap needs engine telemetry this harness does not have.
    cap = host.get("job_memory_bytes")
    resource_failures = unique(
        [
            f"{run['query']}/{run['engine']}: {failure_kind(run.get('error', ''))}"
            for run in runs
            if run["status"] != "ok" and not typed_limit(run.get("error", ""))
        ]
        + [
            f"{run['query']}/{run['engine']}: peak {mib(run['memory_peak_bytes'])} MiB "
            f"over the {mib(cap)} MiB job cap"
            for run in runs
            if cap and run["status"] == "ok" and (run.get("memory_peak_bytes") or 0) > cap
        ]
    )
    geo_mean = math.exp(sum(map(math.log, ratios)) / len(ratios)) if ratios else None
    verdict = {
        "correctness": not failures and not mismatches,
        "performance": bool(ratios)
        and len(ratios) == len(rows)
        and geo_mean <= GEO_MEAN_MAX
        and max(ratios) <= QUERY_MAX,
        "resource": not resource_failures,
    }
    versions = sorted(
        {
            run["engine_version"]
            for run in runs
            if run.get("engine_version")
        }
    )
    return {
        "scale": host["scale"],
        "host": host,
        "engine_versions": versions,
        "queries": rows,
        "geo_mean_ratio": geo_mean,
        "max_ratio": max(ratios) if ratios else None,
        "failures": failures,
        "mismatches": mismatches,
        "resource_failures": resource_failures,
        "verdict": verdict,
    }


def vs_previous_line(current, previous):
    def fmt(value):
        return "n/a" if value is None else f"{value:.2f}"

    def bar(name):
        now = "pass" if current["verdict"][name] else "fail"
        then = "pass" if previous["verdict"][name] else "fail"
        return f"{name} {then}->{now}"

    geo_from, geo_to = previous["geo_mean_ratio"], current["geo_mean_ratio"]
    delta = None if geo_from is None or geo_to is None else geo_to - geo_from
    delta_s = "" if delta is None else f" (delta {delta:+.2f})"
    return (
        f"Vs previous: geometric mean {fmt(geo_from)} -> {fmt(geo_to)}{delta_s}; "
        f"worst query {fmt(previous['max_ratio'])} -> {fmt(current['max_ratio'])}. "
        f"Bars: {', '.join(bar(name) for name in current['verdict'])}."
    )


def markdown(summary):
    host = summary["host"]
    n = max((row["n"] for row in summary["queries"]), default=0)
    lines = [
        f"### Scale factor {summary['scale']}",
        "",
        f"Host: {host['cpus']} CPUs, {host['memory_bytes_host'] / 1073741824:.1f} GiB. "
        f"Engines: {', '.join(summary['engine_versions'])}. "
        f"Trials per query: {n}. Wall clock is the median; peak memory is the maximum; "
        f"spread is min-max.",
        "",
        "| query | duckdb ms | mizuchi ms | ratio | duckdb peak MiB | mizuchi peak MiB | results match |",
        "| --- | ---: | ---: | ---: | ---: | ---: | --- |",
    ]
    for row in summary["queries"]:
        cur = fmt_ms_cell(
            row["current_status"],
            row["current_ms"],
            row["current_ms_min"],
            row["current_ms_max"],
        )
        cand = fmt_ms_cell(
            row["candidate_status"],
            row["candidate_ms"],
            row["candidate_ms_min"],
            row["candidate_ms_max"],
        )
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
    lines.append("| engine-internal memory cap | not measured |")
    notes = [f"incomplete: {name}" for name in summary["failures"]]
    notes += [f"mismatch: {name}" for name in summary["mismatches"]]
    notes += summary["resource_failures"]
    if notes:
        lines += [""] + [f"- {note}" for note in notes]
    if summary.get("previous"):
        lines += ["", vs_previous_line(summary, summary["previous"])]
    return "\n".join(lines) + "\n"


def parse_args(argv):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument(
        "--previous",
        metavar="DIR",
        help="earlier run.sh output to compare (committed snapshot or last weekly dir)",
    )
    parser.add_argument("dirs", nargs="+", help="run.sh output directories")
    return parser.parse_args(argv)


def main(argv):
    args = parse_args(argv)
    previous = None
    if args.previous:
        previous = score(Path(args.previous), require_outputs=False)
    for raw in args.dirs:
        results = Path(raw)
        summary = score(results)
        if previous is not None:
            summary["previous"] = {
                "geo_mean_ratio": previous["geo_mean_ratio"],
                "max_ratio": previous["max_ratio"],
                "verdict": previous["verdict"],
            }
        (results / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
        print(markdown(summary))


if __name__ == "__main__":
    main(sys.argv[1:])
