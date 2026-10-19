# Engine bar results: mizuchi vs DuckDB at SF1 and SF10

Measured against [the engine requirements](../engine-requirements.md) with
the scale-factor harness in `scripts/engine-bar/sf/`. Raw receipts per query
and engine are in `sf1-runs.jsonl` and `sf10-runs.jsonl`; the host of each run
is in `sf1-host.json` and `sf10-host.json`.

## Method

- Data: `generate.sql` at generator scale 125,000 (SF1: 6M `lineitem` rows)
  and 1,250,000 (SF10: 60M rows), the same deterministic decision-support
  tables as the mizuchi suite, written as Parquet by a default-engine job.
- Queries: the four join and aggregate queries in `queries/` (TPC-H q1, q3,
  q6 style and a TPC-DS store-sales style join), each with a full `ORDER BY`
  so results compare row by row.
- Execution: every query runs once per engine through `kako run` under
  kekkai `linux_native`, with the same limits: a 4 GiB cgroup memory cap and a
  30 minute wall clock per job. Each engine runs one worker with its internal
  256 MiB memory cap.
- Wall clock is the receipt `duration_ms`; peak memory is the receipt
  `memory_peak_bytes`. Ratio is mizuchi / DuckDB.
- Correctness compares cells exactly, numbers with relative tolerance 1e-6.
- Host: hosted CI runner, 4 CPUs, 15.6 GiB. One run per query; the numbers
  are a single sample, not a median.

Reproduce with `bash scripts/engine-bar/sf/run.sh <1|10> <dir>` and
`python3 scripts/engine-bar/sf/report.py <dir>`. The `Engine bar` workflow
reruns both scales weekly and on demand and writes the report to the job
summary.

## Results

### Scale factor 1

Host: 4 CPUs, 15.6 GiB. Engines: duckdb 1.5.6, mizuchi 0.1.0.

| query | duckdb ms | mizuchi ms | ratio | duckdb peak MiB | mizuchi peak MiB | results match |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| tpcds_store_sales | 170 | 537 | 3.16 | 49 | 243 | yes |
| tpch_q1 | 305 | failed |  | 9 |  | n/a |
| tpch_q3 | 387 | 1975 | 5.10 | 32 | 761 | yes |
| tpch_q6 | 205 | 1280 | 6.24 | 7 | 394 | yes |

Geometric mean ratio (mizuchi / duckdb): 4.65 (bar 2.0); worst query: 6.24 (bar 5.0).

| bar | result |
| --- | --- |
| correctness | fail |
| performance | fail |
| resource | pass |
| engine-internal memory cap | not measured |

- incomplete: tpch_q1/mizuchi

### Scale factor 10

Host: 4 CPUs, 15.6 GiB. Engines: duckdb 1.5.6, mizuchi 0.1.0.

| query | duckdb ms | mizuchi ms | ratio | duckdb peak MiB | mizuchi peak MiB | results match |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| tpcds_store_sales | 2403 | 29335 | 12.21 | 359 | 4096 | yes |
| tpch_q1 | 2446 | failed |  | 12 |  | n/a |
| tpch_q3 | 3345 | failed |  | 159 |  | n/a |
| tpch_q6 | 1410 | 16396 | 11.63 | 9 | 3793 | yes |

Geometric mean ratio (mizuchi / duckdb): 11.91 (bar 2.0); worst query: 12.21 (bar 5.0).

| bar | result |
| --- | --- |
| correctness | fail |
| performance | fail |
| resource | fail |
| engine-internal memory cap | not measured |

- incomplete: tpch_q1/mizuchi
- incomplete: tpch_q3/mizuchi
- tpch_q3/mizuchi: killed by the job memory cap

Failures and limits:

- `tpch_q1` on mizuchi stops at both scales with the typed failure
  `limit: memory: this aggregate cannot spill`. The resource bar allows a
  typed limit; the run still counts as incomplete for correctness.
- `tpch_q3` on mizuchi is killed by the 4 GiB job memory cap at SF10, a
  failed resource bar.
- Peak memory is the cgroup peak of the whole sandboxed worker, including
  page cache and Parquet decode buffers, and is judged against the job cap.
  Whether each engine stays under its own 256 MiB internal cap is not
  measured: that needs engine-side accounting. For scale, mizuchi peaks at
  761 MiB on SF1 `tpch_q3` where DuckDB peaks at 32 MiB.

Every query both engines completed returned the same rows.

Single runs vary: a rerun on the same runner class measured geometric means
of 2.86 (SF1, worst 5.03) and 15.37 (SF10, worst 18.30), with the same
verdict on every bar. Treat the ratios as an order of magnitude, not a
precise figure.

## Verdict

mizuchi does not meet the bar at SF1 or SF10. Correctness fails at both
scales through runs that did not complete. Performance is 4.65x (SF1) and
11.91x (SF10) slower by geometric mean against a 2.0 bar. The resource bar
passes at SF1 and fails at SF10 on an out-of-memory kill. The default engine
stays DuckDB.
