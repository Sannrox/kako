#!/usr/bin/env bash
# Push the packaged SQL image as ghcr.io/sannrox/kako:<version> and print the
# `image=<name>=<digest>` output line the manifest step reads. Only a release
# tag's version is ever pushed, so a mutable tag such as `latest` cannot be.
# Docker's own output goes to stderr.
#
# Usage: bash scripts/release/push-image.sh <tag> <image-tar> >>"$GITHUB_OUTPUT"
set -euo pipefail

fail() {
  printf 'push-image: %s\n' "$1" >&2
  exit 1
}

[[ $# -eq 2 ]] || fail "usage: $0 <tag> <image-tar>"
tag="$1" tar="$2"
[[ "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-(rc|beta)\.[0-9]+)?$ ]] || fail "invalid release tag: ${tag}"
version="${tag#v}"
image="ghcr.io/sannrox/kako:${version}"

docker load -i "$tar" >&2
docker tag "kako-sql:${version}" "$image" >&2
docker push "$image" >&2
digest="$(docker inspect --format '{{index .RepoDigests 0}}' "$image")"
printf 'image=%s=%s\n' "$image" "${digest#*@}"
