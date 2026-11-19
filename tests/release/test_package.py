"""package.sh SQL image path against a fake docker that records every call."""

import os
import pathlib
import subprocess
import tempfile
import unittest

from tests.release import RELEASE, ROOT

SCRIPT = RELEASE / "package.sh"

FAKE_DOCKER = r"""#!/usr/bin/env bash
printf '%s\n' "$*" >>"$FAKE_LOG"
if [[ "$1" == run && "${FAKE_RUN_FAIL:-}" == 1 ]]; then
  echo "fake docker: image failed to start" >&2
  exit 1
fi
if [[ "$1" == save ]]; then
  out=""
  while [[ $# -gt 0 ]]; do
    if [[ "$1" == -o ]]; then
      out="${2:?}"
      break
    fi
    shift
  done
  [[ -n "$out" ]] && : >"$out"
fi
exit 0
"""


class PackageSqlImage(unittest.TestCase):
    def setUp(self):
        tmp = tempfile.TemporaryDirectory(prefix=f"pkg{os.getpid()}-")
        self.addCleanup(tmp.cleanup)
        root = pathlib.Path(tmp.name)
        bin_dir = root / "bin"
        bin_dir.mkdir()
        (bin_dir / "docker").write_text(FAKE_DOCKER)
        (bin_dir / "docker").chmod(0o755)
        self.log = root / "docker.log"
        self.log.touch()
        self.out = root / "dist"
        self.out.mkdir()
        self.binary = root / "kako"
        self.binary.write_bytes(b"kako-release-fixture\n")
        self.binary.chmod(0o755)
        self.env = {
            **os.environ,
            "PATH": f"{bin_dir}:{os.environ['PATH']}",
            "FAKE_LOG": str(self.log),
        }

    def package(self) -> subprocess.CompletedProcess:
        return subprocess.run(
            [
                "bash",
                str(SCRIPT),
                "--out",
                str(self.out),
                "--skip-build",
                "--binary",
                str(self.binary),
                "--python-dir",
                str(ROOT / "runtime" / "python"),
                "--with-sql-image",
            ],
            env=self.env,
            capture_output=True,
            text=True,
        )

    def test_starts_the_sql_image_before_saving_it(self):
        result = self.package()
        self.assertEqual(result.returncode, 0, result.stderr)
        lines = self.log.read_text().splitlines()
        run = next((i for i, line in enumerate(lines) if line.startswith("run --rm kako-sql:")), None)
        save = next((i for i, line in enumerate(lines) if line.startswith("save kako-sql:")), None)
        self.assertIsNotNone(run, lines)
        self.assertIsNotNone(save, lines)
        self.assertLess(run, save)
        self.assertTrue(lines[run].endswith(" --version"), lines[run])
        self.assertTrue((self.out / "kako-sql-image.tar").is_file())

    def test_a_failing_start_refuses_to_save_the_image(self):
        self.env["FAKE_RUN_FAIL"] = "1"
        result = self.package()
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn("failed to start", result.stderr)
        lines = self.log.read_text().splitlines()
        self.assertTrue(any(line.startswith("run --rm kako-sql:") for line in lines), lines)
        self.assertFalse(any(line.startswith("save ") for line in lines), lines)
        self.assertFalse((self.out / "kako-sql-image.tar").exists())


if __name__ == "__main__":
    unittest.main()
