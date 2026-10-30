# ADR 0012: kako receipts map onto the Sekai governed-transform JobSpec and run receipt

- Status: Accepted
- Date: 2026-10-18
- Relates: [0002](0002-sandboxed-transform-runner.md), [0010](0010-crate-graph.md)
- Source: [Issue #29](https://github.com/Sannrox/kako/issues/29)
- Upstream: sekai-chisei ADR 0097 (in-process transform host), ADR 0102 (isolated-process profile)

## Context

kako writes a versioned file receipt (`receipt_version` 3: job, tenant,
namespace, inputs, code digest, engine version, outputs, usage, lineage,
isolation). The plane admits a content-addressed JobSpec, runs a profile, and
stores a `TransformRun` receipt with lineage. Later engine profiles keep that
JobSpec, transaction, and receipt. There was no mapping, so a kako receipt
could not be checked as a Sekai run receipt.

Hosting governed transforms inside the plane stays a non-goal for kako.

## Decision

1. **Map, do not replace.** kako keeps its file receipt. A pure conversion in
   `kako-core` produces the Sekai `TransformRun` JSON field set from a `Job`
   plus `Receipt`. `kako-core` does not depend on Sekai crates.
2. **JobSpec profile is `isolated_process`.** Every kako engine (DuckDB SQL,
   mizuchi, Python) is a sandboxed child. The JSON is the plane
   `GovernedTransform` shape: empty `steps`, nested `isolated_process`
   (`engine`, `runtime_image`, named inputs). The hashed envelope is
   namespace, transform id (`job.id`), first input and output names, empty
   steps and quality rule, engine spelling, runtime image, and named inputs.
   The hash follows the plane `definition_digest` algorithm (length-prefixed
   SHA-256 fields, `sha256:<hex>`). SQL and unpinned Python leave
   `runtime_image` empty; that definition is not plane-admissible. Plane
   admission stays a non-goal.
3. **Program bytes stay on the kako receipt.** Isolated-process JobSpec does
   not hash SQL or Python source. kako `code` remains the digest of the
   program that ran.
4. **Run receipt fields.** `run_id` and `transform_id` are `job.id`;
   `input_digest` / `output_digest` are the kako file digests (one name uses
   that digest; several names hash name then digest with the same
   length-prefix rule); `profile` is `isolated_process`; `engine` is
   `duckdb_sql` / `mizuchi` / `python`; `dispatch_status` is `completed`;
   `trigger_cause` is `manual`. `runtime_image` is the receipt's resolved
   identity (catalog default when the job left the pin unset). Write lineage,
   when present, fills `rows_out` and one `output_partitions` entry per
   partition identity. Row counts attach only when a write names a single
   partition. Partition `digest` stays empty: kako `commit_id` is write
   metadata, not a content digest. File identity stays on `output_digest`.

## Consequences

- Isolated-process inputs may name an object set (`object_set` JSON, empty
  `dataset_id`). The digest hashes that field when present. Dataset-only
  envelopes are unchanged. `isolated_process.requires` and `code` hash when
  non-empty, matching the plane; kako-generated specs still omit both.
- SQL and Python jobs both yield JSON that deserializes as a Sekai run
  receipt.
- A Sekai adapter can keep fenced write-back as it is; this mapping is the
  receipt contract, not the row-sum output digest used at write-back.
- Changing the plane digest algorithm requires updating the kako copy and
  the contract tests.

## Alternatives considered

- **Adopt `TransformRun` as the kako file receipt.** Rejected: local CLI
  consumers already read `receipt_version` 3 fields (`code`, `isolation`,
  per-name input maps). Schema changes would bump `receipt_version` for a
  format that still needs kako-only evidence.
- **Depend on `sekai-types` from `kako-core`.** Rejected: ADR 0010 keeps
  core free of Sekai.
- **Map DuckDB SQL as the in-process `analytical` profile.** Rejected: kako
  does not run the plane's in-process SQL host.
