#!/usr/bin/env python3
"""Write a CycloneDX 1.5 SBOM for a kako release.

Components are the non-workspace crates reachable from the `kako` package
through normal and build dependencies (not dev-dependencies) on the release
target, and the pinned packages in the Python lockfile (default
runtime/python/requirements.txt), which ship in the Python runtime image and
catalog.

Usage:
  sbom.py OUT [REQUIREMENTS]
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
import uuid
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
# The release workflow builds the runner on this target only.
TARGET = "x86_64-unknown-linux-gnu"
REQUIREMENT_RE = re.compile(r"^([A-Za-z0-9][A-Za-z0-9._-]*)(?:\[[^\]]*\])?==([^\s;\\]+)", re.M)


# Cargo license strings are not guaranteed SPDX IDs, so emit them as an
# expression (a free string in CycloneDX), with the legacy "/" as " OR ".
def license_entry(raw: str) -> dict:
    return {"expression": " OR ".join(part.strip() for part in raw.split("/"))}


def cargo_components(meta: dict) -> tuple[dict, list[dict]]:
    """The kako package and the non-workspace crates it links, from a
    platform-filtered `cargo metadata`."""
    packages = {pkg["id"]: pkg for pkg in meta["packages"]}
    workspace = set(meta["workspace_members"])
    root = next((i for i in workspace if packages[i]["name"] == "kako"), None)
    if root is None:
        raise SystemExit("cargo metadata did not include the kako package")
    deps = {node["id"]: node["deps"] for node in meta["resolve"]["nodes"]}
    seen, todo = {root}, [root]
    while todo:
        for dep in deps[todo.pop()]:
            if dep["pkg"] not in seen and any(k["kind"] in (None, "build") for k in dep["dep_kinds"]):
                seen.add(dep["pkg"])
                todo.append(dep["pkg"])
    components = []
    for pkg in (packages[i] for i in seen if i not in workspace):
        purl = f"pkg:cargo/{pkg['name']}@{pkg['version']}"
        component = {"type": "library", "bom-ref": purl, "name": pkg["name"], "version": pkg["version"], "purl": purl}
        if pkg.get("license"):
            component["licenses"] = [license_entry(pkg["license"])]
        components.append(component)
    kako = packages[root]
    purl = f"pkg:cargo/kako@{kako['version']}"
    return {"type": "application", "bom-ref": purl, "name": "kako", "version": kako["version"], "purl": purl}, components


def python_components(requirements: str) -> list[dict]:
    components = []
    for name, version in REQUIREMENT_RE.findall(requirements):
        purl = f"pkg:pypi/{re.sub(r'[-_.]+', '-', name).lower()}@{version}"
        components.append({"type": "library", "bom-ref": purl, "name": name, "version": version, "purl": purl})
    return components


def bom(meta: dict, requirements: str) -> dict:
    root, crates = cargo_components(meta)
    document = {
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "version": 1,
        "metadata": {"component": root},
        "components": sorted(crates + python_components(requirements), key=lambda c: c["bom-ref"]),
    }

    canonical = json.dumps(document, sort_keys=True, separators=(",", ":"))
    document["serialNumber"] = uuid.uuid5(uuid.NAMESPACE_URL, canonical).urn
    return document


def main(argv: list[str]) -> None:
    if len(argv) not in (1, 2):
        raise SystemExit("usage: sbom.py OUT [REQUIREMENTS]")
    meta = json.loads(
        subprocess.check_output(
            ["cargo", "metadata", "--locked", "--format-version", "1", "--filter-platform", TARGET], cwd=ROOT, text=True
        )
    )
    requirements = Path(argv[1] if len(argv) == 2 else ROOT / "runtime/python/requirements.txt").read_text()
    out = Path(argv[0])
    out.write_text(json.dumps(bom(meta, requirements), indent=2) + "\n")
    print(out)


if __name__ == "__main__":
    main(sys.argv[1:])
