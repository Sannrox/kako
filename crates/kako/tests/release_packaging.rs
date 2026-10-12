//! Release packaging: the dry-run script writes checksums of the real files.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use kako::digest;
use tempfile::TempDir;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn sha256sums_hex(sums: &str, name: &str) -> String {
    for line in sums.lines() {
        let (hex, file) = line
            .split_once(char::is_whitespace)
            .unwrap_or_else(|| panic!("malformed SHA256SUMS line: {line}"));
        let file = file.trim().trim_start_matches('*');
        if file == name {
            return hex.to_string();
        }
    }
    panic!("SHA256SUMS missing {name}: {sums}");
}

fn file_hex(path: &Path) -> String {
    let digest = digest::hash_file(path).unwrap();
    digest
        .0
        .strip_prefix("sha256:")
        .unwrap_or(&digest.0)
        .to_string()
}

#[test]
fn dry_run_checksums_match_packaged_files() {
    let root = workspace_root();
    let out = TempDir::new().unwrap();
    let payload_dir = TempDir::new().unwrap();
    let payload = payload_dir.path().join("kako");
    fs::write(&payload, b"kako-release-fixture\n").unwrap();

    let status = Command::new("bash")
        .arg(root.join("scripts/release-dry-run.sh"))
        .arg("--out")
        .arg(out.path())
        .arg("--skip-build")
        .arg("--binary")
        .arg(&payload)
        .arg("--python-dir")
        .arg(root.join("runtime/python"))
        .status()
        .unwrap();
    assert!(status.success(), "release-dry-run.sh failed: {status}");

    let sums = fs::read_to_string(out.path().join("SHA256SUMS")).unwrap();
    let mut hashed = 0;
    for entry in fs::read_dir(out.path()).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy();
        if name == "SHA256SUMS" {
            continue;
        }
        let listed = sha256sums_hex(&sums, &name);
        let actual = file_hex(&path);
        assert_eq!(listed, actual, "checksum mismatch for {name}");
        hashed += 1;
    }
    assert!(
        hashed >= 2,
        "expected binary and python tarballs, sums:\n{sums}"
    );
    assert!(
        sums.contains("kako-python-catalog.tar.gz"),
        "python catalog archive missing:\n{sums}"
    );
}

fn run_dry_run(out: &Path, binary: &Path) -> std::process::Output {
    let root = workspace_root();
    Command::new("bash")
        .arg(root.join("scripts/release-dry-run.sh"))
        .arg("--out")
        .arg(out)
        .arg("--skip-build")
        .arg("--binary")
        .arg(binary)
        .arg("--python-dir")
        .arg(root.join("runtime/python"))
        .output()
        .unwrap()
}

#[test]
fn python_catalog_archive_matches_lockfile_digest_layout() {
    let root = workspace_root();
    let out = TempDir::new().unwrap();
    let payload_dir = TempDir::new().unwrap();
    let payload = payload_dir.path().join("kako");
    fs::write(&payload, b"kako-release-fixture\n").unwrap();
    let status = run_dry_run(out.path(), &payload).status;
    assert!(status.success(), "release-dry-run.sh failed: {status}");

    let lock = fs::read(root.join("runtime/python/requirements.txt")).unwrap();
    let hex = digest::hash_bytes(&lock)
        .0
        .strip_prefix("sha256:")
        .unwrap()
        .to_string();
    let extract = TempDir::new().unwrap();
    let tar_status = Command::new("tar")
        .arg("-xzf")
        .arg(out.path().join("kako-python-catalog.tar.gz"))
        .arg("-C")
        .arg(extract.path())
        .status()
        .unwrap();
    assert!(tar_status.success(), "tar extract failed: {tar_status}");
    let packed_lock = extract.path().join(&hex).join("requirements.txt");
    assert_eq!(fs::read(&packed_lock).unwrap(), lock);
    let current = fs::read_to_string(extract.path().join("current")).unwrap();
    assert_eq!(current.trim(), format!("python-lockfile:sha256:{hex}"));
}

#[test]
fn tag_version_must_match_cargo_toml() {
    let root = workspace_root();
    let out = TempDir::new().unwrap();
    let payload_dir = TempDir::new().unwrap();
    let payload = payload_dir.path().join("kako");
    fs::write(&payload, b"kako-release-fixture\n").unwrap();
    let output = Command::new("bash")
        .arg(root.join("scripts/release-dry-run.sh"))
        .arg("--out")
        .arg(out.path())
        .arg("--skip-build")
        .arg("--binary")
        .arg(&payload)
        .arg("--python-dir")
        .arg(root.join("runtime/python"))
        .arg("--version")
        .arg("0.0.0-not-the-workspace")
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "mismatched --version must fail closed"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("does not match Cargo.toml"),
        "stderr: {stderr}"
    );
}

#[test]
fn dry_run_checksums_detect_a_tampered_tarball() {
    let root = workspace_root();
    let out = TempDir::new().unwrap();
    let payload_dir = TempDir::new().unwrap();
    let payload = payload_dir.path().join("kako");
    fs::write(&payload, b"kako-release-fixture\n").unwrap();

    let status = Command::new("bash")
        .arg(root.join("scripts/release-dry-run.sh"))
        .arg("--out")
        .arg(out.path())
        .arg("--skip-build")
        .arg("--binary")
        .arg(&payload)
        .arg("--python-dir")
        .arg(root.join("runtime/python"))
        .status()
        .unwrap();
    assert!(status.success(), "release-dry-run.sh failed: {status}");

    let py = out.path().join("kako-python-catalog.tar.gz");
    let mut bytes = fs::read(&py).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    fs::write(&py, &bytes).unwrap();

    let sums = fs::read_to_string(out.path().join("SHA256SUMS")).unwrap();
    let listed = sha256sums_hex(&sums, "kako-python-catalog.tar.gz");
    let actual = file_hex(&py);
    assert_ne!(
        listed, actual,
        "tampering the python tarball must change its digest"
    );
}

#[test]
fn release_workflow_builds_signed_tag_artifacts() {
    let yaml = fs::read_to_string(workspace_root().join(".github/workflows/release.yml")).unwrap();
    assert!(yaml.contains("scripts/release-dry-run.sh"), "{yaml}");
    assert!(yaml.contains("--with-images"), "{yaml}");
    assert!(
        yaml.contains("actions/attest-build-provenance@v3"),
        "{yaml}"
    );
    assert!(yaml.contains("gh release create"), "{yaml}");
    assert!(yaml.contains("tags:"), "{yaml}");
    assert!(yaml.contains("v*"), "{yaml}");
    assert!(yaml.contains("workflow_dispatch"), "{yaml}");
    assert!(yaml.contains("--version"), "{yaml}");
    assert!(yaml.contains("--with-site-packages"), "{yaml}");
    assert!(
        yaml.contains("github.event_name == 'push' && startsWith(github.ref, 'refs/tags/v')"),
        "attest and publish must be tag-only: {yaml}"
    );
    let attest_idx = yaml
        .find("actions/attest-build-provenance@v3")
        .expect("missing attest action");
    let tag_gate_before_attest = yaml[..attest_idx]
        .rfind("startsWith(github.ref, 'refs/tags/v')")
        .expect("attest job must sit behind the tag gate");
    assert!(
        tag_gate_before_attest < attest_idx,
        "attestation must not run on pull_request"
    );
}

#[test]
fn release_script_installs_hashed_site_packages_when_asked() {
    let script = fs::read_to_string(workspace_root().join("scripts/release-dry-run.sh")).unwrap();
    assert!(script.contains("--with-site-packages"), "{script}");
    assert!(script.contains("pip install --require-hashes"), "{script}");
    assert!(script.contains("site-packages"), "{script}");
}

#[test]
fn sql_image_copies_the_runner_binary() {
    let docker = fs::read_to_string(workspace_root().join("runtime/sql/Dockerfile")).unwrap();
    assert!(docker.contains("COPY kako"), "{docker}");
    assert!(docker.contains("ENTRYPOINT"), "{docker}");
}

#[test]
fn crates_stay_unpublished_while_git_pinned() {
    let cargo = fs::read_to_string(workspace_root().join("Cargo.toml")).unwrap();
    assert!(
        cargo.contains("publish = false"),
        "workspace must keep publish = false: {cargo}"
    );
}
