# ADR 0013: Job-scoped credentials stay in the runner process

- Status: Accepted
- Date: 2026-10-18
- Relates: [0002](0002-sandboxed-transform-runner.md), [0004](0004-quota-authority.md), [0012](0012-governed-receipt-mapping.md)
- Source: [Issue #30](https://github.com/Sannrox/kako/issues/30)

## Context

Runs are sandboxed with network denied and only staged paths readable. A
remote job source still needs to read inputs and write outputs on the job's
behalf. A long-lived service credential would let one job touch another
job's data, and anything the runner inherits could reach user code: the
`linux_native` sandbox grants `/proc` (the engine reads its own cgroup), so
`/proc/self/environ` of the sandboxed worker is readable from SQL or Python.

## Decision

1. **One credential per claimed job.** A remote source mints a credential
   right after the claim, scoped to that job only:
   `kako_core::credential::CredentialScope::for_job` (read the job's input
   names, write its output names). Its lifetime is the job's wall clock plus
   a write-back grace (`ttl_for`). The source refuses a minted credential
   whose scope is broader or whose expiry is later, and hands the claim back.
2. **The runner process holds it.** The credential lives in the job source
   (`SekaiSource` keeps it beside the fence). It is not part of `Job`, the
   worker manifest, the scratch tree, or the sandbox environment. `Secret`
   redacts itself in `Debug` and `Display`.
3. **The guest environment is explicit.** `kako-sandbox` drops the runner's
   inherited environment and passes only variables the caller set on the
   command (scratch paths, interpreter settings). Landlock already denies
   `/proc/<pid>/environ` and `mem` of the unsandboxed runner.
4. **Revoke when the claim ends.** Write-back, failure report, hand-back,
   heartbeat failure, and fence loss all end the claim and revoke its
   credential. A transport error that keeps the claim for a retry keeps the
   credential too. Expiry bounds a credential whose revoke fails.
5. **Receipts carry the scope id.** `receipt_version` 4 adds optional
   `credential_scope`, the issuer's id for the credential. The secret never
   appears on a receipt. Local file jobs omit the field.

## Consequences

- A leaked credential is limited to one job's inputs and outputs and expires
  shortly after the run.
- A remote transport implements `mint_credential` and `revoke_credential`
  and presents the credential on `write_outputs`.
- Inherited variables no longer reach the worker. A future engine that needs
  one sets it on the command explicitly.
- Recording an agent actor on the credential scope (for delegated tokens)
  is follow-up work once claims carry an actor chain.

## Alternatives considered

- **A shared service credential for the runner pool.** Rejected: one
  compromised run could read or write every tenant's data.
- **Pass the credential to the worker and let it fetch inputs.** Rejected:
  the worker runs user code in the same sandbox.
- **Mint in the run loop through a separate broker trait.** Rejected for
  now: only the job source knows how to talk to its store, and the local
  source needs no credential. A shared broker can be extracted when a
  second remote source exists.
