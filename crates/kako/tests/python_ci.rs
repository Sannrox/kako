//! CI must fail closed when Python tests would skip, and must build the image.

use std::fs;
use std::path::PathBuf;

use kako::digest;

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

#[test]
fn ci_requires_python_and_builds_the_runtime_image() {
    let root = workspace_root();
    let ci = fs::read_to_string(root.join(".github/workflows/ci.yml")).unwrap();
    assert!(
        ci.contains("KAKO_REQUIRE_PYTHON:") && ci.contains("\"1\""),
        "CI must set KAKO_REQUIRE_PYTHON=1:\n{ci}"
    );
    assert!(
        ci.contains("scripts/python-runtime-ci.sh"),
        "CI must build the Python image:\n{ci}"
    );
    assert!(
        ci.contains("scripts/ci-cgroup-delegate.sh"),
        "CI must delegate a cgroup subtree for memory jobs:\n{ci}"
    );

    let cgroup = fs::read_to_string(root.join("scripts/ci-cgroup-delegate.sh")).unwrap();
    assert!(
        cgroup.contains("systemd-run")
            && cgroup.contains("Delegate=yes")
            && cgroup.contains("bash \"$self\"")
            && cgroup.contains("cgroup.subtree_control")
            && cgroup.contains("memory.swap.max")
            && cgroup.contains("KAKO_CGROUP_ROOT"),
        "cgroup delegate script must use a delegated systemd scope:\n{cgroup}"
    );
    assert!(
        !cgroup.contains("chmod a+w"),
        "must not world-write cgroup.procs:\n{cgroup}"
    );

    let script = fs::read_to_string(root.join("scripts/python-runtime-ci.sh")).unwrap();
    assert!(
        script.contains("docker build --no-cache"),
        "image job must build twice without cache:\n{script}"
    );
    assert!(
        script.contains("join_polars.py") && script.contains("join_pandas.py"),
        "image job must run both joins inside the image:\n{script}"
    );

    let lock = root.join("runtime/python/requirements.txt");
    let hashed = digest::hash_file(&lock).unwrap();
    let hex = hashed.0.strip_prefix("sha256:").unwrap_or(&hashed.0);
    let expected = format!("python-lockfile:sha256:{hex}");
    assert!(
        hashed.0.starts_with("sha256:"),
        "lockfile digest {hashed:?}"
    );
    assert!(
        script.contains("python-lockfile:sha256:"),
        "script must compare the lockfile image identity {expected}:\n{script}"
    );
}

#[test]
fn python_tests_required_follows_the_env() {
    let required = kako::python_tests_required();
    let from_env = std::env::var("KAKO_REQUIRE_PYTHON").ok().as_deref() == Some("1");
    assert_eq!(required, from_env);
}
