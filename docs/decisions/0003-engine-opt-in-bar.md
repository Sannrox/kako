# ADR 0003: New engines are opt-in behind a published bar

- Status: Accepted
- Date: 2026-10-11
- Supersedes: —

## Context

kako runs SQL today through one embedded engine behind the job contract
([ADR 0002](0002-sandboxed-transform-runner.md)). Other engines can look
attractive. Without a written bar, adding or switching one is either never
possible or decided by taste.

The job source stays the authority for what runs. The engine is a worker
behind that contract: it must not take network, must stay inside the run
scratch, and must produce a receipt the source can record.

## Decision

1. **The default engine does not change in the same change that adds a
   candidate.** A new engine lands opt-in (an `Engine` variant or cargo
   feature). Switching the default is a later ADR after the bar is met.
2. **The bar lives in [docs/engine-requirements.md](../engine-requirements.md).**
   That page is the contract, the correctness / performance / resource /
   lineage / operational thresholds, and the evaluation process.
3. **The named fixture suite in `scripts/engine-bar/` is the first baseline.**
   A candidate is measured against the current default engine on that suite,
   under the same sandbox limits. Larger scale-factor suites may be added
   later; they do not replace the fixture suite.

## Consequences

- Reviewers can reject an engine PR that does not publish comparison
  evidence against the bar.
- The current engine remains the baseline even if a candidate is faster on
  some queries.
- Receipt `engine_version` stays a single string until a later receipt
  version adds plan and column lineage.

## Alternatives considered

- **Ad-hoc evaluation per candidate.** Rejected: not repeatable.
- **Switch the default as soon as a candidate is wired up.** Rejected: it
  couples integration work to a production default.
- **Require Arrow in/out of the default engine before any opt-in.** Rejected:
  today's jobs are files in the scratch directory. Arrow is a requirement
  for engines that want a typed in-memory contract; it is not a gate on
  keeping the file-backed default.
