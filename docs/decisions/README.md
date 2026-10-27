# Architecture decision records

Decisions that must outlive a single PR.

<!-- adr-index:start -->
| ADR | Title | Status |
| --- | --- | --- |
| [0001](0001-record-architecture-decisions.md) | Record architecture decisions | Accepted |
| [0002](0002-sandboxed-transform-runner.md) | kako is a sandboxed transform runner with pluggable job sources | Accepted |
| [0003](0003-engine-opt-in-bar.md) | New engines are opt-in behind a published bar | Accepted |
| [0004](0004-quota-authority.md) | The job source owns quotas and fair share; kako enforces per-run limits | Accepted |
| [0005](0005-python-runtime-image.md) | Python engine uses a baked runtime image and cgroup memory | Accepted |
| [0006](0006-python-package-policy.md) | Python jobs pin a curated runtime image | Accepted |
| [0007](0007-single-node-execution.md) | kako stays single-node; scale out by partition fan-out | Accepted |
| [0008](0008-release-artifacts.md) | Releases ship GitHub artifacts; crates stay unpublished | Accepted |
| [0009](0009-isolation-tier.md) | Isolation-tier selection is a job-source floor; kako fails closed | Accepted |
| [0010](0010-crate-graph.md) | Workspace crates follow runner subsystems | Accepted |
| [0011](0011-runner-pool-drain.md) | Runner pool health, drain, and rollback | Accepted |
| [0012](0012-governed-receipt-mapping.md) | kako receipts map onto the Sekai governed-transform JobSpec and run receipt | Accepted |
| [0013](0013-job-scoped-credentials.md) | Job-scoped credentials stay in the runner process | Accepted |
| [0014](0014-job-markings.md) | Markings are opaque labels the job source attaches | Accepted |
| [0015](0015-job-encryption.md) | Job keys stay in the runner; outputs seal after the worker | Accepted |
| [0016](0016-replay-jobs.md) | Replays run pinned steps into a branch and never substitute a pin | Accepted |
| [0017](0017-streaming-checkpoints.md) | Streaming jobs checkpoint on the batch start position and resume from the receipt | Accepted |
<!-- adr-index:end -->

The index is generated from each ADR's heading and `Status:` line. Do not edit
it by hand: run `scripts/adr-index` after adding or changing an ADR. CI runs
`scripts/adr-index --check`.

Template: short context, decision, consequences, rejected alternatives.
