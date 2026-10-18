//! Encrypted job outputs: disk scan finds no plaintext; receipts list key ids.

use std::fs;
use std::sync::Arc;

use kako::crypto::{
    Dek, KeyProvider, KeyPurpose, MemoryKeyProvider, RuntimeProfile, TenantId, contains_plaintext,
};
use kako::local::LocalSource;
use kako::run::{JobFinish, RunnerConfig, run_source};
use kako::sandbox;
use kako::{Failure, JobKeyClaim, JobSource};

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

#[test]
fn encrypted_output_scan_finds_no_plaintext() {
    if !linux_native_ready() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let secret = format!("secret-{}", std::process::id());
    fs::write(dir.path().join("in.csv"), format!("note\n{secret}\n")).unwrap();
    let job_path = dir.path().join("job.json");
    fs::write(
        &job_path,
        r#"{
          "id": "enc-scan",
          "sql": "COPY (SELECT note FROM in) TO 'outputs/out.csv' (HEADER, DELIMITER ',');",
          "inputs": {"in": "in.csv"},
          "outputs": ["out.csv"],
          "tenant_id": "acme",
          "namespace": "default",
          "timeout_ms": 30000
        }"#,
    )
    .unwrap();
    let provider = Arc::new(MemoryKeyProvider::new(RuntimeProfile::Development).unwrap());
    let tenant = TenantId::new("acme").unwrap();
    let dek = Dek::generate();
    let wrapped = provider.wrap(&tenant, KeyPurpose::Store, &dek).unwrap();
    let mut source = LocalSource::from_job_file(&job_path, dir.path().join("out")).unwrap();
    let mut job = source.next_job().unwrap().unwrap();
    job.encryption = Some(JobKeyClaim {
        wrapped_dek: serde_json::to_string(&wrapped).unwrap(),
        dek_id: wrapped.dek_id.as_str().to_string(),
        kek_id: provider.describe().kek_id.as_str().to_string(),
    });
    source.release(&job).unwrap();
    let scratch_parent = dir.path().join("scratch");
    fs::create_dir_all(&scratch_parent).unwrap();
    let config = RunnerConfig {
        worker: worker(),
        scratch_parent,
        keys: Some(provider),
    };
    let finishes = run_source(&mut source, &config).unwrap();
    assert!(
        matches!(finishes[0], JobFinish::Completed { .. }),
        "{finishes:?}"
    );
    let out = fs::read(dir.path().join("out").join("out.csv")).unwrap();
    assert!(out.starts_with(b"KAKOSEAL"), "sealed magic missing");
    assert!(
        !contains_plaintext(&out, &secret),
        "plaintext leaked into published output"
    );
    let receipt = fs::read_to_string(dir.path().join("out").join("receipt.json")).unwrap();
    assert!(
        !contains_plaintext(receipt.as_bytes(), &secret),
        "plaintext leaked into receipt"
    );
    let v: serde_json::Value = serde_json::from_str(&receipt).unwrap();
    assert_eq!(v["key_state"], "ready");
    let ids = v["key_ids"].as_array().expect("key_ids");
    assert!(ids.iter().any(|id| id == wrapped.dek_id.as_str()));
}

#[test]
fn local_fail_writes_revoked_receipt() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("in.csv"), "x\n1\n").unwrap();
    let job_path = dir.path().join("job.json");
    fs::write(
        &job_path,
        r#"{
          "id": "enc-revoked",
          "sql": "SELECT 1",
          "inputs": {"in": "in.csv"},
          "outputs": ["out.csv"],
          "tenant_id": "acme",
          "namespace": "default"
        }"#,
    )
    .unwrap();
    let mut source = LocalSource::from_job_file(&job_path, dir.path().join("out")).unwrap();
    let job = source.next_job().unwrap().unwrap();
    source
        .fail(
            &job,
            &Failure::KeyRevoked {
                dek_id: "dek-revoked".into(),
                kek_id: "kek-1".into(),
            },
        )
        .unwrap();
    let failure = fs::read_to_string(dir.path().join("out").join("failure.txt")).unwrap();
    assert!(failure.contains("key revoked"), "{failure}");
    let receipt = fs::read_to_string(dir.path().join("out").join("receipt.json")).unwrap();
    let v: serde_json::Value = serde_json::from_str(&receipt).unwrap();
    assert_eq!(v["key_state"], "revoked");
    assert_eq!(v["key_ids"][0], "dek-revoked");
}
