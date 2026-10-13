# ADR 0011: Runner pool health, drain, and rollback

- Status: Accepted
- Date: 2026-10-13
- Relates: [0002](0002-sandboxed-transform-runner.md), [0008](0008-release-artifacts.md), [0010](0010-crate-graph.md)
- Source: [Issue #34](https://github.com/Sannrox/kako/issues/34)

## Context

A long-running pool of kako processes had no health, readiness, drain, or
version signal. Replacing a runner under load could lose or duplicate a job.
Tagged GitHub Release artifacts are [ADR 0008](0008-release-artifacts.md);
this decision is the process lifecycle those artifacts run under. The job
source stays the authority for what runs and whether it retries
([ADR 0002](0002-sandboxed-transform-runner.md)).

## Decision

1. **`kako serve`** is the long-running pool entry. It listens for
   `GET /health`, `GET /ready`, and `GET /version` on `--listen`, using the
   standard library only. A pool is N independent processes. This repository
   does not pick a process supervisor. Ready means the process will claim
   work. `kako serve` claims a local job file when given one, then goes
   not-ready when that one-shot source is empty. Without a file it stays
   not-ready and only serves probes and drain. Callers that have a
   `JobSource` use `serve_source`. Live Sekai claiming is later.
2. **Readiness** is true while the process will claim work. It goes false at
   the start of drain. **Health** stays true while an in-flight job is still
   running, then goes false as the process exits. **Version** is the binary's
   crate version and stays available during drain.
3. **Drain** (SIGTERM/SIGINT, or `DrainHandle::request`) stops `next_job`.
   The in-flight job is either finished (`complete` / `fail`) or handed back
   with `JobSource::release`. Hand-back must not write outputs.
   Adapter fence and lease rules stay in the adapter (`kako-source-sekai`).
4. **Rollback** uses the same drain path, then starts the previous `v*`
   GitHub Release binary from ADR 0008. kako does not retry the handed-back
   job; the source does.

## Consequences

- `JobSource::release` is required. Sources that cannot restore a claimed
  job return an error. Hand-back must not write outputs.
- `run_source` still stops when the source is empty. `serve_source` idles.
- Operators roll a pool by draining each process and replacing the binary
  from the previous Release. Autoscaling and guest isolation are out of scope.

## Alternatives considered

- **Let the supervisor SIGKILL in-flight runs.** Rejected: work is lost or
  duplicated unless the source can accept a hand-back.
- **Add an HTTP crate.** Rejected: three GET routes do not justify a new
  dependency on the run-loop crate.
- **kako decides retries after hand-back.** Rejected: that would move
  authority out of the job source.
