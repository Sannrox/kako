#!/usr/bin/env bash
# Push the packaged SQL images as a multi-arch ghcr.io/sannrox/kako:<version>
# index (linux/amd64 and linux/arm64) and print the `image=<name>=<digest>`
# output line the manifest step reads. Only a release tag's version is ever
# pushed, so a mutable tag such as `latest` cannot be. Docker's own output
# goes to stderr.
#
# Usage: bash scripts/release/push-image.sh <tag> <tar> <tar> >>"$GITHUB_OUTPUT"
set -euo pipefail

fail() {
  printf 'push-image: %s\n' "$1" >&2
  exit 1
}

[[ $# -eq 3 ]] || fail "usage: $0 <tag> <tar> <tar>"
tag="$1"
[[ "$tag" =~ ^v[0-9]+\.[0-9]+\.[0-9]+(-(rc|beta)\.[0-9]+)?$ ]] || fail "invalid release tag: ${tag}"
version="${tag#v}"
image="ghcr.io/sannrox/kako:${version}"
local_tag="kako-sql:${version}"

have_amd64=0
have_arm64=0
for tar in "$2" "$3"; do
  [[ -f "$tar" ]] || fail "missing image tar: ${tar}"
  docker load -i "$tar" >&2
  arch="$(docker inspect --format '{{.Architecture}}' "$local_tag")"
  case "$arch" in
    amd64)
      [[ "$have_amd64" -eq 0 ]] || fail "duplicate amd64 image from ${tar}"
      have_amd64=1
      ;;
    arm64)
      [[ "$have_arm64" -eq 0 ]] || fail "duplicate arm64 image from ${tar}"
      have_arm64=1
      ;;
    *) fail "unsupported image architecture ${arch} from ${tar}" ;;
  esac
  docker tag "$local_tag" "${image}-${arch}" >&2
  docker push "${image}-${arch}" >&2
done
[[ "$have_amd64" -eq 1 && "$have_arm64" -eq 1 ]] || fail "need amd64 and arm64 images"

docker buildx imagetools create -t "$image" "${image}-amd64" "${image}-arm64" >&2
inspect="$(docker buildx imagetools inspect "$image")"
printf '%s\n' "$inspect" >&2
[[ "$inspect" == *linux/amd64* && "$inspect" == *linux/arm64* ]] || fail "manifest is missing linux/amd64 or linux/arm64"
digest="$(docker buildx imagetools inspect "$image" --format '{{printf "%s" .Manifest.Digest}}')"
[[ "$digest" == sha256:* && ${#digest} -eq 71 ]] || fail "missing digest for ${image}"
printf 'image=%s=%s\n' "$image" "$digest"
