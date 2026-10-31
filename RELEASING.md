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

Pre-releases are `vX.Y.Z-rc.N` or `vX.Y.Z-beta.N`. A tag is never moved,
deleted, or reused; a bad release gets a new patch version.

## What the tag runs

`.github/workflows/release.yml`:

1. `scripts/verify-release-tag.sh` refuses a lightweight or unsigned tag, a
   signer outside the fingerprint list, a commit that is not on `main` or a
   `release/v*` branch, or a tag that is not the workspace version. The key
   and fingerprint list are read from `main`, so changing them takes a
   reviewed pull request.
2. Packages the runner, runtime images, and Python catalog, and writes a
   CycloneDX SBOM (`kako.cdx.json`, Rust crates plus the Python lockfile).
3. Pushes `ghcr.io/sannrox/kako:<version>` (never `latest`) and records its
   digest.
4. Attests build provenance and the SBOM.
5. Writes `kako-release.json`, the family release manifest: component,
   version, git SHA, file and image digests, attestation URLs, and gate flags
   (`risk:breaking`, `risk:migration`, `risk:realm`, or the matching `gate:*`
   labels on every pull request squashed in since the previous final tag). The manifest gets
   its own provenance attestation, which is its signature for Tenkai.
6. Drafts the GitHub Release from the CHANGELOG section, attaches every file
   above, then publishes it. A rerun finishes an existing draft and refuses a
   published release.

The workflow and the verifier run from the tagged commit, so the `v*` tag
ruleset from the family policy (create limited to maintainers, no update or
delete) is what keeps an unreviewed commit from being released. The
verifier's allowlist check holds only together with that ruleset.

Pull requests and `workflow_dispatch` run the script checks and packaging
only. They never verify a tag, push an image, attest, or create a Release.

Verify a downloaded file with
`gh attestation verify <file> --repo Sannrox/kako`.
