//! Replay end to end: a pinned re-run reproduces the original output digest.

use std::fs;
use std::path::Path;
use std::process::Command;

use kako::sandbox;

fn linux_native_ready() -> bool {
    match sandbox::select_backend() {
        Ok(backend) if backend.to_string() == "linux_native" => true,
        other => {
            eprintln!("skip: sandboxed run needs linux_native, got {other:?}");
            false
        }
    }
}

fn kako(args: &[&str], dir: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_kako"))
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap()
}

fn json(path: &Path) -> serde_json::Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

const SQL: &str = "COPY (SELECT o.customer_id, sum(o.amount) AS total FROM orders o GROUP BY 1 ORDER BY 1) TO 'outputs/totals.csv' (HEADER, DELIMITER ',');";

#[test]
fn replay_with_unchanged_inputs_reproduces_the_original_digest() {
    if !linux_native_ready() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    fs::write(
        dir.path().join("orders.csv"),
        "customer_id,amount\n1,10\n1,15\n2,7\n",
    )
    .unwrap();
    let job = |name: &str, sql: &str| {
        let body = serde_json::json!({
            "id": name, "sql": sql, "inputs": {"orders": "orders.csv"},
            "outputs": ["totals.csv"], "timeout_ms": 30000,
        });
        fs::write(dir.path().join(format!("{name}.json")), body.to_string()).unwrap();
    };
    job("original", SQL);
    let run = kako(
        &["run", "original.json", "--output", "original"],
        dir.path(),
    );
    assert!(run.status.success(), "{run:?}");
    let receipt = json(&dir.path().join("original/receipt.json"));

    let pins = |code: &serde_json::Value| {
        serde_json::json!({
            "code": code,
            "engine_version": receipt["engine_version"],
            "inputs": receipt["inputs"],
            "outputs": receipt["outputs"],
        })
    };
    let replay = serde_json::json!({
        "id": "r1", "branch": "replay/r1",
        "steps": [{"id": "totals", "job": "original.json", "pins": pins(&receipt["code"])}],
    });
    fs::write(dir.path().join("replay.json"), replay.to_string()).unwrap();
    let check = kako(&["replay", "replay.json", "--check"], dir.path());
    assert!(check.status.success(), "{check:?}");
    let out = kako(
        &["replay", "replay.json", "--output", "branches"],
        dir.path(),
    );
    assert!(out.status.success(), "{out:?}");
    let report = json(&dir.path().join("branches/replay%2Fr1/replay.json"));
    assert_eq!(report["reproducible"], true, "{report}");
    let step = &report["steps"][0];
    assert_eq!(step["status"], "reproduced", "{report}");
    assert_eq!(
        step["outputs"]["totals.csv"]["actual"],
        receipt["outputs"]["totals.csv"]
    );
    let step_receipt = json(&dir.path().join("branches/replay%2Fr1/totals/receipt.json"));
    assert_eq!(step_receipt["outputs"], receipt["outputs"]);
    assert_eq!(
        fs::read(dir.path().join("branches/replay%2Fr1/totals/totals.csv")).unwrap(),
        fs::read(dir.path().join("original/totals.csv")).unwrap()
    );

    // A changed transform pins its own code and reports the divergence.
    let changed = SQL.replace("sum(o.amount)", "max(o.amount)");
    job("changed", &changed);
    let code = kako::digest::hash_bytes(changed.as_bytes()).0;
    let replay = serde_json::json!({
        "id": "r2", "branch": "replay/r2",
        "steps": [{"id": "totals", "job": "changed.json", "pins": pins(&code.into())}],
    });
    fs::write(dir.path().join("replay.json"), replay.to_string()).unwrap();
    let out = kako(
        &["replay", "replay.json", "--output", "branches"],
        dir.path(),
    );
    assert!(out.status.success(), "{out:?}");
    let report = json(&dir.path().join("branches/replay%2Fr2/replay.json"));
    assert_eq!(report["steps"][0]["status"], "diverged", "{report}");
    assert_ne!(
        report["steps"][0]["outputs"]["totals.csv"]["actual"],
        receipt["outputs"]["totals.csv"]
    );
}
