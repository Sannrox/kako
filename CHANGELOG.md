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
