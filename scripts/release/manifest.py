#!/usr/bin/env python3
"""Write the family release manifest Tenkai admits for one kako tag.

The release workflow runs this after attestation, then attests the manifest
itself, so Tenkai receives a signed record of: component, version, git SHA,
file and image digests, attestation URLs, and gate flags. Images are always
referenced by digest; a `latest` tag is refused.

With --labels-from-history, gate flags come from the labels of every pull
request squashed into the release range (see release_range), read with `gh`
from the current git checkout. Any lookup error, or a commit in the range
without a pull request reference, fails instead of dropping a gate.

Usage:
  manifest.py --version V --git-sha SHA --dist DIR --out FILE
      [--tag vV] [--image NAME=sha256:...] [--attestation URL]
      [--label NAME]... [--labels-from-history]
"""

from __future__ import annotations

import argparse
import hashlib
import json
import pathlib
import re
import subprocess
import sys

SCHEMA = "tenkai.family-release-manifest.v1"
COMPONENT = "kako"
MANIFEST_NAME = "kako-release.json"
# Files in dist that describe the release rather than ship in it.
SKIP_NAMES = {"SHA256SUMS", "RELEASE_NOTES.md", MANIFEST_NAME}
# Merged pull request labels that hold a release for human approval.
GATE_LABELS = {
    "risk:breaking": "breaking",
    "gate:break": "breaking",
    "risk:migration": "migration",
    "gate:mig": "migration",
    "risk:realm": "realm",
    "gate:realm": "realm",
}
VERSION_RE = re.compile(r"[0-9]+\.[0-9]+\.[0-9]+(-(rc|beta)\.[0-9]+)?")
DIGEST_RE = re.compile(r"sha256:[0-9a-f]{64}")
# A squash-merged pull request ends its subject with "(#N)".
PR_REF_RE = re.compile(r"\(#([0-9]+)\)$")


def sha256_file(path: pathlib.Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1 << 20), b""):
            digest.update(chunk)
    return "sha256:" + digest.hexdigest()


def gate_flags(labels: list[str]) -> list[str]:
    return sorted({GATE_LABELS[n] for n in (l.strip().lower() for l in labels) if n in GATE_LABELS})


def parse_image(raw: str) -> dict[str, str]:
    name, _, digest = raw.partition("=")
    if not name or not DIGEST_RE.fullmatch(digest):
        raise SystemExit(f"image must be NAME=sha256:<64 hex>, got {raw}")
    if name.endswith(":latest"):
        raise SystemExit("images must not use a latest tag")
    return {"name": name, "digest": digest}


def build_manifest(
    *,
    version: str,
    git_sha: str,
    tag: str | None,
    dist: pathlib.Path,
    images: list[dict[str, str]],
    attestations: list[str],
    labels: list[str],
) -> dict:
    if not re.fullmatch(r"[0-9a-f]{40}", git_sha):
        raise SystemExit("git_sha must be a full lowercase SHA")
    if not VERSION_RE.fullmatch(version):
        raise SystemExit(f"invalid version: {version}")
    if tag is not None and tag != f"v{version}":
        raise SystemExit(f"tag {tag} does not match version {version}")
    files = [
        {"name": p.name, "digest": sha256_file(p)}
        for p in sorted(dist.iterdir())
        if p.is_file() and p.name not in SKIP_NAMES
    ]
    if not files:
        raise SystemExit(f"no release files in {dist}")
    return {
        "schema": SCHEMA,
        "component": COMPONENT,
        "version": version,
        "git_sha": git_sha,
        "tag": tag,
        "binaries": files,
        "images": images,
        "attestations": attestations,
        "gate_flags": gate_flags(labels),
    }


def git(*args: str) -> str:
    return subprocess.run(["git", *args], check=True, capture_output=True, text=True).stdout


def release_range(sha: str) -> str:
    """The commits whose pull requests can gate the release at sha.

    The baseline is the nearest final (non-pre-release) ancestor tag, so a
    final release also carries gates first seen in its release candidates.
    Without a final tag, the range starts at the first pull request: the
    commits before it are the repository bootstrap and carry no labels.
    """
    prev = subprocess.run(
        ["git", "describe", "--tags", "--abbrev=0", "--match", "v*", "--exclude", "*-*", f"{sha}^"],
        capture_output=True,
        text=True,
    )
    if prev.returncode == 0:
        return f"{prev.stdout.strip()}..{sha}"
    for line in git("log", "--reverse", "--format=%H %s", sha).splitlines():
        commit, _, subject = line.partition(" ")
        if PR_REF_RE.search(subject):
            return f"{commit}^..{sha}"
    raise SystemExit(f"no pull request commit reaches {sha}")


def pull_requests(subjects: str) -> list[int]:
    """Pull request numbers from `git log --format='%h %s'` lines."""
    lines = subjects.splitlines()
    unattributed = [line for line in lines if not PR_REF_RE.search(line)]
    if unattributed or not lines:
        raise SystemExit("commits without a pull request reference:\n" + "\n".join(unattributed))
    return sorted({int(PR_REF_RE.search(line).group(1)) for line in lines})


def labels_from_history(sha: str) -> list[str]:
    span = release_range(sha)
    labels = []
    for pr in pull_requests(git("log", "--format=%h %s", span)):
        found = subprocess.run(
            ["gh", "pr", "view", str(pr), "--json", "labels", "--jq", ".labels[].name"],
            capture_output=True,
            text=True,
        )
        if found.returncode != 0:
            raise SystemExit(f"cannot read labels of pull request #{pr}: {found.stderr.strip()}")
        labels += [name for name in found.stdout.splitlines() if name]
    return labels


def main(argv: list[str]) -> None:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--version", required=True)
    parser.add_argument("--git-sha", required=True)
    parser.add_argument("--tag")
    parser.add_argument("--dist", required=True, type=pathlib.Path)
    parser.add_argument("--out", required=True, type=pathlib.Path)
    parser.add_argument("--image", action="append", default=[])
    parser.add_argument("--attestation", action="append", default=[])
    parser.add_argument("--label", action="append", default=[])
    parser.add_argument("--labels-from-history", action="store_true")
    args = parser.parse_args(argv)
    manifest = build_manifest(
        version=args.version,
        git_sha=args.git_sha,
        tag=args.tag,
        dist=args.dist,
        images=[parse_image(raw) for raw in args.image],
        attestations=args.attestation,
        labels=args.label + (labels_from_history(args.git_sha) if args.labels_from_history else []),
    )
    args.out.write_text(json.dumps(manifest, indent=2, sort_keys=True) + "\n")
    print(args.out)


if __name__ == "__main__":
    main(sys.argv[1:])
