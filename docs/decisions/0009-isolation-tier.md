# ADR 0009: Isolation-tier selection is a job-source floor; kako fails closed

- Status: Accepted
- Date: 2026-10-12
- Relates: [0002](0002-sandboxed-transform-runner.md)
- Source: [Issue #32](https://github.com/Sannrox/kako/issues/32)
- Follow-up: [Issue #3](https://github.com/Sannrox/kako/issues/3) (guest spawn), [Issue #33](https://github.com/Sannrox/kako/issues/33) (kekkai pin)
- Upstream: [kekkai ADR 0003](https://github.com/Sannrox/kekkai/blob/main/docs/decisions/0003-guest-backend-interface.md)

## Context

Every kako run today selects kekkai `linux_native`: Landlock, seccomp, and
cgroup in a child that shares the host kernel and the runner's OS user. That
is enough for trusted local SQL. It is not enough for untrusted Python from
mutually distrusting users. kekkai already says callers rank isolation and
fail closed; it does not put a minimum-tier rank on `Settings`. kako still
has no job field for that floor, no receipt field for what ran, and no rule
for refusing a job the host cannot isolate.

Guest enforcement stays in kekkai. kako only chooses a backend and aborts
when the choice cannot be applied.

## Decision

1. **Named tiers, weakest to strongest.** kako understands three isolation
   tiers:
   - `linux_native` — in-process kekkai `Backend::LinuxNative` (shared kernel
     and OS user).
   - `userspace` — kekkai `Backend::Userspace` (userspace guest). Compilable
     after the pin in Issue #33; spawned by Issue #3.
   - `kvm` — a KVM/microVM backend, only after kekkai ships one. Requiring it
     before that adapter exists is a runner failure.

2. **The job source requires a minimum.** Job JSON and `Job.isolation` carry
   `isolation`. The value is a floor, not an exact runtime class. Omitted
   means `linux_native`, which is today's default. The local CLI does not
   invent a stronger floor.

3. **kako selects the weakest available backend that meets the floor.** Ranking
   lives in kako. kekkai `Settings` stays an explicit backend. If several
   backends meet the floor, kako picks the weakest so a trusted job does not
   pay guest startup unless the source asked for it.

4. **Fail closed. Never downgrade.** If the host or the pinned kekkai cannot
   provide a backend at or above the floor, the run is `Failure::Runner`. No
   outputs are written. Health errors, a missing guest helper, or an unpinned
   KVM adapter never become `linux_native`.

5. **Receipts record what ran.** Receipt JSON `isolation` is the tier that
   actually ran, not the requested floor when they differ. Adding the field
   bumps `receipt_version` to 3. A missing or weaker backend never appears as
   a successful receipt.

6. **Guest policy stays in kekkai.** kako does not reimplement guest spawn,
   mounts, or helpers. It maps `userspace` / `kvm` onto the kekkai backend of
   the same rank once that backend exists in the pin.

## Consequences

- Untrusted work is a job-source decision: set `isolation` to `userspace`
  (or `kvm` later). A pool that only has `linux_native` refuses those jobs.
- Default local SQL stays `linux_native` and keeps working on hosts that
  already pass `kako doctor`.
- Issue #3 wires `Backend::Userspace` spawn against this floor. Issue #33
  moves the pin so that type exists. This ADR does not spawn a guest.
- Operators do not get a host-wide override that silently raises or lowers
  the floor. The job source remains the authority.

## Alternatives considered

- **Always guest.** Rejected: trusted local SQL would pay guest startup on
  every run. The source should require the stronger floor when the code is
  untrusted.
- **Always in-process (`linux_native` for every job).** Rejected: that is
  today's implicit selection, and it blocks untrusted code from mutually
  distrusting users. Issue #3 would have no policy to implement.
- **A minimum-tier rank inside kekkai `Settings`.** Rejected: kekkai ADR 0003
  already keeps ranking with the caller. kako is that caller.
- **Silent fallback to a weaker backend when health fails.** Rejected: a
  successful run would then claim isolation the host did not enforce.
