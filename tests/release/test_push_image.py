"""push-image.sh against a fake docker that records every call."""

import os
import pathlib
import subprocess
import tempfile
import unittest

from tests.release import RELEASE

SCRIPT = RELEASE / "push-image.sh"
DIGEST = "sha256:" + "c" * 64

FAKE_DOCKER = f"""#!/usr/bin/env bash
printf '%s\\n' "$*" >>"$FAKE_LOG"
echo "docker $1 output"
[[ "$1" == inspect ]] && echo "ghcr.io/sannrox/kako@{DIGEST}"
exit 0
"""


class PushImage(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory(prefix=f"pi{os.getpid()}-")
        self.addCleanup(tmp.cleanup)
        bin_dir = pathlib.Path(tmp.name) / "bin"
        bin_dir.mkdir()
        (bin_dir / "docker").write_text(FAKE_DOCKER)
        (bin_dir / "docker").chmod(0o755)
        self.log = pathlib.Path(tmp.name) / "docker.log"
        self.log.touch()
        self.env = {**os.environ, "PATH": f"{bin_dir}:{os.environ['PATH']}", "FAKE_LOG": str(self.log)}

    def push(self, tag: str) -> subprocess.CompletedProcess:
        return subprocess.run(["bash", str(SCRIPT), tag, "dist/kako-sql-image.tar"],
                              env=self.env, capture_output=True, text=True)

    def test_pushes_the_version_tag_and_prints_only_the_output_line(self):
        result = self.push("v0.1.0-rc.1")
        self.assertEqual(result.returncode, 0, result.stderr)
        image = "ghcr.io/sannrox/kako:0.1.0-rc.1"
        self.assertEqual(result.stdout, f"image={image}={DIGEST}\n")
        self.assertEqual(self.log.read_text().splitlines()[:3], [
            "load -i dist/kako-sql-image.tar",
            f"tag kako-sql:0.1.0-rc.1 {image}",
            f"push {image}",
        ])

    def test_refuses_anything_but_a_release_tag_before_calling_docker(self):
        for tag in ("latest", "vlatest", "v0.1", "0.1.0"):
            with self.subTest(tag=tag):
                self.assertNotEqual(self.push(tag).returncode, 0)
        self.assertEqual(self.log.read_text(), "")


if __name__ == "__main__":
    unittest.main()
