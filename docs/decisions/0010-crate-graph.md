# ADR 0010: Workspace crates follow runner subsystems

- Status: Accepted
- Date: 2026-10-12
- Relates: [0002](0002-sandboxed-transform-runner.md), [0008](0008-release-artifacts.md)
- Source: [Issue #43](https://github.com/Sannrox/kako/issues/43)

## Context

kako started as one crate. Domain types, the run loop, kekkai, bundled DuckDB,
mizuchi/Arrow, and the Python image catalog shared a dependency set. A hot
edit of the run loop typechecked the SQL engines. Public sandbox functions
named kekkai `Backend` and `ChildUsage`. ADR 0002 already drew those
boundaries as modules; the crate graph did not.

## Decision

1. **`kako-core`** owns jobs, receipts, `JobSource`, isolation tier, hashing,
   scratch staging, and the worker manifest. It does not depend on kekkai,
   DuckDB, mizuchi, or Arrow.
2. **`kako-sandbox`** owns kekkai selection and spawn. Its public API takes
   `IsolationTier` and returns kako usage values. Kekkai types stay inside
   this crate.
3. **`kako-engine-duckdb`**, **`kako-engine-mizuchi`**, and
   **`kako-engine-python`** own in-process execute (and, for Python, the
   parent-side image catalog). Weight stays in the crate that calls it.
4. **`kako-worker`** dispatches `--internal-execute` onto those engines.
5. **`kako`** is the run loop, local job source, and CLI. It re-exports
   `kako-core` types at the existing `kako::` paths. The binary depends on
   `kako-worker` so the child can run SQL engines; the run-loop modules do
   not call DuckDB or mizuchi.

Job JSON, receipt JSON, isolation policy, the kekkai pin, scratch layout, and
the Python lockfile are unchanged. Crates stay unpublished (ADR 0008).

## Consequences

- A run-loop edit no longer typechecks DuckDB or mizuchi sources.
- A second job-source crate can depend on `kako-core` without pulling engine
  weight.
- The `kako` package still links `kako-worker` so `kako --internal-execute`
  works. Optional engine features and a separate CLI package are left for
  later if package-level fingerprints need thinning.

## Alternatives considered

- **Keep one crate and thin default features.** Rejected: DuckDB and mizuchi
  still compile inside the hot package.
- **Optional engine features on `kako`.** Rejected as the first cut: the
  parent run loop does not call DuckDB or mizuchi, so those crates belong
  behind the worker, not behind feature flags on the run loop.
- **A separate CLI package.** Deferred. Integration tests and
  `CARGO_BIN_EXE_kako` live with the binary today.
