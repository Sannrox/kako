#!/usr/bin/env bash
# Check the v* release path without publishing: script self-tests plus the
# workflow invariants that keep pull requests from minting attestations.
# Runs in the release workflow's `scripts` job and locally.
set -euo pipefail

cd "$(dirname "$0")/.."

fail() {
  printf 'validate-release: %s\n' "$1" >&2
  exit 1
}

python3 scripts/changelog-notes.py --self-test
python3 scripts/release-manifest.py --self-test
python3 scripts/release-sbom.py --self-test
bash scripts/verify-release-tag.sh self-test

version="$(sed -n '/^\[workspace.package\]/,/^\[/s/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
python3 scripts/changelog-notes.py "$version" >/dev/null || fail "CHANGELOG.md needs a non-empty [${version}] section"

grep -F 'release-sbom.py' scripts/release-dry-run.sh >/dev/null || fail "release-dry-run.sh must write the SBOM"
grep -F 'kako.cdx.json; do' scripts/release-dry-run.sh >/dev/null || fail "SHA256SUMS must cover the SBOM"

workflow=.github/workflows/release.yml
tag_gate="github.event_name == 'push' && startsWith(github.ref, 'refs/tags/v')"
for needle in \
  'verify-release-tag.sh verify' \
  'release-manifest.py' \
  'changelog-notes.py' \
  'actions/attest-sbom@' \
  "needs.package.result == 'success'" \
  "needs.attest.result == 'success'"; do
  grep -F -- "$needle" "$workflow" >/dev/null || fail "${workflow} is missing: ${needle}"
done
# Every job that can attest, push, or publish runs only on a v* tag push.
for job in verify-tag push-image attest publish; do
  sed -n "/^  ${job}:/,/^  [a-z-]*:\$/p" "$workflow" | grep -F "if: ${tag_gate}" >/dev/null ||
    fail "${workflow} job ${job} must run only on v* tag pushes"
done
sed -n '1,/^jobs:/p' "$workflow" | grep -E 'attestations: write|id-token: write|packages: write' >/dev/null &&
  fail "${workflow} must not grant write scopes at workflow level"
grep -E ':latest\b|--tag[= ]latest' "$workflow" >/dev/null && fail "${workflow} must not produce a latest tag"

printf 'validate-release passed\n'
