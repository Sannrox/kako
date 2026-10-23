# ADR 0016: Replays run pinned steps into a branch and never substitute a pin

- Status: Accepted
- Date: 2026-10-23
- Relates: [0006](0006-python-package-policy.md), [0012](0012-governed-receipt-mapping.md)
- Source: [Issue #53](https://github.com/Sannrox/kako/issues/53)
- Upstream: pipeline replay (sekai-chisei#1585)

## Context

The control plane replays a pipeline to check that it is reproducible, or
to see what a changed transform would produce. It needs a runner that
executes each step exactly as an earlier run did and keeps the result away
from the published outputs. It also needs to know, before anything runs,
which steps this runner cannot reproduce.

## Decision

1. **A replay is a branch and ordered steps.** Each step is a job plus
   pins taken from the original receipt: the input digests, the code digest
   (the transform version) and the engine version. A Python step's job pins
   its `runtime_image`. Optional output pins are the original output
   digests.
2. **Check every pin before running anything.** Inputs and code are
   hashed. SQL engine versions are read from the linked engines without a
   run. Python images are looked up in the image catalog, and the
   interpreter reports its version with that image's packages, without
   running user code. If any step fails a pin, nothing runs and the report
   lists every such step. A missing version is reported, never substituted.
3. **Re-check each step against its receipt.** If the engine that ran, or
   an input as staged, differs from its pin, the step is not reproducible
   and its outputs are not written. Later steps are skipped.
4. **Outputs go to a branch.** `kako replay` writes each step to
   `<output>/<branch>/<step>/` with its receipt, `/` in the branch written
   `%2F` so branches never nest. An OS file lock lets one replay hold a
   branch at a time and is dropped if the process dies. `replay.json` records each
   step's status and output digests next to the pins. `main` and `master`
   are refused.
5. **Steps do not feed each other.** The job source stages every step's
   inputs at their pinned versions. It also decides what runs and retries
   (ADR 0002) and computes diffs from the reports.

## Consequences

- With unchanged inputs and pins, a replay reproduces the original output
  digests, and the report says `reproduced`. A changed transform pins its
  own code, and the report says `diverged` where outputs differ.
- Old runtime images must stay in the image catalog for as long as their
  runs should stay replayable. The shipped image is always available.
- The receipt format is unchanged. Per-step receipts are ordinary receipts.
- A crashed replay is rerun from the top. The report is marked incomplete
  and every step directory is cleared before anything runs, so a stale
  receipt never looks current.

## Rejected alternatives

- Falling back to the current engine or default image. That would make a
  replay look reproducible when it is not.
- A replay field on `Job` and `Receipt`. That would change the job-source
  trait and receipt format for every source, while only replays need it.
- Chaining step outputs into later steps' inputs. That is a dependency
  graph between transforms, which the job source owns.
