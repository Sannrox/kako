# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Security

- The sandboxed worker gets only the environment variables kako sets for it.
  It no longer inherits the runner's environment, which user code could read
  through `/proc/self/environ`.

- Remote job sources mint one short-lived credential per claimed job, scoped
  to that job's inputs and outputs. The runner process holds it, revokes it
  when the claim ends, and refuses a broader or longer-lived credential
  ([ADR 0013](docs/decisions/0013-job-scoped-credentials.md)). The Sekai
  `ClaimTransport` gains `mint_credential` and `revoke_credential`, and
  `write_outputs` takes the credential.

### Changed

- Receipt JSON is `receipt_version` 4: optional `credential_scope` names the
  job-scoped credential used for write-back, never its secret. Local file
  jobs omit it.

- Isolated-process jobs key quotas on the submitting transform owner from
  `TransformWork.tenant_id` when present, and keep the namespace key for
  community work with no owner. The optional `sekai-client` pin is
  `dccc1e704adf21057bcaa435aef90660db0e9c52`.

- The workspace splits into `kako-core`, `kako-sandbox`, engine crates,
  `kako-worker`, and `kako` ([ADR 0010](docs/decisions/0010-crate-graph.md)).
  `kako::` module paths stay. Sandbox functions take `IsolationTier` and
  return kako usage values instead of kekkai types. Job schema, receipts,
  isolation policy, and the kekkai pin are unchanged.

- kekkai pin moves to `73e24f580ed6223b7e8395793e6f2317eb8edfd9` (includes the
  userspace guest adapter). Default jobs still select `linux_native`. Jobs
  that require `userspace` spawn through the guest helper, or fail closed
  when guest health cannot be applied.

### Added

- kako jobs and receipts map onto the Sekai isolated-process JobSpec and
  `TransformRun` JSON (`kako_core::governed`). The JobSpec digest uses the
  plane `definition_digest` algorithm. SQL and Python receipts deserialize
  as that run-receipt shape
  ([ADR 0012](docs/decisions/0012-governed-receipt-mapping.md),
  [governed-receipt.md](docs/governed-receipt.md)).

- `kako serve`: `/health`, `/ready`, and `/version` on a long-running
  process; SIGTERM drains (stop claiming, finish the current job or hand it
  back with `JobSource::release`, then exit). Ready means the process will
  claim work. A local job file is claimed, then the process goes not-ready
  when that one-shot source is empty; without a file it stays not-ready.
  Probe connections are handled off the accept loop so a slow client cannot
  stall `/health` or `/ready`. Rollback is the same drain path plus the
  previous GitHub Release binary
  ([ADR 0011](docs/decisions/0011-runner-pool-drain.md)).

- Optional `kako-source-sekai` crate: a `JobSource` adapter that claims
  isolated-process work behind a fenced transport. Default tests use a fake
  client so a lost fence aborts write-back and writes nothing. kako-core
  stays free of Sekai (`sekai` feature on `kako`, #8).

- Mizuchi jobs pass `write_mode` (`overwrite` or `append`) through to the
  adapter. COPY SQL writes a new CSV and rejects `append`; partitioned
  parquet writes honor both modes. Receipt snapshots stay identical across
  repeated runs except timing fields.

- FairSource load test: with N owners and one flood, every other owner's
  first job starts within N−1 slots across repeated `run_source` trials
  ([ADR 0004](docs/decisions/0004-quota-authority.md)).

- CI sets `KAKO_REQUIRE_PYTHON=1` so Python engine tests cannot skip,
  delegates a cgroup v2 subtree for `memory_bytes` jobs, and builds the
  Python runtime image twice then runs the Polars and pandas joins inside
  it (`scripts/python-runtime-ci.sh`).

- Isolation is a job-source floor (`isolation` on the job, recorded on the
  receipt). Omitted jobs stay `linux_native`. A stronger floor the host
  cannot meet is a runner failure, with no silent downgrade
  ([ADR 0009](docs/decisions/0009-isolation-tier.md)). Receipt JSON is
  `receipt_version` 3.

- Jobs with `isolation: userspace` prepare kekkai `Backend::Userspace` and
  spawn through `kekkai-guest` (or `KEKKAI_GUEST_HELPER`): per-run scratch
  as the workspace, no network, job CPU and wall-clock limits. A missing
  helper or failed guest health is a runner failure with no outputs. Escape
  tests that need the helper are `#[ignore]` in offline `cargo test`.

- Tagged GitHub releases package the runner binary, SQL and Python runtime
  images, checksums, and artifact attestations. Crates stay unpublished
  while git pins remain
  ([ADR 0008](docs/decisions/0008-release-artifacts.md)).
  `bash scripts/release-dry-run.sh --out dist` builds the same files
  without creating a release.

- `kako run <job.json>`: local job source, copied input staging, sandboxed
  DuckDB SQL worker under kekkai `linux_native`, SHA-256 receipts, and
  scratch cleanup on success, failure, and timeout. Runs fail closed when
  `linux_native` cannot be applied. User SQL can read and write only the
  per-run scratch directory.
- `kako doctor`: reports the kekkai backend the host can enforce.
- Receipt JSON (`receipt_version` 1): input and code digests, engine version,
  output digests, start time, duration.
- Skeleton runner binary and core types: `JobSource`, `Job`, `Output`,
  `Receipt`, `Failure`.
- ADR 0002: sandboxed transform runner with pluggable job sources.
- Engine requirements bar ([ADR 0003](docs/decisions/0003-engine-opt-in-bar.md),
  [docs/engine-requirements.md](docs/engine-requirements.md)): new engines are
  opt-in; `scripts/engine-bar/` is the named fixture baseline.
- Opt-in mizuchi SQL engine: `"engine": "mizuchi"` on a job file. Default
  remains DuckDB.
- Mizuchi receipts include a `lineage` object (plan, reads after prune,
  writes). DuckDB receipts omit the field.
- Jobs, receipts, and quota keys carry `tenant_id` and `namespace`. Receipt
  JSON is `receipt_version` 2. The local CLI fills `local` / `default`
  unless `--require-identity` is set.
- Per-run optional `cpu_time` and `memory_bytes` on jobs. The job source
  owns quotas and fair share ([ADR 0004](docs/decisions/0004-quota-authority.md));
  kako enforces the limits it is handed. Receipts include `cpu_usec` and
  `memory_peak_bytes` when the sandbox reports them.
- `FairSource`: round-robin by quota key so one owner's flood does not
  block another owner's first run beyond a few slots. The local CLI stays
  one-shot.
- Opt-in Python engine: `"engine": "python"` with `transform(inputs)` over
  staged frames. Packages come from `runtime/python/requirements.txt`;
  receipts record `runtime_image` as the lockfile digest. Default stays
  DuckDB ([ADR 0005](docs/decisions/0005-python-runtime-image.md)).
- Python jobs may pin `runtime_image` to a curated lockfile identity.
  Operators keep prior images under `KAKO_PYTHON_IMAGES`; an unknown pin
  fails closed. Upgrading the default image does not change a pinned
  transform ([ADR 0006](docs/decisions/0006-python-package-policy.md)).
- Single-node ceiling: kako stays one process per run. Scale-out is
  partition fan-out at the job source. `scripts/single-node-ceiling.sh`
  measures DuckDB and Polars join+agg duration
  ([ADR 0007](docs/decisions/0007-single-node-execution.md)).
