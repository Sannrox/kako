# ADR 0004: The job source owns quotas and fair share; kako enforces per-run limits

- Status: Accepted
- Date: 2026-10-11
- Supersedes: —

## Context

Several owners can share a pool of runners. One owner's flood can starve
others, and nothing yet bounds CPU, memory, or run time per owner.

A single runner cannot see the global queue. Quota keys are already
`(tenant_id, namespace)` on every job ([Issue #10](https://github.com/Sannrox/kako/issues/10)).
The sandbox can apply a CPU ceiling, a cgroup memory cap, and a wall-clock
timeout, and can report measured CPU time and peak memory after the child
exits.

## Decision

1. **The job source owns the queue.** It chooses the next job, applies
   concurrent-run and aggregate quotas, and implements fair share and
   priority across owners. kako never reorders, skips, or retries on its
   own.
2. **kako enforces per-run limits it is handed.** `Job` carries wall-clock
   (already required) plus optional `cpu_time` and `memory_bytes`. A memory
   cap is applied through the sandbox cgroup. If that cap cannot be set,
   the run fails closed.
3. **Receipts report measured usage.** When the sandbox reports them, the
   receipt includes `cpu_usec` and `memory_peak_bytes`. Missing measurements
   are omitted rather than invented.
4. **The local CLI source stays one-shot.** It does not schedule. A
   multi-tenant source uses `QuotaKey` to fair-share.

## Consequences

- Adapters that see the whole queue implement fairness. `FairSource` is a
  reference round-robin adapter; it is not installed in front of other
  sources. Tests drive it through the run loop. Load tests through
  `run_source` assert the round-robin bound: with N owners and one flood,
  every other owner's first job starts within N−1 slots of the first
  start. Sequential `run_source` records one start per slot. Missing-input
  jobs fail closed before the sandbox so the load test measures
  scheduling.
- Per-run memory limits require a delegated cgroup on the host.
- Aggregate owner caps never live in kako: the runner has no global view.

## Alternatives considered

- **Enforce quotas only in kako.** Rejected: fairness would be per runner
  and would ignore the rest of the queue.
- **A kako-owned scheduler in front of every source.** Rejected: it would
  take authority away from the job source ([ADR 0002](0002-sandboxed-transform-runner.md)).
