//! `kako validate` and `kako preview` against the sandboxed DuckDB engine.

use std::fs;
use std::path::Path;
use std::process::Command;

use kako::sandbox;

fn kako() -> Command {
    Command::new(env!("CARGO_BIN_EXE_kako"))
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

/// Join job over two CSVs with join condition `on` and aggregate `total`.
fn join_job(dir: &Path, on: &str, total: &str) -> std::path::PathBuf {
    fs::write(dir.join("customers.csv"), "id,name\n1,a\n2,b\n").unwrap();
    fs::write(
        dir.join("orders.csv"),
        "customer_id,amount\n1,10\n1,15\n2,7\n",
    )
    .unwrap();
    let job = dir.join("job.json");
    let body = serde_json::json!({
        "id": "preview-join",
        "sql": format!("COPY (\n  SELECT c.name, {total} AS total\n  FROM customers c\n  JOIN orders o ON {on}\n  GROUP BY c.name\n  ORDER BY c.name\n) TO 'outputs/totals.csv' (HEADER, DELIMITER ',');"),
        "inputs": {"customers": "customers.csv", "orders": "orders.csv"},
        "outputs": ["totals.csv"],
        "timeout_ms": 30000
    });
    fs::write(&job, body.to_string()).unwrap();
    job
}

#[test]
fn validate_reports_bad_join_key_and_type_mismatch_before_a_run() {
    if !linux_native_ready() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    for (on, total) in [
        ("o.customer = c.id", "sum(o.amount)"),
        ("o.customer_id = c.id", "sum(c.name)"),
    ] {
        let job = join_job(dir.path(), on, total);
        let out = kako().arg("validate").arg(&job).output().unwrap();
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{on} / {total} validated");
        assert!(
            stderr.contains("Binder Error") && stderr.contains("LINE"),
            "{stderr}"
        );
    }
    let job = join_job(dir.path(), "o.customer_id = c.id", "sum(o.amount)");
    let out = kako().arg("validate").arg(&job).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn preview_returns_sample_rows_and_writes_no_outputs() {
    if !linux_native_ready() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let job = join_job(dir.path(), "o.customer_id = c.id", "sum(o.amount)");
    let before: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    let out = kako()
        .current_dir(dir.path())
        .args(["preview", "--rows", "2"])
        .arg(&job)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let preview: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(preview["committing"], false);
    assert_eq!(preview["receipt"]["job_id"], "preview-join");
    // Two order lines survive the cap, both for customer 1.
    assert_eq!(
        preview["outputs"][0]["rows"],
        serde_json::json!([["a", "25"]])
    );
    let after: Vec<_> = fs::read_dir(dir.path())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(before, after, "preview wrote into the job directory");
}
