import unittest
import uuid

from tests.release import load

sbom = load("sbom")

KAKO, CORE = "path+file:///w/crates/kako#0.1.0", "path+file:///w/crates/kako-core#0.1.0"


def dep(pkg: str, *kinds: str | None) -> dict:
    return {"pkg": pkg, "dep_kinds": [{"kind": k, "target": None} for k in kinds]}


PACKAGES = [
    {"id": KAKO, "name": "kako", "version": "0.1.0", "license": "Apache-2.0"},
    {"id": CORE, "name": "kako-core", "version": "0.1.0"},
    {"id": "r#serde@1.0.0", "name": "serde", "version": "1.0.0", "license": "MIT OR Apache-2.0"},
    {"id": "r#memchr@2.0.0", "name": "memchr", "version": "2.0.0", "license": "MIT/Unlicense"},
    {"id": "r#proptest@1.0.0", "name": "proptest", "version": "1.0.0", "license": "MIT"},
]
META = {
    "workspace_members": [KAKO, CORE],
    "packages": PACKAGES,
    "resolve": {
        "nodes": [
            {"id": KAKO, "deps": [dep(CORE, None), dep("r#memchr@2.0.0", "build"), dep("r#proptest@1.0.0", "dev")]},
            {"id": CORE, "deps": [dep("r#serde@1.0.0", None, "dev")]},
        ]
        + [{"id": p["id"], "deps": []} for p in PACKAGES[2:]]
    },
}
REQUIREMENTS = (
    "numpy==2.1.0 \\\n    --hash=sha256:abc\n"
    "onnx_runtime==1.20.0 ; python_version >= '3.12'\n"
    "Zope.Interface[x]==6.0\n"
    "# comment==1\n"
)


class Bom(unittest.TestCase):
    def test_lists_shipped_crates_and_pinned_python_packages(self):
        doc = sbom.bom(META, REQUIREMENTS)
        self.assertEqual(doc["metadata"]["component"]["purl"], "pkg:cargo/kako@0.1.0")
        by_ref = {c["bom-ref"]: c for c in doc["components"]}
        # Workspace crates and dev-only dependencies do not ship.
        self.assertEqual(
            set(by_ref),
            {
                "pkg:cargo/serde@1.0.0",
                "pkg:cargo/memchr@2.0.0",
                "pkg:pypi/numpy@2.1.0",
                "pkg:pypi/onnx-runtime@1.20.0",
                "pkg:pypi/zope-interface@6.0",
            },
        )
        self.assertEqual(by_ref["pkg:cargo/memchr@2.0.0"]["licenses"], [{"expression": "MIT OR Unlicense"}])

    def test_serial_number_is_a_stable_content_hash(self):
        serial = sbom.bom(META, REQUIREMENTS)["serialNumber"]
        self.assertEqual(uuid.UUID(serial.removeprefix("urn:uuid:")).version, 5)
        self.assertEqual(sbom.bom(META, REQUIREMENTS)["serialNumber"], serial)
        self.assertNotEqual(sbom.bom(META, REQUIREMENTS + "another==1.0.0\n")["serialNumber"], serial)

    def test_refuses_metadata_without_the_kako_package(self):
        with self.assertRaisesRegex(SystemExit, "kako package"):
            sbom.bom({"workspace_members": [], "packages": [], "resolve": {"nodes": []}}, "")


if __name__ == "__main__":
    unittest.main()
