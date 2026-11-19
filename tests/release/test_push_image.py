"""push-image.sh against a fake docker that records every call."""

import os
import pathlib
import subprocess
import tempfile
import unittest

from tests.release import RELEASE

SCRIPT = RELEASE / "push-image.sh"
DIGEST = "sha256:" + "c" * 64

FAKE_DOCKER = r"""#!/usr/bin/env bash
printf '%s\n' "$*" >>"$FAKE_LOG"
if [[ "$1" == load ]]; then
  printf '%s\n' "${3:-}" >"$FAKE_STATE"
  echo "Loaded image: kako-sql:0.1.0-rc.1"
  exit 0
fi
if [[ "$1" == inspect && "$*" == *Architecture* ]]; then
  tar="$(cat "$FAKE_STATE" 2>/dev/null || true)"
  if [[ "${FAKE_ARCH:-}" == same ]]; then
    echo amd64
  elif [[ "$tar" == *arm64* ]]; then
    echo arm64
  else
    echo amd64
  fi
  exit 0
fi
if [[ "$1" == buildx && "$2" == imagetools && "$3" == inspect ]]; then
  if [[ "$*" == *Manifest.Digest* ]]; then
    echo "${FAKE_DIGEST:-}"
    exit 0
  fi
  echo "${FAKE_INSPECT:-}"
  exit 0
fi
echo "docker $1 output"
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
        self.amd64 = pathlib.Path(tmp.name) / "kako-sql-image-amd64.tar"
        self.arm64 = pathlib.Path(tmp.name) / "kako-sql-image-arm64.tar"
        self.amd64.write_bytes(b"amd64")
        self.arm64.write_bytes(b"arm64")
        self.env = {
            **os.environ,
            "PATH": f"{bin_dir}:{os.environ['PATH']}",
            "FAKE_LOG": str(self.log),
            "FAKE_STATE": str(pathlib.Path(tmp.name) / "last-tar"),
            "FAKE_DIGEST": DIGEST,
            "FAKE_INSPECT": "Platform: linux/amd64\nPlatform: linux/arm64\n",
        }

    def push(self, tag: str, *tars: str) -> subprocess.CompletedProcess:
        return subprocess.run(
            ["bash", str(SCRIPT), tag, *tars],
            env=self.env,
            capture_output=True,
            text=True,
        )

    def test_pushes_a_multi_arch_index_and_prints_only_the_output_line(self):
        result = self.push("v0.1.0-rc.1", str(self.amd64), str(self.arm64))
        self.assertEqual(result.returncode, 0, result.stderr)
        image = "ghcr.io/sannrox/kako:0.1.0-rc.1"
        self.assertEqual(result.stdout, f"image={image}={DIGEST}\n")
        self.assertEqual(
            self.log.read_text().splitlines(),
            [
                f"load -i {self.amd64}",
                f"inspect --format {{{{.Architecture}}}} kako-sql:0.1.0-rc.1",
                f"tag kako-sql:0.1.0-rc.1 {image}-amd64",
                f"push {image}-amd64",
                f"load -i {self.arm64}",
                f"inspect --format {{{{.Architecture}}}} kako-sql:0.1.0-rc.1",
                f"tag kako-sql:0.1.0-rc.1 {image}-arm64",
                f"push {image}-arm64",
                f"buildx imagetools create -t {image} {image}-amd64 {image}-arm64",
                f"buildx imagetools inspect {image}",
                f"buildx imagetools inspect {image} --format {{{{printf \"%s\" .Manifest.Digest}}}}",
            ],
        )

    def test_refuses_anything_but_a_release_tag_before_calling_docker(self):
        for tag in ("latest", "vlatest", "v0.1", "0.1.0"):
            with self.subTest(tag=tag):
                self.assertNotEqual(self.push(tag, str(self.amd64), str(self.arm64)).returncode, 0)
        self.assertEqual(self.log.read_text(), "")

    def test_refuses_a_single_tar_before_calling_docker(self):
        result = self.push("v0.1.0-rc.1", str(self.amd64))
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(self.log.read_text(), "")

    def test_refuses_two_images_of_the_same_architecture(self):
        self.env["FAKE_ARCH"] = "same"
        result = self.push("v0.1.0-rc.1", str(self.amd64), str(self.arm64))
        self.assertNotEqual(result.returncode, 0, result.stderr)
        self.assertIn("duplicate amd64", result.stderr)
        self.assertFalse(any("imagetools create" in line for line in self.log.read_text().splitlines()))

    def test_refuses_an_index_missing_a_platform(self):
        self.env["FAKE_INSPECT"] = "Platform: linux/amd64\n"
        result = self.push("v0.1.0-rc.1", str(self.amd64), str(self.arm64))
        self.assertNotEqual(result.returncode, 0, result.stderr)
        self.assertIn("missing linux/amd64 or linux/arm64", result.stderr)


if __name__ == "__main__":
    unittest.main()
