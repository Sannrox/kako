# Project profile for agent-kit skills

<!-- Read by the shared skills in https://github.com/Sannrox/agent-kit (pinned in .agents/agent-kit.lock). Keep repo-specific facts here, not in the shared skills. Section headings are a contract: do not rename them. -->

## Project

- Name: kako
- Feature means: a new observable operator or integration outcome

## Ownership boundaries

The job source decides what runs; kako only executes and reports outputs and receipts; kekkai owns the sandbox.

Route work owned by another repository (a job source's protocol, sandbox enforcement) to that repository.

## Affected areas

job source/stage/run/receipt/sandbox/image/security

## Shared surfaces

Issues touching the same shared surface run in sequence, never as parallel lanes:
the claim-client pin, the kekkai pin, the run workspace layout, or the runtime image.

## Impact boundaries

- job source authority versus kako execution: kako never decides what runs or retries on its own;
- the `JobSource` contract and each adapter's own guarantees (for example a claim fence, where outputs and the receipt land together or not at all);
- the receipt format: input digests, code digest, engine version, output digests, timings;
- the kekkai sandbox boundary: no network, CPU, memory and wall-clock limits, writes confined to the run's scratch workspace;
- input staging, output hashing determinism, and scratch cleanup on success, failure, and timeout;
- engines: DuckDB for SQL; Python (Polars, pandas) only as user code inside the sandbox, with packages from the runtime image;
- pinned upstream versions: kekkai and any adapter client crates;
- configuration, CLI, logs and metrics, and packaging.

## Compatibility and failure obligations

Adapter and kekkai version pins, partial runs (crash after output written but before it is reported, an adapter losing its claim mid-run), retries owned by the source, cleanup of scratch and processes, and runtime image contents.

## Evidence map

Unit tests for pure logic; tests with a fake job source for the run loop and reporting paths; fixtures for staging and hashing; ignored live tests only when a real external job source, Python runtime, or kernel feature is essential.

## Verification matrix

| Surface | Required evidence |
| --- | --- |
| job source, adapters, receipts | tests with a fake job source covering success, job failure, limit hits, and source errors; receipts complete and deterministic |
| staging, hashing, cleanup | deterministic tests: identical inputs give identical hashes; scratch removed on success, failure, and timeout |
| sandboxed execution | the run is spawned through kekkai with no network and limits set; kernel-dependent tests skip with an explicit message |
| dependency pins (kekkai, adapter clients) | `Cargo.lock` diff reviewed; the pinned tag exists upstream; upstream release notes read |
| runtime image / Python packages | image builds; package set matches the documented policy |

## Release audit

- workspace `Cargo.toml` version and lockfile consistency;
- pinned kekkai version and adapter client versions, and their release notes;
- job source adapter compatibility and receipt format changes;
- configuration additions, removals, and defaults;
- runtime image contents and the Python package set;
- packaging, rollback, and upgrade actions.
