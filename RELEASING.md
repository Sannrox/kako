# Releasing kako

Family versioning, tagging, and release policy:
[tenkai ADR 0040](https://github.com/Sannrox/tenkai/blob/main/docs/decisions/0040-family-versioning-and-release-policy.md).
What a release ships: [ADR 0008](docs/decisions/0008-release-artifacts.md).
This page is how a maintainer tags this repository.

## Prepare

1. Choose the version. Feature pull requests never pick one.
2. Draft notes with `bash scripts/changelog-draft.sh <version>` and commit
   them as the `## [<version>]` section of `CHANGELOG.md`. Pull requests do
   not edit `CHANGELOG.md` otherwise.
3. Land the release pull request: `[workspace.package] version` in
   `Cargo.toml`, `Cargo.lock`, `deploy/tenkai.toml`, and the CHANGELOG section
   move together. `bash scripts/validate-release.sh` checks the release path
   without publishing.

## Tag

Agents never push `v*` tags. On the merged release commit the maintainer
creates an annotated tag signed with a key listed in
`security/release-tag-fingerprints` (public key in
`security/release-tag-key.asc`):

```bash
git tag -s vX.Y.Z -m vX.Y.Z
git push origin vX.Y.Z
```

A tag is never moved, deleted, or reused; a bad release gets a new patch
version. The tagged commit must be on `main`; there are no hotfix tags on
other branches.

## Release candidates

Cut `vX.Y.Z-rc.N` (or `-beta.N`) before a final tag. The rc's release pull
request sets the `[workspace.package]` version to `X.Y.Z-rc.N` and adds a
dated `## [X.Y.Z-rc.N]` section to `CHANGELOG.md`. The final `vX.Y.Z` needs
its own section; it never reuses the rc notes.

A tag containing `-` publishes as a GitHub pre-release and is never marked
latest. A failed release candidate costs only an rc number. A final tag on a
commit whose version is still an rc is refused.

## Start, retry, and dry run

The tag push runs `.github/workflows/release-dispatch.yml`, which does only
this:

```bash
gh workflow run release.yml --ref main -f tag=vX.Y.Z
```

So a release always runs **main's** `release.yml` and tag verifier, never the
copies at the tagged commit. `release.yml` refuses a tagged run that does not
start from `main`.

To retry a failed release, fix the workflow on `main` and run the same
command for the same tag. A retry picks up fixes to `release.yml` and to the
tag verifier only. Everything else (the release scripts, `CHANGELOG.md`, the
Dockerfiles, and the sources) comes from the tagged commit, so a bug there
needs a new rc or patch version. The release script checks in the run come
from the tagged commit too; main's `release.yml` itself was checked by the
pull request that changed it.

A re-run resumes the draft release: it deletes the draft's earlier assets and
uploads the rebuilt ones. Every job that pushes the image, attests, or
changes the release first refuses a release that is already published, and
fails when the release cannot be read, so "Re-run failed jobs" on an old run
cannot touch a published release either. Only one run per tag executes at a
time, and a tagged run is never cancelled.

Pull requests and `gh workflow run release.yml --ref <branch>` without a tag
are dry runs: they run the release script checks and packaging only, and
never push an image, attest, or touch a GitHub Release.

## What the tag runs

`.github/workflows/release.yml`, from `main`:

1. Checks out the tag and fetches the annotated tag object. Main's
   `scripts/verify-release-tag.sh` refuses a lightweight or unsigned tag, a
   signer outside the fingerprint list, an expired or revoked key, or a tag
   that is not the tagged commit's workspace version. The key and fingerprint
   list are read from `main`, so changing them takes a reviewed pull request.
   The tagged commit must be on `main`, and a published release for the tag
   stops the run.
2. Every later job checks out that commit and confirms HEAD is that commit.
   The release script checks, the packages, the image tag, the gate flags and
   the manifest `git_sha` all come from it.
3. Packages the runner, runtime images, and Python catalog, and writes a
   CycloneDX SBOM (`kako.cdx.json`, Rust crates plus the Python lockfile).
4. Pushes `ghcr.io/sannrox/kako:<version>` (never `latest`) and records its
   digest.
5. Attests build provenance and the SBOM.
6. Writes `kako-release.json`, the family release manifest: component,
   version, git SHA, file and image digests, attestation URLs, and gate flags
   (`risk:breaking`, `risk:migration`, `risk:realm`, or the matching `gate:*`
   labels on every pull request squashed in since the previous final tag). The
   manifest gets its own provenance attestation, which is its signature for
   Tenkai.
7. Drafts the GitHub Release with `gh release create <tag> --verify-tag` from
   the CHANGELOG section, or resumes the draft an earlier run left, attaches
   every file above, then publishes it, as a pre-release for a tag containing
   `-`.

## Provenance

Attestations record the workflow that built the artifacts, which is main's
`release.yml` at `refs/heads/main`, not the tag. The signed tag and the
manifest `git_sha` (the tagged commit) pin the source. The `v*` tag ruleset
from the family policy (create limited to maintainers, no update or delete)
keeps a tag pointing at the commit that was signed.

Verify a downloaded file with
`gh attestation verify <file> --repo Sannrox/kako`.
