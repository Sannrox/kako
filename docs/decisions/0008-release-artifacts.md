# ADR 0008: Releases ship GitHub artifacts; crates stay unpublished

- Status: Accepted
- Date: 2026-10-12
- Relates: [0005](0005-python-runtime-image.md), [0006](0006-python-package-policy.md), [0011](0011-runner-pool-drain.md)
- Source: [Issue #31](https://github.com/Sannrox/kako/issues/31)

## Context

kako needs versioned, reversible releases: a runner people can install, and
the SQL and Python runtimes that jobs pin. The workspace crate currently
sets `publish = false`. The crate graph pins the sandbox and the opt-in SQL
engine as git revisions, which a registry crate cannot depend on. A long-running
pool's health and drain are a separate Issue.

## Decision

1. **What ships on a `v*` tag.** A GitHub Release with:
   - the `kako` runner binary (Linux, the host triple CI builds);
   - a SQL runtime image (container with that binary; DuckDB is bundled in it);
   - a Python runtime image built from `runtime/python/Dockerfile` (hashed
     lockfile, no packages at run time);
   - a Python catalog archive that unpacks into `KAKO_PYTHON_IMAGES` as
     `<lockfile-hex>/requirements.txt` (digest equals the directory name),
     `<lockfile-hex>/site-packages` installed from that lockfile, and a
     `current` pointer ([ADR 0006](0006-python-package-policy.md)). A local
     dry-run without `--with-site-packages` writes the lockfile layout only.
   - `SHA256SUMS` for those files;
   - GitHub artifact attestations on the files.
2. **Crates stay `publish = false`** until every runtime dependency is a
   registry crate. Flipping that bit while git pins remain would produce a
   crate that cannot be published.
3. **Dry-run.** `scripts/release-dry-run.sh`, pull requests that touch the
   release path, and `workflow_dispatch` build the same files and skip
   attestations and the GitHub Release. A real tag (`v*`) passes `--version`
   from the tag, which must match `Cargo.toml`, attests the files, and
   creates the Release from `CHANGELOG.md` for that version.
4. **Signing.** Provenance is GitHub artifact attestations on tagged
   artifacts only. Pull requests do not mint attestations. There is no
   long-lived project signing key in this repository.

## Consequences

- Operators install the binary and, for Python, either the image or the
  catalog tree. Receipts still identify the Python image by lockfile digest,
  not by a registry location.
- Cutting a tag is a maintainer action. This decision records the artifacts
  and the workflow; it does not cut a version.
- Runner-pool drain and rollback are [ADR 0011](0011-runner-pool-drain.md).

## Alternatives considered

- **Publish the crate to a registry now.** Rejected: git-pinned sandbox and
  SQL engine dependencies are not registry crates.
- **Ship only the binary and leave images to operators.** Rejected in
  [ADR 0006](0006-python-package-policy.md): the Python package set would not
  be a kako release artifact.
- **Keep a project-owned signing key in CI secrets.** Rejected: GitHub
  artifact attestations already bind the files to the workflow and commit.
