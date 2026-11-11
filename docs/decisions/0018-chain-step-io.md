# ADR 0018: Jobs declare inputs by upstream output digest

- Status: Accepted
- Date: 2026-11-06
- Relates: [0002](0002-sandboxed-transform-runner.md), [0016](0016-replay-jobs.md)
- Source: [Issue #126](https://github.com/Sannrox/kako/issues/126)

## Context

A caller may run two kako jobs in sequence and feed the first job's output
into the second. Receipts already record input and output content digests.
Without a way to name an input by that digest, the second job can only
point at a path, and a swapped file still runs.

kako stays a single-job runner. Ordering, retries, and DAG evaluation stay
with the caller ([ADR 0002](0002-sandboxed-transform-runner.md)). Replay
pins each step's inputs independently and does not feed steps into each
other ([ADR 0016](0016-replay-jobs.md)).

## Decision

1. **A job input is a path or a digest.** The local job file keeps string
   values. A relative path is staged as today. A `sha256:` digest is
   resolved by the job source from a prior run's output directory.
2. **The job source finds the bytes.** Local `kako run` looks up the digest
   in `--inputs-from` directories by reading each `receipt.json` and taking
   the matching output file. A missing store, a missing digest, or an
   unreadable receipt fails closed before the sandbox starts.
3. **Staging re-hashes.** When an input carries a declared digest, the
   staged copy must match. Tampering the file after the receipt was
   written is refused here, not by trusting the receipt index.
4. **Receipts already chain.** The second receipt's input digest equals the
   first receipt's output digest. The receipt schema is unchanged. A
   verifier needs only those two receipts.

## Consequences

- Two local jobs can chain by digest without a scheduler.
- Isolation fail-closed behavior is unchanged.
- The Sekai adapter still stages claim snapshot rows as path inputs. Live
  isolated-process proof stays the ignored `sekai_live` test.
- Adding `Input.digest` is a public struct-field change on `kako-core`.

## Alternatives considered

- **A content-addressed blob directory in kako.** Rejected: the previous
  run's output directory plus receipt is already the store.
- **A `JobSource` fetch-by-digest method.** Rejected: the source resolves
  the path when it yields the job; staging only verifies.
- **Path plus optional digest object in job JSON.** Rejected: a string is
  either a path or a digest, which keeps the existing map shape.
