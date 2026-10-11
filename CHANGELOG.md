# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project
uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

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
