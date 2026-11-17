import json
import os
import pathlib
import subprocess
import tempfile
import unittest

from tests.release import RELEASE, load

manifest = load("manifest")

SCRIPT = RELEASE / "manifest.py"
IMAGE = "ghcr.io/sannrox/kako:0.1.0=sha256:" + "b" * 64

# Prints the labels of the pull request in $1 from $FAKE_LABELS ("N=label,...")
# and fails for any number listed in $FAKE_FAIL.
FAKE_GH = """#!/usr/bin/env bash
pr="$3"
[[ " $FAKE_FAIL " == *" $pr "* ]] && { echo "HTTP 502" >&2; exit 1; }
for entry in $FAKE_LABELS; do
  [[ "${entry%%=*}" == "$pr" ]] && tr , '\\n' <<<"${entry#*=}"
done
exit 0
"""


def write_dist(dist: pathlib.Path) -> None:
    (dist / "kako-0.1.0-x86_64-unknown-linux-gnu.tar.gz").write_bytes(b"bin")
    (dist / "kako-sql-image.tar").write_bytes(b"tar")
    (dist / "kako.cdx.json").write_text("{}")
    for name in manifest.SKIP_NAMES:
        (dist / name).write_text("skip\n")


class Manifest(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory(prefix=f"mf{os.getpid()}-")
        self.addCleanup(tmp.cleanup)
        self.dist = pathlib.Path(tmp.name)
        self.args = dict(
            version="0.1.0", git_sha="a" * 40, tag="v0.1.0", dist=self.dist,
            images=[], attestations=[], labels=[],
        )

    def test_maps_gate_labels_and_ignores_others(self):
        flags = manifest.gate_flags(["risk:breaking", "area:ops", "GATE:mig", "gate:realm", "docs"])
        self.assertEqual(flags, ["breaking", "migration", "realm"])

    def test_images_need_a_digest_and_never_a_latest_tag(self):
        with self.assertRaisesRegex(SystemExit, "latest"):
            manifest.parse_image("ghcr.io/sannrox/kako:latest=sha256:" + "a" * 64)
        with self.assertRaisesRegex(SystemExit, "NAME=sha256"):
            manifest.parse_image("ghcr.io/sannrox/kako:0.1.0=sha256:abc")

    def test_refuses_a_bad_sha_a_mismatched_tag_or_an_empty_dist(self):
        with self.assertRaisesRegex(SystemExit, "no release files"):
            manifest.build_manifest(**self.args)
        write_dist(self.dist)
        with self.assertRaisesRegex(SystemExit, "full lowercase SHA"):
            manifest.build_manifest(**{**self.args, "git_sha": "deadbeef"})
        with self.assertRaisesRegex(SystemExit, "does not match"):
            manifest.build_manifest(**{**self.args, "tag": "v0.2.0"})

    def test_lists_only_shipped_files_by_digest(self):
        write_dist(self.dist)
        doc = manifest.build_manifest(**{**self.args, "images": [manifest.parse_image(IMAGE)]})
        self.assertEqual(doc["schema"], manifest.SCHEMA)
        self.assertEqual(
            [b["name"] for b in doc["binaries"]],
            ["kako-0.1.0-x86_64-unknown-linux-gnu.tar.gz", "kako-sql-image.tar", "kako.cdx.json"],
        )
        self.assertTrue(all(manifest.DIGEST_RE.fullmatch(b["digest"]) for b in doc["binaries"]))
        self.assertNotIn("latest", json.dumps(doc))


class LabelsFromHistory(unittest.TestCase):
    """manifest.py --labels-from-history in a scratch repository with a fake gh."""

    def setUp(self):
        tmp = tempfile.TemporaryDirectory(prefix=f"mh{os.getpid()}-")
        self.addCleanup(tmp.cleanup)
        self.tmp = pathlib.Path(tmp.name)
        self.work = self.tmp / "w"
        bin_dir = self.tmp / "bin"
        bin_dir.mkdir()
        (bin_dir / "gh").write_text(FAKE_GH)
        (bin_dir / "gh").chmod(0o755)
        self.env = {
            **os.environ,
            "PATH": f"{bin_dir}:{os.environ['PATH']}",
            "GIT_CONFIG_GLOBAL": os.devnull,
            "GIT_CONFIG_NOSYSTEM": "1",
            "FAKE_LABELS": "1=risk:breaking 2=gate:mig,docs 3=risk:realm",
            "FAKE_FAIL": "",
        }
        subprocess.run(["git", "init", "-q", str(self.work)], env=self.env, check=True)
        self.git("config", "user.name", "Release test")
        self.git("config", "user.email", "release-test@example.invalid")
        self.git("config", "commit.gpgsign", "false")
        self.dist = self.tmp / "dist"
        self.dist.mkdir()
        write_dist(self.dist)

    def git(self, *args: str) -> str:
        return subprocess.run(
            ["git", "-C", str(self.work), *args], env=self.env, check=True, capture_output=True, text=True
        ).stdout.strip()

    def commit(self, subject: str) -> str:
        self.git("commit", "-q", "--allow-empty", "-m", subject)
        return self.git("rev-parse", "HEAD")

    def run_manifest(self, sha: str) -> subprocess.CompletedProcess:
        out = self.tmp / "kako-release.json"
        return subprocess.run(
            ["python3", str(SCRIPT), "--version", "0.1.0", "--git-sha", sha, "--tag", "v0.1.0",
             "--dist", str(self.dist), "--out", str(out), "--image", IMAGE, "--labels-from-history"],
            cwd=self.work, env=self.env, capture_output=True, text=True,
        )

    def gate_flags(self, sha: str) -> list[str]:
        result = self.run_manifest(sha)
        self.assertEqual(result.returncode, 0, result.stderr)
        return json.loads((self.tmp / "kako-release.json").read_text())["gate_flags"]

    def test_first_release_skips_the_bootstrap_and_reads_every_pull_request(self):
        self.commit("initial import")
        self.commit("feat: one (#1)")
        sha = self.commit("feat: two (#2)")
        self.assertEqual(self.gate_flags(sha), ["breaking", "migration"])

    def test_a_final_release_starts_at_the_previous_final_tag_and_keeps_its_candidates(self):
        self.commit("feat: one (#1)")
        self.git("tag", "v0.0.1")
        self.commit("feat: two (#2)")
        self.git("tag", "v0.1.0-rc.1")
        sha = self.commit("fix: three (#3)")
        self.assertEqual(self.gate_flags(sha), ["migration", "realm"])

    def test_a_commit_without_a_pull_request_fails(self):
        self.commit("initial import")
        self.commit("feat: one (#1)")
        sha = self.commit("pushed straight to main")
        result = self.run_manifest(sha)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("pushed straight to main", result.stderr)

    def test_a_label_lookup_error_fails_instead_of_dropping_a_gate(self):
        self.commit("initial import")
        self.commit("feat: one (#1)")
        sha = self.commit("feat: two (#2)")
        self.env["FAKE_FAIL"] = "2"
        result = self.run_manifest(sha)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("cannot read labels of pull request #2", result.stderr)
        self.assertFalse((self.tmp / "kako-release.json").exists())


if __name__ == "__main__":
    unittest.main()
