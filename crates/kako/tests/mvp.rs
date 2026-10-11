//! End-to-end MVP: local job file, sandboxed DuckDB, receipt, cleanup.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use kako::Failure;
use kako::local::LocalSource;
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
        assert_eq!(v["receipt_version"], 2);
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
        &worker,
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
    assert_eq!(v["receipt_version"], 2);
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
