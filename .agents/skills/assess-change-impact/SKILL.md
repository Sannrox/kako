---
name: assess-change-impact
description: Assess a proposed or implemented kako change across trust, API, dependency, and operations boundaries. Use when scoping an Issue, planning tests, reviewing a diff, or identifying compatibility, documentation, and security obligations.
---

<!-- Ported from Sannrox/shikigami@331897846885e4a55697b0f1006d03b4f2a7495e .agents/skills/assess-change-impact/SKILL.md (rewritten for kako; structure kept). Compare against the source to detect drift. -->

# Assess Change Impact

Build an evidence-backed impact map before implementation or review.

## Procedure

1. Read the linked Issue or request, `README.md`, accepted ADRs under
   `docs/decisions/`, and the relevant code. For a diff, inspect every changed
   file and its direct callers or implementors. Complete when the claimed
   outcome and actual change surface are both known.
2. Trace applicable boundaries:
   - job source authority versus kako execution: kako never decides what
     runs or retries on its own;
   - the `JobSource` contract and each adapter's own guarantees (for example
     a claim fence, where outputs and the receipt land together or not at all);
   - the receipt format: input digests, code digest, engine version, output
     digests, timings;
   - the kekkai sandbox boundary: no network, CPU, memory and wall-clock
     limits, writes confined to the run's scratch workspace;
   - input staging, output hashing determinism, and scratch cleanup on
     success, failure, and timeout;
   - engines: DuckDB for SQL; Python (Polars, pandas) only as user code inside
     the sandbox, with packages from the runtime image;
   - pinned upstream versions: kekkai and any adapter client crates;
   - configuration, CLI, logs and metrics, and packaging.
   Complete when each applicable boundary has an owner and expected invariant.
3. Identify compatibility and failure obligations: adapter and kekkai
   version pins, partial runs (crash after output written but before it is
   reported, an adapter losing its claim mid-run), retries owned by the source, cleanup of scratch
   and processes, and runtime image contents. Complete when data-loss,
   duplicate-write, and partial-failure paths are accounted for.
4. Map evidence to risk: unit tests for pure logic; tests with a fake job
   source for the run loop and reporting paths; fixtures for staging and hashing;
   ignored live tests only when a real external job source, Python runtime, or kernel
   feature is essential. Complete when every material risk has a proposed check
   or an explicit residual uncertainty.
5. Determine durable artifacts that must change: README, docs, examples, an
   ADR, upstream Issues in the owning repository, or a repository Skill.
   Complete when no artifact is proposed merely to record temporary planning.

## Output

Return a compact matrix with columns:

| Surface | Evidence found | Required change/check | Risk if missed |
| --- | --- | --- | --- |

Then list scope boundaries, blocking questions, and the smallest safe PR split.
Do not approve an architecture, perform a full security audit, or claim
behavior parity without inspecting the implementations.
