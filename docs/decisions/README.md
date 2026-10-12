# Architecture decision records

Decisions that must outlive a single PR.

| ADR | Title | Status |
| --- | --- | --- |
| [0001](0001-record-architecture-decisions.md) | Record architecture decisions | Accepted |
| [0002](0002-sandboxed-transform-runner.md) | kako is a sandboxed transform runner with pluggable job sources | Accepted |
| [0003](0003-engine-opt-in-bar.md) | New engines are opt-in behind a published bar | Accepted |
| [0004](0004-quota-authority.md) | Job source owns quotas; kako enforces per-run limits | Accepted |
| [0005](0005-python-runtime-image.md) | Python engine uses a baked runtime image and cgroup memory | Accepted |
| [0006](0006-python-package-policy.md) | Python jobs pin a curated runtime image | Accepted |
| [0007](0007-single-node-execution.md) | kako stays single-node; scale out by partition fan-out | Accepted |
| [0008](0008-release-artifacts.md) | Releases ship GitHub artifacts; crates stay unpublished | Accepted |
| [0009](0009-isolation-tier.md) | Isolation-tier selection is a job-source floor; kako fails closed | Accepted |
| [0010](0010-crate-graph.md) | Workspace crates follow runner subsystems | Accepted |

Template: short context, decision, consequences, rejected alternatives.
