# Engine requirements

This page is the bar for adding an engine behind kako's job contract, or
for switching the default later. It does not implement any engine.
[ADR 0003](decisions/0003-engine-opt-in-bar.md) is the decision that this
bar is how those changes are judged.

The job source still decides what runs and whether to retry. An engine only
executes one job in one sandboxed child.

## Engine contract

An engine is a worker inside the kekkai sandbox. It must:

| Requirement | Meaning |
| --- | --- |
| Job I/O | Read staged inputs from the run scratch; write each named output under `outputs/`. |
| Isolation | No network. Filesystem writes stay in the scratch tree (plus the sandbox temp dir). The job's `isolation` floor is enforced fail-closed ([ADR 0009](decisions/0009-isolation-tier.md)). |
| Limits | Honor the thread cap and the memory cap the runner sets. Spill to `tmp/` inside scratch, or fail as `Failure::Limit`. Do not crash the host. |
| Cancellation | Stop work when the runner kills the process group. After that signal, the child is gone within 5 seconds. |
| Identity | Return a stable `engine_version` string for the receipt (name and version). |
| Determinism | The same job bytes and input bytes produce the same output bytes, or a documented, tested equality rule. |

Typed in-memory frames (for example Arrow) are allowed as an internal
contract between kako and the engine. They are not required of the current
file-backed SQL path. A candidate that only speaks frames must still land
files under `outputs/` so the job source sees the same artifacts.

User code is data. The runner does not import it. The Python engine loads
user source from a scratch file through a wrapper
([ADR 0005](decisions/0005-python-runtime-image.md)).

## Correctness bar

- **Fixture suite (required now):** `scripts/engine-bar/` join and constant-row
  jobs. Results must match the current default engine after `ORDER BY` on the
  fixture queries. Integers compare exactly. Floats, if they appear, compare
  with relative tolerance `1e-6`.
- **Digest stability:** two runs of the join job on the same inputs produce
  the same output digest.
- **sqllogictest (required to switch the default):** a named `.slt` corpus
  checked into this repository, run against both engines, with the equality
  rules above. Ordering is compared only when the statement has `ORDER BY`;
  otherwise both results are sorted before compare.

## Performance bar

Measured on one node, same sandbox backend, same CPU and wall-clock limits,
same scratch layout.

| Gate | Threshold |
| --- | --- |
| Fixture suite | Candidate geometric mean of wall-clock ratios (candidate / current) ≤ 2.0; no job more than 5× slower. |
| Scale-factor suite (required to switch the default) | Same ratios at scale factor 1, then 10, on a fixed join/aggregate query set stored next to the harness. |

Wall-clock is the receipt `duration_ms`. Do not compare machines. Publish
the host class (CPU count, memory) with the numbers.

## Resource bar

- Memory: the engine stays at or under the configured cap, or fails as
  `Failure::Limit`. An out-of-memory kill of the sandbox is a failed bar,
  not a pass.
- Threads: the engine does not create more worker threads than the cap.
- Spill: temporary files go under the scratch `tmp/` directory.
- Timeout: the existing wall-clock kill path remains the cancellation
  mechanism until an engine exposes a cooperative cancel.

## Lineage bar

For an opt-in engine, `engine_version` is enough for receipt v1. The
mizuchi adapter also writes a `lineage` object (plan, reads after prune,
writes with `overwrite` or `append`). DuckDB receipts omit that field.

To switch the default, the engine must also expose, for each run:

- input datasets actually read (names and paths inside scratch);
- columns read, when the engine knows them;
- the executed plan as a string.

Those fields may land in a later receipt version. The engine API must
produce them even if v1 does not serialize them yet.

## Operational bar

- Versioned releases and a changelog.
- A security policy: no network, no reads outside the granted roots, a
  private channel for sandbox escapes.
- Fail closed when the sandbox backend cannot be applied.

## Process

1. Add the engine opt-in. Default tests stay green without it.
2. Run `bash scripts/engine-bar/run.sh` against current default and the
   candidate on the same host. Keep both JSON reports with the PR.
   A mizuchi job uses `"engine": "mizuchi"` and
   `scripts/engine-bar/join-mizuchi.json` on the same fixture inputs.
   A Python job uses `"engine": "python"` and
   `scripts/engine-bar/join_polars.py` / `join_pandas.py`.
3. A maintainer accepts the comparison against this page.
4. Switching the default is a new ADR once correctness, performance at
   scale factor 1, resource, lineage, and operational bars all pass.

## Current results

mizuchi against DuckDB, from [engine-bar/results.md](engine-bar/results.md):

| Bar | SF1 | SF10 |
| --- | --- | --- |
| Correctness (completed queries match) | pass | pass |
| Correctness (every query completes) | fail: `tpch_q1` aggregate cannot spill | fail: `tpch_q1`, `tpch_q3` |
| Performance (geometric mean ≤ 2.0, no query > 5×) | fail: 4.65, worst 6.24 | fail: 11.91, worst 12.21 |
| Resource (stays under the job cap or fails as a limit) | pass | fail: cgroup out-of-memory kill |
| Engine-internal memory cap | not measured | not measured |

The default engine stays DuckDB.

## Harness

`scripts/engine-bar/run.sh` runs the fixture jobs through `kako run` and
prints one JSON object per job (`id`, `engine_version`, `duration_ms`,
output digests). It needs kekkai `linux_native`. Without that backend the
script exits 2; the same SQL is still covered by
`engine::tests::engine_bar_join_matches_fixture` and the MVP join tests.

`bash scripts/engine-bar/sf/run.sh <tiny|1|10> <dir>` is the scale-factor suite:
it generates the decision-support tables, runs the fixed query set in
`scripts/engine-bar/sf/queries/` on both engines under the same limits, and
`report.py` scores the run against this page. The `Engine bar` workflow runs
SF1 and SF10 weekly and on demand; it is not part of pull-request CI except
when the harness changes. `engine_bar_sf` in the kako test suite runs the same
SQL at the tiny scale in process.

To capture a baseline:

```bash
bash scripts/engine-bar/run.sh > engine-bar-baseline.jsonl
```
