# ADR 0007: kako stays single-node; scale out by partition fan-out

- Status: Accepted
- Date: 2026-10-11
- Relates: [0002](0002-sandboxed-transform-runner.md), [0003](0003-engine-opt-in-bar.md)
- Source: [Issue #7](https://github.com/Sannrox/kako/issues/7)

## Context

Each kako run is one sandboxed process on one host ([ADR 0002](0002-sandboxed-transform-runner.md)).
Larger datasets can exhaust memory or miss a wall-clock limit. A common
instinct is to add a cluster engine. That would be a new engine behind
[ADR 0003](0003-engine-opt-in-bar.md), a new sandbox story, and a receipt
that no longer describes one process.

The job source already decides what runs and how results commit. It can
split a large input into partitions, run many single-node jobs, and
commit them together.

## Decision

1. **kako stays single-node.** DuckDB SQL and opt-in Python (Polars,
   pandas) remain in-process engines in one sandboxed child. This change
   does not add a cluster, a distributed query engine, or a task graph.
2. **Scale-out, when needed, is partition fan-out at the job source.**
   The source slices inputs, submits one kako job per slice, and commits
   outputs under its own fence. kako keeps one job, one receipt, one
   scratch tree.
3. **Defer a distributed engine** until a job source actually sees
   inputs that fail the per-run memory cap or miss the operator's
   wall-clock after fan-out is in use. A new engine would still have to
   clear the bar in [docs/engine-requirements.md](../engine-requirements.md).
4. **Measured engine ceiling (join + group aggregate, two orders per
   customer, CSV in/out).** Recorded on the research host with
   `KAKO_CEILING_CUSTOMERS=2000,20000,100000`
   (`scripts/single-node-ceiling.sh`). The sweep calls
   `execute_in_scratch` in the test process: that is the shipped engine
   worker, without kekkai spawn, Landlock, or cgroup. A production
   `kako run` adds that sandbox cost on top. Numbers are host-specific;
   they show this size class finishes in seconds on the engine, not a
   guarantee for every machine.

   | customers | orders | DuckDB | Polars |
   | --- | --- | --- | --- |
   | 2_000 | 4_000 | 262 ms | 458 ms |
   | 20_000 | 40_000 | 498 ms | 456 ms |
   | 100_000 | 200_000 | 807 ms | 473 ms |

   Both engines wrote identical `totals.csv` bytes at each size. Python
   samples include starting the interpreter and importing packages
   (DuckDB SQL does the equivalent in-process). The first Python sample
   on a cold host was about 15 s; later samples in the same sweep
   finished in a few hundred milliseconds once those files were in the
   OS cache. Peak memory on a real run is receipt `memory_peak_bytes`.
   Default `cargo test` runs only 2_000 customers so offline CI stays
   cheap. One hundred thousand customers is tens of megabytes of CSV,
   not tens of gigabytes; after import the engine still finished in
   under a second, which is why the next step is fan-out rather than a
   cluster.

## Consequences

- Operators who outgrow one node ask the job source to partition. They
  do not configure a kako cluster.
- Receipts stay one process: one code digest, one engine version, one
  set of output digests.
- A later distributed engine is a new opt-in behind ADR 0003, with its
  own sandbox and a receipt story for many workers. That work is not
  started here.

## Alternatives considered

- **A distributed query engine as a kako engine.** Rejected for now:
  new runtime, new sandbox surface, and a receipt that is no longer one
  child. Rust-native query clusters still need a scheduler and shuffle
  that kako does not own.
- **A general task-graph or JVM batch cluster.** Rejected: heavy
  operations, weak sandbox fit, and a control plane that would compete
  with the job source.
- **Keep silent and wait for OOM.** Rejected: the Issue asked for a
  recorded threshold and a path. Fan-out is that path; the table above
  is the current single-node evidence.
