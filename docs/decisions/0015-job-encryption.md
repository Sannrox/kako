# ADR 0015: Job keys stay in the runner; outputs seal after the worker

- Status: Accepted
- Date: 2026-10-18
- Relates: [0013](0013-job-scoped-credentials.md)
- Source: [Issue #50](https://github.com/Sannrox/kako/issues/50)
- Upstream: customer-managed keys (sekai-chisei#1577), engine file encryption (mizuchi#143)

## Context

Job scratch trees, temporary files, and published outputs were plaintext.
A tenant key must make those files unreadable after revoke, and the sandboxed
worker must not see key material through its environment or the scratch tree.

## Decision

1. **The claim carries a wrapped DEK.** `Job.encryption` holds JSON of the
   wrapped key plus `dek_id` and `kek_id`. The runner process unwraps it
   through a `KeyProvider` on `RunnerConfig`. The DEK never enters the
   worker manifest, the scratch tree, or the sandbox environment.
2. **Seal outputs after the worker exits.** The engine writes as today. The
   runner replaces each expected output with a `KAKOSEAL` AES-256-GCM
   envelope before hashing and before `JobSource::complete` copies it.
3. **Revoke fails closed with a distinct receipt.** Unwrap `Revoked` is
   `Failure::KeyRevoked`. The local source writes `failure.txt` and a
   receipt with `key_state: "revoked"` and the key ids, never key bytes.
4. **Receipts list ids.** `receipt_version` 6 adds optional `key_ids` and
   `key_state` (`ready` or `revoked`). Unencrypted jobs omit both.

## Consequences

- A scan of published outputs after a keyed job does not contain the
  input plaintext.
- Scratch trees still hold plaintext while the worker runs; they are
  removed after complete/fail. Encrypted-volume scratch is follow-up.
- Mizuchi parquet modular encryption is available on the bumped pin.
  Current kako SQL outputs are CSV, so the runner envelope is the
  encryption that lands on disk.
- `sekai-keys` is a git pin until it is on crates.io.

## Rejected alternatives

- Passing the DEK through the sandbox environment. `/proc/self/environ`
  is readable from guest code (ADR 0013).
- Putting key bytes in `manifest.json`. The scratch tree is on disk.
- An encrypted scratch volume. Not portable for the default test suite.
