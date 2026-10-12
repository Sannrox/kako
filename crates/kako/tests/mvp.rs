//! End-to-end MVP: local job file, sandboxed DuckDB, receipt, cleanup.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use kako::Failure;
use kako::local::LocalSource;
use kako::python_tests_required;
use kako::run::{JobFinish, RunnerConfig, run_source};
use kako::sandbox;

fn worker() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_kako"))
}

fn linux_native_ready() -> bool {
    match sandbox::select_backend() {
        Ok(backend) if backend.to_string() == "linux_native" => true,
        other => {
            eprintln!("skip: sandboxed run needs linux_native, got {other:?}");
            false
        }
    }
}

fn userspace_ready() -> bool {
    matches!(
        sandbox::select_backend_for(kako::IsolationTier::Userspace),
        Ok(tier) if tier.to_string() == "userspace"
    )
}

fn join_job(dir: &Path) -> std::path::PathBuf {
    fs::write(dir.join("customers.csv"), "id,name\n1,a\n2,b\n").unwrap();
    fs::write(
        dir.join("orders.csv"),
        "customer_id,amount\n1,10\n1,15\n2,7\n",
    )
    .unwrap();
    let job = dir.join("job.json");
    fs::write(
        &job,
        r#"{
          "id": "join-agg",
          "sql": "COPY (\n  SELECT c.id AS customer_id, c.name, sum(o.amount) AS total\n  FROM customers c\n  JOIN orders o ON o.customer_id = c.id\n  GROUP BY c.id, c.name\n  ORDER BY c.id\n) TO 'outputs/totals.csv' (HEADER, DELIMITER ',');",
          "inputs": {"customers": "customers.csv", "orders": "orders.csv"},
          "outputs": ["totals.csv"],
          "timeout_ms": 30000
        }"#,
    )
    .unwrap();
    job
}

fn scratch_leftovers(parent: &Path) -> Vec<String> {
    fs::read_dir(parent)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("kako-run-") {
                Some(name.into_owned())
            } else {
                None
            }
        })
        .collect()
}

#[test]
fn cli_join_twice_same_output_digest() {
    if !linux_native_ready() {
        return;
    }
    let exe = worker();
    let dir = tempfile::tempdir().unwrap();
    let job = join_job(dir.path());
    let out1 = dir.path().join("out1");
    let out2 = dir.path().join("out2");
    for out in [&out1, &out2] {
        let status = Command::new(&exe)
            .arg("run")
            .arg(&job)
            .arg("--output")
            .arg(out)
            .status()
            .unwrap();
        assert!(status.success(), "kako run failed: {status}");
        let csv = fs::read_to_string(out.join("totals.csv")).unwrap();
        assert!(csv.contains("customer_id"), "{csv}");
        assert!(csv.contains("25") || csv.contains("25.0"), "{csv}");
        let receipt = fs::read_to_string(out.join("receipt.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&receipt).unwrap();
        assert_eq!(v["receipt_version"], 3);
        assert_eq!(v["isolation"], "linux_native");
        assert_eq!(v["job_id"], "join-agg");
        assert!(
            v["inputs"]["customers"]
                .as_str()
                .unwrap()
                .starts_with("sha256:")
        );
        assert!(v["code"].as_str().unwrap().starts_with("sha256:"));
        assert!(v["engine_version"].as_str().unwrap().starts_with("duckdb "));
        assert!(
            v["outputs"]["totals.csv"]
                .as_str()
                .unwrap()
                .starts_with("sha256:")
        );
        assert!(v["duration_ms"].as_u64().is_some());
    }
    let r1: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(out1.join("receipt.json")).unwrap()).unwrap();
    let r2: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(out2.join("receipt.json")).unwrap()).unwrap();
    assert_eq!(r1["outputs"]["totals.csv"], r2["outputs"]["totals.csv"]);
    assert_eq!(r1["inputs"], r2["inputs"]);
    assert_eq!(r1["code"], r2["code"]);
}

#[test]
fn cli_mizuchi_join() {
    if !linux_native_ready() {
        return;
    }
    let bar = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/engine-bar");
    let exe = worker();
    let dir = tempfile::tempdir().unwrap();
    fs::copy(bar.join("customers.csv"), dir.path().join("customers.csv")).unwrap();
    fs::copy(bar.join("orders.csv"), dir.path().join("orders.csv")).unwrap();
    let job: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(bar.join("join-mizuchi.json")).unwrap()).unwrap();
    let job_path = dir.path().join("job.json");
    fs::write(&job_path, job.to_string()).unwrap();
    let out = dir.path().join("out");
    let status = Command::new(&exe)
        .arg("run")
        .arg(&job_path)
        .arg("--output")
        .arg(&out)
        .status()
        .unwrap();
    assert!(
        status.success(),
        "kako run --engine mizuchi failed: {status}"
    );
    let csv = fs::read_to_string(out.join("totals.csv")).unwrap();
    assert!(csv.contains('a'), "{csv}");
    assert!(csv.contains("25") || csv.contains("25.0"), "{csv}");
    let receipt = fs::read_to_string(out.join("receipt.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&receipt).unwrap();
    assert!(
        v["engine_version"]
            .as_str()
            .unwrap()
            .starts_with("mizuchi "),
        "{receipt}"
    );
    assert!(v["lineage"]["plan"].as_str().is_some(), "{receipt}");
    assert!(v["lineage"]["reads"].as_array().is_some(), "{receipt}");
    assert_eq!(v["lineage"]["writes"][0]["mode"], "overwrite", "{receipt}");
}

#[test]
fn cli_run_fails_closed_when_job_requires_userspace() {
    if userspace_ready() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let job = dir.path().join("job.json");
    fs::write(
        &job,
        r#"{
          "id": "guest",
          "sql": "COPY (SELECT 1 AS n) TO 'outputs/totals.csv' (HEADER, DELIMITER ',');",
          "inputs": {},
          "outputs": ["totals.csv"],
          "isolation": "userspace",
          "timeout_ms": 15000
        }"#,
    )
    .unwrap();
    let out = dir.path().join("out");
    let output = Command::new(worker())
        .arg("run")
        .arg(&job)
        .arg("--output")
        .arg(&out)
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "userspace floor must fail closed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("userspace"), "{stderr}");
    assert!(
        !out.join("receipt.json").exists(),
        "fail-closed run must not write a receipt"
    );
    assert!(
        !out.join("totals.csv").exists(),
        "fail-closed run must not write outputs"
    );
}

#[test]
fn cli_run_fails_closed_without_linux_native() {
    if linux_native_ready() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let job = join_job(dir.path());
    let output = Command::new(worker())
        .arg("run")
        .arg(&job)
        .arg("--output")
        .arg(dir.path().join("out"))
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "run must fail closed without linux_native"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("linux_native"), "{stderr}");
}

#[test]
fn cli_doctor_reports_backend() {
    let output = Command::new(worker()).arg("doctor").output().unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("backend_selected:"), "{stdout}");
    assert!(
        stdout.contains("linux_native") || stdout.contains("rlimit"),
        "{stdout}"
    );
    assert!(stdout.contains("userspace_health:"), "{stdout}");
}

#[test]
fn timeout_kills_the_child_and_removes_scratch() {
    if !linux_native_ready() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("in.csv"), "x\n1\n").unwrap();
    let job_path = dir.path().join("slow.json");
    fs::write(
        &job_path,
        r#"{
          "id": "slow",
          "sql": "SELECT sum(i) FROM range(100000000000) t(i);",
          "inputs": {"in": "in.csv"},
          "outputs": ["totals.csv"],
          "timeout_ms": 1000
        }"#,
    )
    .unwrap();
    let scratch_parent = dir.path().join("scratch");
    fs::create_dir_all(&scratch_parent).unwrap();
    let mut source = LocalSource::from_job_file(&job_path, dir.path().join("out")).unwrap();
    let config = RunnerConfig {
        worker: worker(),
        scratch_parent: scratch_parent.clone(),
    };
    let finishes = run_source(&mut source, &config).unwrap();
    match &finishes[0] {
        JobFinish::Failed {
            failure: Failure::Limit(message),
            ..
        } => assert!(message.contains("timeout"), "{message}"),
        other => panic!("expected limit timeout, got {other:?}"),
    }
    assert!(scratch_leftovers(&scratch_parent).is_empty());
}

#[test]
fn job_error_removes_scratch() {
    if !linux_native_ready() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("in.csv"), "x\n1\n").unwrap();
    let job_path = dir.path().join("bad.json");
    fs::write(
        &job_path,
        r#"{
          "id": "bad",
          "sql": "SELECT this_function_does_not_exist();",
          "inputs": {"in": "in.csv"},
          "outputs": ["totals.csv"],
          "timeout_ms": 15000
        }"#,
    )
    .unwrap();
    let scratch_parent = dir.path().join("scratch");
    fs::create_dir_all(&scratch_parent).unwrap();
    let mut source = LocalSource::from_job_file(&job_path, dir.path().join("out")).unwrap();
    let config = RunnerConfig {
        worker: worker(),
        scratch_parent: scratch_parent.clone(),
    };
    let finishes = run_source(&mut source, &config).unwrap();
    match &finishes[0] {
        JobFinish::Failed {
            failure: Failure::Job(message),
            ..
        } => {
            assert!(
                message.contains("this_function_does_not_exist") || message.contains("sql:"),
                "{message}"
            );
        }
        other => panic!("expected job error, got {other:?}"),
    }
    assert!(scratch_leftovers(&scratch_parent).is_empty());
    let reported = fs::read_to_string(dir.path().join("out/failure.txt")).unwrap();
    assert!(reported.contains("job:"), "{reported}");
    assert!(
        reported.contains("this_function_does_not_exist") || reported.contains("sql:"),
        "{reported}"
    );
}

#[test]
fn network_from_the_child_is_denied_on_linux_native() {
    if !linux_native_ready() {
        return;
    }
    let backend = sandbox::select_backend().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let mut cmd = Command::new(worker());
    cmd.arg("--internal-probe").arg("network");
    let worker = worker();
    let (status, _, stderr, _) = sandbox::run_sandboxed(
        cmd,
        scratch.path(),
        backend,
        Duration::from_secs(10),
        None,
        None,
        std::slice::from_ref(&worker),
    )
    .unwrap();
    assert_eq!(
        status.code(),
        Some(2),
        "probe should be denied (exit 2), stderr={}",
        String::from_utf8_lossy(&stderr)
    );
}

#[test]
fn cli_copy_constant_row() {
    if !linux_native_ready() {
        return;
    }
    let exe = worker();
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("in.csv"), "x\n1\n").unwrap();
    let job = dir.path().join("job.json");
    fs::write(
        &job,
        r#"{
          "id": "probe",
          "sql": "COPY (SELECT 1 AS x) TO 'outputs/one.csv' (HEADER, DELIMITER ',');",
          "inputs": {"in": "in.csv"},
          "outputs": ["one.csv"],
          "timeout_ms": 15000
        }"#,
    )
    .unwrap();
    let out = dir.path().join("out");
    let status = Command::new(&exe)
        .arg("run")
        .arg(&job)
        .arg("--output")
        .arg(&out)
        .status()
        .unwrap();
    assert!(status.success(), "kako run failed: {status}");
    let csv = fs::read_to_string(out.join("one.csv")).unwrap();
    assert!(csv.contains('x'), "{csv}");
    assert!(csv.contains('1'), "{csv}");
    let receipt = fs::read_to_string(out.join("receipt.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&receipt).unwrap();
    assert_eq!(v["receipt_version"], 3);
    assert_eq!(v["isolation"], "linux_native");
    assert_eq!(v["tenant_id"], "local");
    assert_eq!(v["namespace"], "default");
    assert!(
        v["outputs"]["one.csv"]
            .as_str()
            .unwrap()
            .starts_with("sha256:")
    );
}

#[test]
fn cli_require_identity_refuses_missing_tenant() {
    let dir = tempfile::tempdir().unwrap();
    let job = join_job(dir.path());
    let exe = worker();
    for i in 1..=2 {
        let output = Command::new(&exe)
            .arg("run")
            .arg(&job)
            .arg("--output")
            .arg(dir.path().join(format!("out{i}")))
            .arg("--require-identity")
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "run {i} must refuse a job without tenant_id"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("tenant_id"), "run {i} stderr: {stderr}");
    }
}

#[test]
fn successful_run_leaves_no_scratch() {
    if !linux_native_ready() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let job_path = join_job(dir.path());
    let scratch_parent = dir.path().join("scratch");
    fs::create_dir_all(&scratch_parent).unwrap();
    let out = dir.path().join("out");
    let mut source = LocalSource::from_job_file(&job_path, out.clone()).unwrap();
    let config = RunnerConfig {
        worker: worker(),
        scratch_parent: scratch_parent.clone(),
    };
    let finishes = run_source(&mut source, &config).unwrap();
    assert!(
        matches!(finishes[0], JobFinish::Completed { .. }),
        "{finishes:?}"
    );
    assert!(scratch_leftovers(&scratch_parent).is_empty());
    assert!(out.join("totals.csv").is_file());
    assert!(out.join("receipt.json").is_file());
}

fn skip_python(reason: &str) {
    if python_tests_required() {
        panic!("python test required (KAKO_REQUIRE_PYTHON=1): {reason}");
    }
    eprintln!("skip: {reason}");
}

fn require_linux_native_for_python() -> bool {
    if linux_native_ready() {
        return true;
    }
    skip_python("sandboxed python run needs linux_native");
    false
}

fn python_runtime_available() -> bool {
    let venv = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../runtime/python/.venv/bin/python");
    if venv.is_file() {
        return true;
    }
    Command::new("python3")
        .args(["-c", "import polars, pandas"])
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn engine_bar_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/engine-bar")
}

fn python_join_job(dir: &Path, source: &str) -> std::path::PathBuf {
    let bar = engine_bar_dir();
    fs::copy(bar.join("customers.csv"), dir.join("customers.csv")).unwrap();
    fs::copy(bar.join("orders.csv"), dir.join("orders.csv")).unwrap();
    let code = fs::read_to_string(bar.join(source)).unwrap();
    let job = dir.join("job.json");
    let body = serde_json::json!({
        "id": "join-python",
        "engine": "python",
        "code": code,
        "inputs": {"customers": "customers.csv", "orders": "orders.csv"},
        "outputs": ["totals.csv"],
        "timeout_ms": 30000
    });
    fs::write(&job, body.to_string()).unwrap();
    job
}

fn assert_join_csv(csv: &str) {
    assert!(csv.contains('a'), "{csv}");
    assert!(csv.contains('b'), "{csv}");
    assert!(
        csv.contains("25") || csv.contains("25.0"),
        "expected aggregate 25, got {csv}"
    );
    assert!(csv.contains('7'), "expected aggregate 7, got {csv}");
}

#[test]
fn cli_python_polars_join() {
    if !require_linux_native_for_python() {
        return;
    }
    if !python_runtime_available() {
        skip_python("python runtime with polars is unavailable");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let job = python_join_job(dir.path(), "join_polars.py");
    let out = dir.path().join("out");
    let status = Command::new(worker())
        .arg("run")
        .arg(&job)
        .arg("--output")
        .arg(&out)
        .status()
        .unwrap();
    assert!(status.success(), "kako run python polars failed: {status}");
    let csv = fs::read_to_string(out.join("totals.csv")).unwrap();
    assert_join_csv(&csv);
    let receipt: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(out.join("receipt.json")).unwrap()).unwrap();
    assert!(
        receipt["engine_version"]
            .as_str()
            .unwrap()
            .starts_with("python "),
        "{receipt}"
    );
    assert!(
        receipt["runtime_image"]
            .as_str()
            .unwrap()
            .starts_with("python-lockfile:sha256:"),
        "{receipt}"
    );
}

#[test]
fn cli_python_pandas_join() {
    if !require_linux_native_for_python() {
        return;
    }
    if !python_runtime_available() {
        skip_python("python runtime with pandas is unavailable");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let job = python_join_job(dir.path(), "join_pandas.py");
    let out = dir.path().join("out");
    let status = Command::new(worker())
        .arg("run")
        .arg(&job)
        .arg("--output")
        .arg(&out)
        .status()
        .unwrap();
    assert!(status.success(), "kako run python pandas failed: {status}");
    let csv = fs::read_to_string(out.join("totals.csv")).unwrap();
    assert_join_csv(&csv);
}

#[test]
fn python_user_code_cannot_pip_install_or_open_sockets() {
    if !require_linux_native_for_python() {
        return;
    }
    if !python_runtime_available() {
        skip_python("python runtime unavailable");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("customers.csv"), "id,name\n1,a\n").unwrap();
    fs::write(dir.path().join("orders.csv"), "customer_id,amount\n1,1\n").unwrap();
    let probes = [
        (
            "pip",
            r#"
def transform(inputs):
    import subprocess, sys
    subprocess.check_call([sys.executable, "-m", "pip", "install", "does-not-exist-kako-probe"])
    return {}
"#,
        ),
        (
            "socket",
            r#"
def transform(inputs):
    import socket
    socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    return {}
"#,
        ),
    ];
    for (name, code) in probes {
        let job = dir.path().join(format!("{name}.json"));
        let body = serde_json::json!({
            "id": name,
            "engine": "python",
            "code": code,
            "inputs": {"customers": "customers.csv", "orders": "orders.csv"},
            "outputs": ["totals.csv"],
            "timeout_ms": 15000
        });
        fs::write(&job, body.to_string()).unwrap();
        let output = Command::new(worker())
            .arg("run")
            .arg(&job)
            .arg("--output")
            .arg(dir.path().join(format!("out-{name}")))
            .output()
            .unwrap();
        assert!(
            !output.status.success(),
            "{name} must fail closed, stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn python_oom_is_a_limit_and_leaves_the_runner_alive() {
    if !require_linux_native_for_python() {
        return;
    }
    if !python_runtime_available() {
        skip_python("python runtime unavailable");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    fs::copy(
        engine_bar_dir().join("customers.csv"),
        dir.path().join("customers.csv"),
    )
    .unwrap();
    fs::copy(
        engine_bar_dir().join("orders.csv"),
        dir.path().join("orders.csv"),
    )
    .unwrap();
    let job_path = dir.path().join("oom.json");
    let body = serde_json::json!({
        "id": "oom",
        "engine": "python",
        "code": "def transform(inputs):\n    blob = bytearray(512 * 1024 * 1024)\n    return {\"totals.csv\": inputs[\"customers\"]}\n",
        "inputs": {"customers": "customers.csv", "orders": "orders.csv"},
        "outputs": ["totals.csv"],
        "timeout_ms": 30000,
        "memory_bytes": 32 * 1024 * 1024
    });
    fs::write(&job_path, body.to_string()).unwrap();
    let scratch_parent = dir.path().join("scratch");
    fs::create_dir_all(&scratch_parent).unwrap();
    let mut source = LocalSource::from_job_file(&job_path, dir.path().join("out")).unwrap();
    let config = RunnerConfig {
        worker: worker(),
        scratch_parent: scratch_parent.clone(),
    };
    let finishes = match run_source(&mut source, &config) {
        Ok(finishes) => finishes,
        Err(error) => {
            skip_python(&format!("python oom run could not start: {error}"));
            return;
        }
    };
    match &finishes[0] {
        JobFinish::Failed {
            failure: Failure::Limit(message),
            ..
        } => assert!(
            message.contains("oom") || message.contains("memory") || message.contains("limit"),
            "{message}"
        ),
        JobFinish::Failed {
            failure: Failure::Runner(message),
            ..
        } if message.contains("memory limit") => {
            skip_python(&format!("cgroup memory limit cannot be set: {message}"));
            return;
        }
        other => panic!("expected limit OOM, got {other:?}"),
    }
    assert!(scratch_leftovers(&scratch_parent).is_empty());
    let follow = python_join_job(dir.path(), "join_polars.py");
    let out = dir.path().join("after");
    let status = Command::new(worker())
        .arg("run")
        .arg(&follow)
        .arg("--output")
        .arg(&out)
        .status()
        .unwrap();
    assert!(
        status.success(),
        "runner must still run after a Python OOM: {status}"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn python_polars_join_finishes_under_cgroup_memory() {
    if !require_linux_native_for_python() {
        return;
    }
    if !python_runtime_available() {
        skip_python("python runtime unavailable");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let job = python_join_job(dir.path(), "join_polars.py");
    let raw = fs::read_to_string(&job).unwrap();
    let mut body: serde_json::Value = serde_json::from_str(&raw).unwrap();
    body["memory_bytes"] = serde_json::json!(256 * 1024 * 1024);
    fs::write(&job, body.to_string()).unwrap();
    let out = dir.path().join("out");
    let output = Command::new(worker())
        .arg("run")
        .arg(&job)
        .arg("--output")
        .arg(&out)
        .output()
        .unwrap();
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("memory limit") {
            skip_python(&format!("cgroup memory limit cannot be set: {stderr}"));
            return;
        }
        panic!("polars join under cgroup memory failed: {stderr}");
    }
    let csv = fs::read_to_string(out.join("totals.csv")).unwrap();
    assert_join_csv(&csv);
}

#[cfg(target_os = "linux")]
#[test]
fn python_pandas_join_finishes_under_cgroup_memory() {
    if !require_linux_native_for_python() {
        return;
    }
    if !python_runtime_available() {
        skip_python("python runtime unavailable");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let job = python_join_job(dir.path(), "join_pandas.py");
    let raw = fs::read_to_string(&job).unwrap();
    let mut body: serde_json::Value = serde_json::from_str(&raw).unwrap();
    body["memory_bytes"] = serde_json::json!(256 * 1024 * 1024);
    fs::write(&job, body.to_string()).unwrap();
    let out = dir.path().join("out");
    let output = Command::new(worker())
        .arg("run")
        .arg(&job)
        .arg("--output")
        .arg(&out)
        .output()
        .unwrap();
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if stderr.contains("memory limit") {
            skip_python(&format!("cgroup memory limit cannot be set: {stderr}"));
            return;
        }
        panic!("pandas join under cgroup memory failed: {stderr}");
    }
    let csv = fs::read_to_string(out.join("totals.csv")).unwrap();
    assert_join_csv(&csv);
}

/// Requires Linux, `linux_native` health, and `kekkai-guest` on PATH or
/// `KEKKAI_GUEST_HELPER`. Offline `cargo test` skips this.
#[test]
#[ignore = "needs kekkai-guest helper; set KEKKAI_GUEST_HELPER"]
fn cli_run_userspace_writes_receipt_isolation() {
    assert!(
        userspace_ready(),
        "guest helper and linux_native health are required"
    );
    let dir = tempfile::tempdir().unwrap();
    let job = join_job(dir.path());
    let raw = fs::read_to_string(&job).unwrap();
    let mut body: serde_json::Value = serde_json::from_str(&raw).unwrap();
    body["isolation"] = serde_json::json!("userspace");
    fs::write(&job, body.to_string()).unwrap();
    let out = dir.path().join("out");
    let output = Command::new(worker())
        .arg("run")
        .arg(&job)
        .arg("--output")
        .arg(&out)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "userspace run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt = fs::read_to_string(out.join("receipt.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&receipt).unwrap();
    assert_eq!(v["isolation"], "userspace", "{receipt}");
    let csv = fs::read_to_string(out.join("totals.csv")).unwrap();
    assert!(csv.contains('a'), "{csv}");
}

/// Requires Linux, `linux_native` health, and `kekkai-guest` on PATH or
/// `KEKKAI_GUEST_HELPER`. Offline `cargo test` skips this.
#[test]
#[ignore = "needs kekkai-guest helper; set KEKKAI_GUEST_HELPER"]
fn cli_run_userspace_cannot_read_host_secret() {
    assert!(
        userspace_ready(),
        "guest helper and linux_native health are required"
    );
    let dir = tempfile::tempdir().unwrap();
    let secret = dir.path().join("host-secret.txt");
    fs::write(&secret, "guest-escape-token").unwrap();
    let secret_path = secret.display().to_string().replace('\\', "\\\\");
    fs::write(dir.path().join("in.csv"), "x\n1\n").unwrap();
    let job = dir.path().join("job.json");
    fs::write(
        &job,
        format!(
            r#"{{
          "id": "escape",
          "sql": "COPY (SELECT * FROM read_csv_auto('{secret_path}')) TO 'outputs/totals.csv' (HEADER, DELIMITER ',');",
          "inputs": {{"in": "in.csv"}},
          "outputs": ["totals.csv"],
          "isolation": "userspace",
          "timeout_ms": 15000
        }}"#
        ),
    )
    .unwrap();
    let out = dir.path().join("out");
    let output = Command::new(worker())
        .arg("run")
        .arg(&job)
        .arg("--output")
        .arg(&out)
        .output()
        .unwrap();
    assert!(
        !output.status.success(),
        "guest SQL must not read a host secret"
    );
    assert!(
        !out.join("totals.csv").exists(),
        "escape must not write output"
    );
    assert!(
        !out.join("receipt.json").exists(),
        "escape must not write a receipt"
    );
}
