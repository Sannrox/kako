#!/usr/bin/env python3
"""SQLLogicTest harness for the engine bar.

Parses `scripts/engine-bar/slt/*.slt`. CREATE TABLE / INSERT become CSV
inputs. Each query is wrapped as `COPY (SELECT ...) TO 'outputs/result.csv'`
so both engines can run it as a kako job. Equality follows
docs/engine-requirements.md: integers exact, floats relative 1e-6, rows
sorted when the statement has no ORDER BY. `skipif` / `onlyif` records are
unsupported, never silent skips.

Usage:
  python3 harness.py plan --work DIR
  python3 harness.py run --kako PATH --engine duckdb_sql|mizuchi|both --results DIR
"""

from __future__ import annotations

import argparse
import csv
import json
import math
import re
import shutil
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path

REL_TOL = 1e-6
ENGINES = ("duckdb_sql", "mizuchi")
ORDER_BY = re.compile(r"\bORDER\s+BY\b", re.IGNORECASE)


def corpus_dir() -> Path:
    return Path(__file__).resolve().parent


@dataclass
class Table:
    columns: list[str]
    rows: list[list[str | None]]


@dataclass
class Query:
    file: str
    index: int
    sql: str
    types: str
    sort: str
    expected: list[str]
    skipif: list[str]
    tables: dict[str, Table]


@dataclass
class Unsupported:
    file: str
    index: int
    sql: str
    engine: str
    reason: str


def parse_slt(src: str, filename: str) -> tuple[list[Query], list[Unsupported]]:
    lines = src.splitlines()
    i = 0
    index = 0
    tables: dict[str, Table] = {}
    queries: list[Query] = []
    unsupported: list[Unsupported] = []
    while i < len(lines):
        line = lines[i].strip()
        if not line or line.startswith("#"):
            i += 1
            continue
        skip_engines: list[str] = []
        only: str | None = None
        j = i
        while j < len(lines):
            t = lines[j].strip()
            if t.startswith("skipif "):
                engine = t[7:].strip()
                if engine in ENGINES:
                    skip_engines.append(engine)
                j += 1
                continue
            if t.startswith("onlyif "):
                engine = t[7:].strip()
                only = engine
                j += 1
                continue
            break
        if j >= len(lines):
            break
        head = lines[j].strip()
        if head == "halt":
            break
        if head.startswith("hash-threshold"):
            i = j + 1
            continue
        if only is not None:
            skip_engines.extend(e for e in ENGINES if e != only)
        index += 1
        if head.startswith("statement ok"):
            sql, nxt = take_sql(lines, j + 1)
            apply_statement(tables, sql)
            i = nxt
            continue
        if head.startswith("statement error"):
            sql, nxt = take_sql(lines, j + 1)
            for engine in ENGINES:
                unsupported.append(
                    Unsupported(filename, index, sql, engine, "statement error is not a kako job")
                )
            i = nxt
            continue
        if head.startswith("query "):
            rest = head[6:]
            parts = rest.split()
            types = parts[0] if parts else ""
            sort = "none"
            if len(parts) > 1 and parts[1] in ("rowsort", "valuesort"):
                sort = "row" if parts[1] == "rowsort" else "value"
            sql, after = take_sql(lines, j + 1)
            expected, nxt = take_expected(lines, after)
            snapshot = {name: clone_table(table) for name, table in tables.items()}
            if skip_engines == list(ENGINES):
                for engine in ENGINES:
                    unsupported.append(
                        Unsupported(filename, index, sql, engine, "skipif " + engine)
                    )
            else:
                queries.append(
                    Query(
                        filename,
                        index,
                        sql,
                        types,
                        sort,
                        expected,
                        skip_engines,
                        snapshot,
                    )
                )
                for engine in skip_engines:
                    unsupported.append(
                        Unsupported(filename, index, sql, engine, "skipif " + engine)
                    )
            i = nxt
            continue
        raise ValueError(f"{filename}:{j + 1}: unrecognized SLT directive: {head}")
    return queries, unsupported


def take_sql(lines: list[str], i: int) -> tuple[str, int]:
    sql: list[str] = []
    while i < len(lines):
        t = lines[i]
        if t.strip() == "----" or t.strip() == "":
            break
        sql.append(t)
        i += 1
    return "\n".join(sql), i


def take_expected(lines: list[str], i: int) -> tuple[list[str], int]:
    if i < len(lines) and lines[i].strip() == "----":
        i += 1
    expected: list[str] = []
    while i < len(lines) and lines[i].strip() != "":
        expected.append(lines[i])
        i += 1
    return expected, i


def clone_table(table: Table) -> Table:
    return Table(list(table.columns), [list(row) for row in table.rows])


def apply_statement(tables: dict[str, Table], sql: str) -> None:
    trimmed = sql.strip().rstrip(";").strip()
    upper = trimmed.upper()
    if upper.startswith("CREATE TABLE"):
        name, columns = parse_create(trimmed)
        tables[name] = Table(columns, [])
        return
    if upper.startswith("INSERT INTO"):
        name, rows = parse_insert(trimmed)
        table = tables.get(name)
        if table is None:
            raise ValueError(f"INSERT into unknown table {name}")
        table.rows.extend(rows)
        return
    raise ValueError(f"statement ok is not CREATE TABLE or INSERT: {sql}")


def parse_create(sql: str) -> tuple[str, list[str]]:
    rest = sql.split(None, 2)[2]
    name_end = rest.find("(")
    name = rest[:name_end].strip()
    inner = rest[name_end:].strip()
    if not (inner.startswith("(") and inner.endswith(")")):
        raise ValueError(f"bad CREATE TABLE: {sql}")
    columns = []
    for part in inner[1:-1].split(","):
        col = part.split()[0]
        columns.append(col)
    return name, columns


def parse_insert(sql: str) -> tuple[str, list[list[str | None]]]:
    upper = sql.upper()
    values_at = upper.find("VALUES")
    head = sql[:values_at].strip()
    name = head.split()[-1]
    return name, parse_value_rows(sql[values_at + 6 :].strip())


def parse_value_rows(src: str) -> list[list[str | None]]:
    rows: list[list[str | None]] = []
    i = 0
    n = len(src)
    while i < n:
        while i < n and src[i] in " \t\n,":
            i += 1
        if i >= n:
            break
        if src[i] != "(":
            raise ValueError(f"expected row at {src[i:]!r}")
        i += 1
        row: list[str | None] = []
        while True:
            while i < n and src[i] in " \t\n":
                i += 1
            if i >= n:
                raise ValueError("unterminated VALUES row")
            if src[i] == ")":
                i += 1
                break
            if src[i] == "'":
                i += 1
                buf: list[str] = []
                while i < n:
                    if src[i] == "'" and i + 1 < n and src[i + 1] == "'":
                        buf.append("'")
                        i += 2
                        continue
                    if src[i] == "'":
                        i += 1
                        break
                    buf.append(src[i])
                    i += 1
                row.append("".join(buf))
            else:
                start = i
                while i < n and src[i] not in ",)":
                    i += 1
                tok = src[start:i].strip()
                row.append(None if tok.upper() == "NULL" else tok)
            while i < n and src[i] in " \t\n":
                i += 1
            if i < n and src[i] == ",":
                i += 1
                continue
            if i < n and src[i] == ")":
                i += 1
                break
            raise ValueError(f"bad VALUES row at {src[i:]!r}")
        rows.append(row)
    return rows


def wrap_copy(sql: str) -> str:
    body = sql.strip().rstrip(";").strip()
    return f"COPY (\n{body}\n) TO 'outputs/result.csv' (HEADER, DELIMITER ',');"


def write_table_csv(path: Path, table: Table) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    with path.open("w", newline="") as fh:
        writer = csv.writer(fh, lineterminator="\n")
        writer.writerow(table.columns)
        for row in table.rows:
            writer.writerow(["" if cell is None else cell for cell in row])


def load_corpus(root: Path | None = None) -> tuple[list[Query], list[Unsupported]]:
    root = root or corpus_dir()
    files = sorted(p for p in root.glob("*.slt") if p.is_file())
    if not files:
        raise FileNotFoundError(f"no .slt files under {root}")
    queries: list[Query] = []
    unsupported: list[Unsupported] = []
    for path in files:
        q, u = parse_slt(path.read_text(), path.name)
        queries.extend(q)
        unsupported.extend(u)
    return queries, unsupported


def expected_rows(query: Query) -> list[list[str | None]]:
    ncols = max(len(query.types), 1)
    cells = [None if cell.strip().upper() == "NULL" else cell for cell in query.expected]
    if len(cells) % ncols != 0:
        raise ValueError(
            f"{query.file}:{query.index}: expected {len(cells)} cells not divisible by {ncols}"
        )
    rows = [cells[i : i + ncols] for i in range(0, len(cells), ncols)]
    return normalize_rows(rows, query)


def normalize_rows(rows: list[list[str | None]], query: Query) -> list[list[str | None]]:
    if query.sort == "value":
        flat = [cell for row in rows for cell in row]
        flat.sort(key=cell_key)
        ncols = max(len(query.types), 1)
        rows = [flat[i : i + ncols] for i in range(0, len(flat), ncols)]
    elif query.sort == "row" or not ORDER_BY.search(query.sql):
        rows = sorted(rows, key=lambda row: tuple(cell_key(cell) for cell in row))
    return rows


def cell_key(cell: str | None) -> tuple[int, str]:
    if cell is None:
        return (0, "")
    return (1, cell)


def read_result_csv(path: Path, ncols: int) -> list[list[str | None]]:
    with path.open(newline="") as fh:
        reader = csv.reader(fh)
        try:
            next(reader)
        except StopIteration:
            return []
        rows: list[list[str | None]] = []
        for raw in reader:
            if len(raw) < ncols:
                raw = raw + [""] * (ncols - len(raw))
            rows.append([null_cell(cell) for cell in raw[:ncols]])
        return rows


def null_cell(cell: str) -> str | None:
    text = cell.strip()
    if text == "" or text.upper() == "NULL" or text == r"\N":
        return None
    return cell


def same_cell(left: str | None, right: str | None) -> bool:
    if left is None or right is None:
        return left is None and right is None
    try:
        x, y = float(left), float(right)
    except ValueError:
        return left == right
    return math.isclose(x, y, rel_tol=REL_TOL, abs_tol=1e-9)


def same_result(got: list[list[str | None]], want: list[list[str | None]]) -> bool:
    if len(got) != len(want):
        return False
    return all(
        len(a) == len(b) and all(same_cell(x, y) for x, y in zip(a, b))
        for a, b in zip(got, want)
    )


def query_id(query: Query) -> str:
    return f"{query.file}:{query.index}"


def write_plan(work: Path, queries: list[Query]) -> dict:
    work.mkdir(parents=True, exist_ok=True)
    planned = []
    for query in queries:
        qid = query_id(query)
        folder = work / qid.replace(":", "-")
        folder.mkdir(parents=True, exist_ok=True)
        inputs = {}
        for name, table in query.tables.items():
            rel = f"{folder.name}/{name}.csv"
            write_table_csv(work / rel, table)
            inputs[name] = rel
        want = expected_rows(query)
        planned.append(
            {
                "id": qid,
                "file": query.file,
                "index": query.index,
                "sql": query.sql,
                "copy_sql": wrap_copy(query.sql),
                "types": query.types,
                "sort": query.sort,
                "sort_rows": query.sort == "row" or not ORDER_BY.search(query.sql),
                "expected_rows": want,
                "skipif": query.skipif,
                "inputs": inputs,
            }
        )
    plan = {"queries": planned}
    (work / "plan.json").write_text(json.dumps(plan, indent=2) + "\n")
    return plan


def cmd_plan(args: argparse.Namespace) -> int:
    queries, _unsupported = load_corpus()
    write_plan(Path(args.work), queries)
    return 0


def run_kako(kako: Path, job: Path, output: Path, err: Path) -> bool:
    result = subprocess.run(
        [str(kako), "run", str(job), "--output", str(output)],
        stdout=subprocess.DEVNULL,
        stderr=err.open("w"),
    )
    return result.returncode == 0


def engines_from(name: str) -> list[str]:
    if name == "both":
        return list(ENGINES)
    if name in ENGINES:
        return [name]
    raise SystemExit(f"engine must be duckdb_sql, mizuchi, or both, not {name}")


def cmd_run(args: argparse.Namespace) -> int:
    results = Path(args.results)
    results.mkdir(parents=True, exist_ok=True)
    queries, unsupported = load_corpus()
    work = results / "work"
    if work.exists():
        shutil.rmtree(work)
    plan = write_plan(work, queries)
    kako = Path(args.kako)
    summary = {
        "engines": {},
        "unsupported": [
            {
                "file": item.file,
                "index": item.index,
                "engine": item.engine,
                "reason": item.reason,
                "sql": item.sql,
            }
            for item in unsupported
        ],
    }
    fail_count = 0
    for engine in engines_from(args.engine):
        passed = 0
        failed = []
        engine_unsupported = [
            item for item in unsupported if item.engine == engine
        ]
        for item in plan["queries"]:
            if engine in item["skipif"]:
                continue
            qid = item["id"]
            job_dir = work / f"run-{engine}-{qid.replace(':', '-')}"
            job_dir.mkdir(parents=True, exist_ok=True)
            inputs = {}
            for name, rel in item["inputs"].items():
                dest = job_dir / f"{name}.csv"
                shutil.copy(work / rel, dest)
                inputs[name] = dest.name
            job = {
                "id": f"slt-{qid}-{engine}",
                "engine": engine,
                "sql": item["copy_sql"],
                "inputs": inputs,
                "outputs": ["result.csv"],
                "timeout_ms": 30000,
            }
            job_path = job_dir / "job.json"
            job_path.write_text(json.dumps(job, indent=2) + "\n")
            out = job_dir / "out"
            err = job_dir / "stderr.txt"
            if out.exists():
                shutil.rmtree(out)
            if not run_kako(kako, job_path, out, err):
                failed.append(
                    {
                        "id": qid,
                        "error": err.read_text()[-400:] if err.exists() else "kako run failed",
                    }
                )
                continue
            csv_path = out / "result.csv"
            if not csv_path.is_file():
                failed.append({"id": qid, "error": "missing result.csv"})
                continue
            query = next(q for q in queries if query_id(q) == qid)
            got = normalize_rows(
                read_result_csv(csv_path, max(len(query.types), 1)), query
            )
            want = item["expected_rows"]
            if not same_result(got, want):
                failed.append({"id": qid, "error": f"got {got!r} want {want!r}"})
                continue
            passed += 1
        summary["engines"][engine] = {
            "pass": passed,
            "fail": len(failed),
            "unsupported": len(engine_unsupported),
            "failures": failed,
        }
        fail_count += len(failed)
    (results / "slt.json").write_text(json.dumps(summary, indent=2) + "\n")
    sys.stdout.write(markdown(summary))
    return 0 if fail_count == 0 else 1


def markdown(summary: dict) -> str:
    lines = [
        "### sqllogictest",
        "",
        "| engine | pass | fail | unsupported |",
        "| --- | ---: | ---: | ---: |",
    ]
    for engine, row in summary["engines"].items():
        lines.append(
            f"| {engine} | {row['pass']} | {row['fail']} | {row['unsupported']} |"
        )
    fails = sum(row["fail"] for row in summary["engines"].values())
    lines += [
        "",
        "| bar | result |",
        "| --- | --- |",
        f"| sqllogictest | {'pass' if fails == 0 else 'fail'} |",
    ]
    notes = []
    for item in summary["unsupported"]:
        notes.append(
            f"unsupported {item['engine']}: {item['file']} record {item['index']}: {item['reason']}"
        )
    for engine, row in summary["engines"].items():
        for failure in row["failures"]:
            notes.append(f"fail {engine}: {failure['id']}: {failure['error']}")
    if notes:
        lines.append("")
        lines.extend(f"- {note}" for note in notes)
    return "\n".join(lines) + "\n"


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="cmd", required=True)
    plan = sub.add_parser("plan")
    plan.add_argument("--work", required=True)
    run = sub.add_parser("run")
    run.add_argument("--kako", required=True)
    run.add_argument("--engine", required=True)
    run.add_argument("--results", required=True)
    args = parser.parse_args(argv)
    if args.cmd == "plan":
        return cmd_plan(args)
    return cmd_run(args)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except Exception as error:  # noqa: BLE001 — CLI fail-closed
        print(f"slt: {error}", file=sys.stderr)
        sys.exit(1)
