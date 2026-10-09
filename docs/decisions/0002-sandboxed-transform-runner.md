# ADR 0002: kako is a sandboxed transform runner with pluggable job sources

- Status: Proposed
- Date: 2026-10-09
- Supersedes: —

## Context

Data transforms (SQL with joins and aggregates, Python with Polars or pandas)
are often user-supplied code. Python is arbitrary code: it must not run inside
the process of the system that stores or governs the data, and it needs a
sandbox, resource limits, and no network. Users of the output also need to
know exactly what produced it.

Systems that want such transforms differ in where jobs come from: a local
file, a CLI call, a queue, or a control plane such as
[Sekai](https://github.com/Sannrox/sekai-chisei). The execution part is the
same.

## Decision

1. **kako is a standalone runner binary written in Rust.** Python exists only
   as user code inside the sandbox.
2. **Pluggable job sources.** A `JobSource` trait hands out jobs and receives
   outputs, receipts, and failures. The source decides what runs, retries,
   and records results; kako never decides on its own. A local/CLI source is
   first-class; a Sekai adapter is one of several.
3. **One run is one short-lived sandboxed process.** kako stages inputs into a
   scratch directory, runs the engine under no-network, CPU, memory and
   wall-clock limits, hashes outputs, reports them, and removes the scratch
   directory.
4. **Engines:** DuckDB SQL first; Python (Polars, pandas) later.
5. **Receipts:** every run yields input digests, code digest, engine version,
   output digests, and timings. The format is versioned.
6. **Sandbox from kekkai.** kako depends on the
   [kekkai](https://github.com/Sannrox/kekkai) crate and never forks sandbox
   code. A per-run container or microVM tier (gVisor, Firecracker) is
   required before running untrusted code from mutually distrusting users.

## Consequences

- The runner can be used and tested without any control plane.
- Each adapter maps its own claim, lease, and write-back semantics onto the
  trait; source-specific guarantees (for example fenced write-back) live in
  the adapter.
- Every Python package must be in the runtime image. Package policy is a
  follow-up decision.
- Address-space limits (`RLIMIT_AS`) break numeric libraries; memory is
  limited through cgroup v2 limits in kekkai instead.

## Alternatives considered

- **Add a "run a command" mode to an agent runtime.** Rejected: it turns an
  agent harness into a general job runner and couples scaling, images and
  security review.
- **Nomad or Kubernetes Jobs.** Good at running a container to completion,
  but they bring their own scheduling and identity and produce no receipts.
  They may still host kako runners later.
- **Dagster, Airflow, or dbt.** Full orchestrators with their own lineage and
  scheduling; kako is meant to sit underneath such systems, not replace them.
- **Hardwire one control plane.** Rejected: it would make local use and
  testing depend on that plane.
