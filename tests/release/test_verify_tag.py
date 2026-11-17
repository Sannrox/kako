"""verify-tag.sh against real annotated, signed, and lightweight tags in a scratch repository.

The signer allowlist and key are read from the trusted ref (RELEASE_TRUST_REF,
`main` here), never from the tagged commit.
"""

import os
import pathlib
import shutil
import subprocess
import tempfile
import unittest

from tests.release import RELEASE

SCRIPT = RELEASE / "verify-tag.sh"
UNKNOWN_FPR = "AEDA618886BF0331105B9CFCE4DC1F4A579505FB"


def cargo_toml(version: str) -> str:
    # The [workspace.dependencies] version must not be read as the release version.
    return (
        f'[workspace]\nmembers = []\n\n[workspace.package]\nversion = "{version}"\n\n'
        '[workspace.dependencies]\nversion = "9.9.9"\n'
    )


class Fixture:
    """A keyring with one signing key and a repository whose main trusts it."""

    def __init__(self, prefix: str):
        # Short base path: gpg-agent socket paths have a length limit.
        self.tmp = pathlib.Path(tempfile.mkdtemp(prefix=f"{prefix}{os.getpid()}-", dir="/tmp"))
        self.gnupg = self.tmp / "g"
        self.gnupg.mkdir(mode=0o700)
        self.work = self.tmp / "w"
        self.env = {
            **os.environ,
            "GNUPGHOME": str(self.gnupg),
            "GIT_CONFIG_GLOBAL": os.devnull,
            "GIT_CONFIG_NOSYSTEM": "1",
            "RELEASE_TRUST_REF": "main",
        }
        self.gpg("--pinentry-mode", "loopback", "--passphrase", "",
                 "--quick-generate-key", "Release test <release-test@example.invalid>", "ed25519", "default", "never")
        listing = self.gpg("--list-secret-keys", "--with-colons")
        self.fpr = next(line.split(":")[9] for line in listing.splitlines() if line.startswith("fpr:"))
        subprocess.run(["git", "init", "-q", "-b", "main", str(self.work)], env=self.env, check=True)
        for key, value in {
            "user.name": "Release test", "user.email": "release-test@example.invalid",
            "commit.gpgsign": "false", "tag.gpgSign": "false",
            "user.signingkey": self.fpr, "gpg.program": shutil.which("gpg"),
        }.items():
            self.git("config", key, value)
        (self.work / "security").mkdir()
        self.export_key()
        self.allow(self.fpr)
        (self.work / "Cargo.toml").write_text(cargo_toml("0.1.0"))
        self.git("add", "Cargo.toml", "security")
        self.git("commit", "-q", "-m", "first")
        self.first = self.git("rev-parse", "HEAD")

    def gpg(self, *args: str) -> str:
        return subprocess.run(["gpg", "--batch", *args], env=self.env, check=True, capture_output=True, text=True).stdout

    def git(self, *args: str) -> str:
        return subprocess.run(
            ["git", "-C", str(self.work), *args], env=self.env, check=True, capture_output=True, text=True
        ).stdout.strip()

    def export_key(self) -> None:
        (self.work / "security" / "release-tag-key.asc").write_text(self.gpg("--armor", "--export", self.fpr))

    def allow(self, fpr: str) -> None:
        (self.work / "security" / "release-tag-fingerprints").write_text(f"# test\n{fpr}\n")

    def verify(self, *args: str) -> subprocess.CompletedProcess:
        return subprocess.run(["bash", str(SCRIPT), "verify", *args], cwd=self.work, env=self.env,
                              capture_output=True, text=True)

    def close(self) -> None:
        subprocess.run(["gpgconf", "--kill", "gpg-agent"], env=self.env, capture_output=True)
        shutil.rmtree(self.tmp, ignore_errors=True)


@unittest.skipUnless(shutil.which("gpg"), "gpg is required")
class VerifyTag(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.f = f = Fixture("vt")
        f.git("tag", "v0.0.1", f.first)
        f.git("tag", "-a", "v0.0.2", "-m", "unsigned", f.first)
        f.git("tag", "-s", "v0.1.0", "-m", "signed", f.first)
        f.git("tag", "-s", "v0.2.0", "-m", "wrong version", f.first)
        f.git("checkout", "-q", "-b", "side")
        f.git("commit", "-q", "--allow-empty", "-m", "side")
        f.git("tag", "-s", "v0.1.0-rc.1", "-m", "side")
        f.git("checkout", "-q", "main")
        (f.work / "Cargo.toml").write_text(cargo_toml("0.3.0-rc.1"))
        f.git("commit", "-q", "-am", "rc")
        cls.rc = f.git("rev-parse", "HEAD")
        f.git("tag", "-s", "v0.3.0-rc.1", "-m", "rc", cls.rc)
        f.git("tag", "-s", "v0.3.0", "-m", "final on rc", cls.rc)

    @classmethod
    def tearDownClass(cls):
        cls.f.close()

    def assertRefused(self, message: str, *args: str):
        result = self.f.verify(*args)
        self.assertNotEqual(result.returncode, 0, result.stdout)
        self.assertIn(message, result.stderr)

    def test_accepts_a_signed_tag_matching_the_workspace_version(self):
        for tag, sha in (("v0.1.0", self.f.first), ("v0.3.0-rc.1", self.rc)):
            with self.subTest(tag=tag):
                result = self.f.verify(tag, sha)
                self.assertEqual(result.returncode, 0, result.stderr)

    def test_refuses_lightweight_and_unsigned_tags(self):
        self.assertRefused("is lightweight", "v0.0.1", self.f.first)
        self.assertRefused("unsigned", "v0.0.2", self.f.first)

    def test_refuses_a_tag_for_another_commit(self):
        self.assertRefused(f"resolves to {self.f.first}, expected {self.rc}", "v0.1.0", self.rc)

    def test_refuses_a_tag_off_the_trusted_branch(self):
        self.assertRefused("is not on main", "v0.1.0-rc.1")

    def test_refuses_a_version_mismatch_including_a_final_tag_on_an_rc_version(self):
        self.assertRefused("does not match workspace version 0.1.0", "v0.2.0", self.f.first)
        self.assertRefused("does not match workspace version 0.3.0-rc.1", "v0.3.0", self.rc)

    def test_refuses_malformed_tags(self):
        for tag in ("v0.1", "v0.3.0-rc"):
            with self.subTest(tag=tag):
                self.assertRefused("invalid release tag", tag, self.f.first)

    def test_the_allowlist_on_the_trusted_ref_decides(self):
        f = self.f
        head = f.git("rev-parse", "HEAD")
        self.addCleanup(f.git, "reset", "-q", "--hard", head)
        f.allow(UNKNOWN_FPR)
        f.git("commit", "-q", "-am", "drop the test key")
        self.assertRefused("unknown key", "v0.1.0", f.first)


@unittest.skipUnless(shutil.which("gpg"), "gpg is required")
class RevokedKey(unittest.TestCase):
    def setUp(self):
        self.f = Fixture("vr")
        self.addCleanup(self.f.close)

    def test_revoking_the_key_on_the_trusted_ref_refuses_tags_it_signed(self):
        f = self.f
        f.git("tag", "-s", "v0.1.0", "-m", "signed", f.first)
        self.assertEqual(f.verify("v0.1.0").returncode, 0)
        # gpg prefixes the stored revocation with ':' against accidental use.
        revocation = (f.gnupg / "openpgp-revocs.d" / f"{f.fpr}.rev").read_text()
        subprocess.run(["gpg", "--batch", "--import"], input=revocation.replace(":-----BEGIN", "-----BEGIN"),
                       env=f.env, check=True, capture_output=True, text=True)
        f.export_key()
        f.git("commit", "-q", "-am", "revoke the test key")
        result = f.verify("v0.1.0")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("expired or revoked", result.stderr)


if __name__ == "__main__":
    unittest.main()
