#!/usr/bin/env python3
"""Write a CycloneDX 1.5 SBOM for a kako release.

Components are the non-workspace crates `cargo metadata --locked` resolves
and the pinned packages in the Python lockfile (default
runtime/python/requirements.txt), which ship in the Python runtime image and
catalog.

Usage:
  release-sbom.py OUT [REQUIREMENTS]
  release-sbom.py --self-test
"""

from __future__ import annotations

import json
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
REQUIREMENT_RE = re.compile(r"^([A-Za-z0-9][A-Za-z0-9._-]*)(?:\[[^\]]*\])?==([^\s;\\]+)", re.M)


# Cargo license strings are not guaranteed SPDX IDs, so emit them as an
# expression (a free string in CycloneDX), with the legacy "/" as " OR ".
def license_entry(raw: str) -> dict:
    return {"expression": " OR ".join(part.strip() for part in raw.split("/"))}


def cargo_components(meta: dict) -> tuple[dict, list[dict]]:
    workspace = set(meta["workspace_members"])
    root = None
    components = []
    for pkg in meta["packages"]:
        purl = f"pkg:cargo/{pkg['name']}@{pkg['version']}"
        if pkg["id"] in workspace:
            if pkg["name"] == "kako":
                root = {"type": "application", "bom-ref": purl, "name": "kako", "version": pkg["version"], "purl": purl}
            continue
        component = {"type": "library", "bom-ref": purl, "name": pkg["name"], "version": pkg["version"], "purl": purl}
        if pkg.get("license"):
            component["licenses"] = [license_entry(pkg["license"])]
        components.append(component)
    if root is None:
        raise SystemExit("cargo metadata did not include the kako package")
    return root, components


def python_components(requirements: str) -> list[dict]:
    components = []
    for name, version in REQUIREMENT_RE.findall(requirements):
        purl = f"pkg:pypi/{re.sub(r'[-_.]+', '-', name).lower()}@{version}"
        components.append({"type": "library", "bom-ref": purl, "name": name, "version": version, "purl": purl})
    return components


def bom(meta: dict, requirements: str) -> dict:
    root, crates = cargo_components(meta)
    return {
        "bomFormat": "CycloneDX",
        "specVersion": "1.5",
        "version": 1,
        "metadata": {"component": root},
        "components": sorted(crates + python_components(requirements), key=lambda c: c["bom-ref"]),
    }


def self_test() -> None:
    meta = {
        "workspace_members": ["path+file:///w/crates/kako#0.1.0", "path+file:///w/crates/kako-core#0.1.0"],
        "packages": [
            {"id": "path+file:///w/crates/kako#0.1.0", "name": "kako", "version": "0.1.0", "license": "Apache-2.0"},
            {"id": "path+file:///w/crates/kako-core#0.1.0", "name": "kako-core", "version": "0.1.0"},
            {"id": "registry+x#serde@1.0.0", "name": "serde", "version": "1.0.0", "license": "MIT OR Apache-2.0"},
            {"id": "registry+x#memchr@2.0.0", "name": "memchr", "version": "2.0.0", "license": "MIT/Unlicense"},
        ],
    }
    requirements = "numpy==2.1.0 \\\n    --hash=sha256:abc\nonnx_runtime==1.20.0 ; python_version >= '3.12'\nZope.Interface[x]==6.0\n# comment==1\n"
    out = bom(meta, requirements)
    assert out["metadata"]["component"]["name"] == "kako"
    by_ref = {c["bom-ref"]: c for c in out["components"]}
    assert set(by_ref) == {
        "pkg:cargo/serde@1.0.0",
        "pkg:cargo/memchr@2.0.0",
        "pkg:pypi/numpy@2.1.0",
        "pkg:pypi/onnx-runtime@1.20.0",
        "pkg:pypi/zope-interface@6.0",
    }, set(by_ref)
    assert by_ref["pkg:cargo/serde@1.0.0"]["licenses"] == [{"expression": "MIT OR Apache-2.0"}]
    assert by_ref["pkg:cargo/memchr@2.0.0"]["licenses"] == [{"expression": "MIT OR Unlicense"}]
    try:
        bom({"workspace_members": [], "packages": []}, "")
    except SystemExit as exc:
        assert "kako package" in str(exc)
    else:
        raise AssertionError("expected missing kako package to fail")
    print("release-sbom self-test passed")


def main(argv: list[str]) -> None:
    if argv == ["--self-test"]:
        self_test()
        return
    if len(argv) not in (1, 2):
        raise SystemExit("usage: release-sbom.py OUT [REQUIREMENTS]")
    meta = json.loads(
        subprocess.check_output(["cargo", "metadata", "--locked", "--format-version", "1"], cwd=ROOT, text=True)
    )
    requirements = Path(argv[1] if len(argv) == 2 else ROOT / "runtime/python/requirements.txt").read_text()
    out = Path(argv[0])
    out.write_text(json.dumps(bom(meta, requirements), indent=2) + "\n")
    print(out)


if __name__ == "__main__":
    main(sys.argv[1:])
