#!/usr/bin/env bash
# Check the v* release path without publishing: script self-tests plus the
# workflow invariants that keep a release on main's workflow and the signed
# tag's commit, and keep dry runs from minting attestations.
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
dispatcher=.github/workflows/release-dispatch.yml

# The dispatcher only turns a v* tag push into a main run of release.yml: no
# checkout, no actions, no other trigger, no write access but actions.
trigger="$(sed -n '/^on:/,/^permissions:/p' "$dispatcher" | grep -vE '^[[:space:]]*(#|$)')"
[[ "$trigger" == $'on:\n  push:\n    tags: ["v*"]\npermissions:' ]] || fail "${dispatcher} must trigger only on v* tag pushes"
grep -F 'actions: write' "$dispatcher" >/dev/null || fail "${dispatcher} must be able to dispatch workflows"
grep -E '^[[:space:]]*(-[[:space:]]+)?uses:|contents: write' "$dispatcher" >/dev/null && fail "${dispatcher} must only dispatch"
# shellcheck disable=SC2016 # literal workflow text
[[ "$(grep -E '^[[:space:]]*(-[[:space:]]+)?run:' "$dispatcher" | sed -E 's/^[[:space:]]*(-[[:space:]]+)?//')" == 'run: gh workflow run release.yml --ref main -f tag="$TAG"' ]] ||
  fail "${dispatcher} must run only: gh workflow run release.yml --ref main -f tag=\"\$TAG\""

# release.yml runs from a dispatch (main for a tag) or as a pull request dry
# run; a tag push never runs it.
grep -E '^  (push|release):|^[[:space:]]+tags:' "$workflow" >/dev/null &&
  fail "${workflow} must not run on push; release-dispatch.yml starts it from main"
# shellcheck disable=SC2016 # literal workflow text
for needle in \
  'group: release-${{ inputs.tag || github.ref }}' \
  'cancel-in-progress: false' \
  '[[ "$WORKFLOW_REF" == refs/heads/main ]]' \
  "ref: \${{ inputs.tag && format('refs/tags/{0}', inputs.tag) || '' }}" \
  'git fetch --force origin "refs/tags/${TAG}:refs/tags/${TAG}" "+refs/heads/main:refs/remotes/origin/main"' \
  'sha="$(git rev-parse "${TAG}^{commit}")"' \
  'RELEASE_TRUST_REF=origin/main bash "${MAIN_TREE}/scripts/verify-release-tag.sh" verify "$TAG" "$sha"' \
  'git merge-base --is-ancestor "$sha" refs/remotes/origin/main' \
  'release-manifest.py' \
  '--git-sha "$SHA"' \
  'changelog-notes.py' \
  'actions/attest-sbom@' \
  '[[ "$TAG" == *-* ]]' \
  'kind=(--prerelease --latest=false)' \
  'gh release create "$TAG" "${files[@]}" --draft --verify-tag "${kind[@]}"' \
  'gh release edit "$TAG" --draft=false --prerelease --latest=false' \
  'needs: [verify-tag, package, attest, push-image]' \
  "needs.package.result == 'success'" \
  "needs.attest.result == 'success'"; do
  grep -F -- "$needle" "$workflow" >/dev/null || fail "${workflow} is missing: ${needle}"
done
# Every job that can attest, push, or publish runs only for a verified tag.
for job in push-image attest publish; do
  sed -n "/^  ${job}:/,/^  [a-z-]*:\$/p" "$workflow" | grep -F "if: needs.verify-tag.outputs.tag != ''" >/dev/null ||
    fail "${workflow} job ${job} must run only for a verified tag"
done
# Every job after verify-tag checks out the release commit and checks HEAD.
checkouts="$(grep -c 'uses: actions/checkout@' "$workflow")"
# shellcheck disable=SC2016 # literal workflow text
sha_checkouts="$(grep -c 'ref: ${{ needs.verify-tag.outputs.sha }}' "$workflow")"
# shellcheck disable=SC2016 # literal workflow text
head_checks="$(grep -c 'run: test "$(git rev-parse HEAD)" = "$SHA"' "$workflow")"
((checkouts == sha_checkouts + 2)) || fail "${workflow} must check out the release commit in every job after verify-tag"
((head_checks == sha_checkouts)) || fail "${workflow} must check HEAD after every release-commit checkout"
grep -E 'github\.sha|github\.ref_name|github\.event_name|GITHUB_SHA|GITHUB_REF' "$workflow" >/dev/null &&
  fail "${workflow} must use the tagged commit and tag input, not the event SHA or ref name"
grep -E -- '--latest( |$|")' "$workflow" >/dev/null && fail "${workflow} must never force a release to latest"
(($(grep -c 'is already published; cut a new tag instead of re-running' "$workflow") >= 3)) ||
  fail "${workflow} must refuse a published release early, when drafting, and before publishing"
sed -n '1,/^jobs:/p' "$workflow" | grep -E 'attestations: write|id-token: write|packages: write' >/dev/null &&
  fail "${workflow} must not grant write scopes at workflow level"
grep -E ':latest\b|--tag[= ]latest' "$workflow" >/dev/null && fail "${workflow} must not produce a latest tag"

printf 'validate-release passed\n'
