//! Versioned JSON receipt written next to outputs.

use std::collections::BTreeMap;
use std::time::UNIX_EPOCH;

use serde::Serialize;

use crate::{Lineage, Receipt};

/// Schema 3 adds required `isolation` (the tier that ran). Schema 4 adds
/// optional `credential_scope` (the job-scoped credential id, never the secret).
/// Schema 5 adds input/output markings and an optional declassify decision.
/// Schema 6 adds optional `key_ids` and `key_state` for encrypted runs.
const RECEIPT_VERSION: u32 = 6;

#[derive(Serialize)]
struct FileReceipt<'a> {
    receipt_version: u32,
    job_id: &'a str,
    tenant_id: &'a str,
    namespace: &'a str,
    inputs: BTreeMap<&'a str, &'a str>,
    code: &'a str,
    engine_version: &'a str,
    outputs: BTreeMap<&'a str, &'a str>,
    started_at_unix_ms: u64,
    duration_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    cpu_usec: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    memory_peak_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    lineage: Option<&'a Lineage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    runtime_image: Option<&'a str>,
    isolation: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    credential_scope: Option<&'a str>,
    input_markings: BTreeMap<&'a str, Vec<&'a str>>,
    output_markings: BTreeMap<&'a str, Vec<&'a str>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    declassify: Option<&'a crate::DeclassifyDecision>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    key_ids: Vec<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    key_state: Option<&'a str>,
    /// The registry model a scoring job scored with.
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<&'a crate::ModelRef>,
}

/// Serialize a receipt as stable JSON (sorted maps, trailing newline).
pub fn to_json(receipt: &Receipt) -> Result<String, String> {
    let started_at_unix_ms = receipt
        .started_at
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let body = FileReceipt {
        receipt_version: RECEIPT_VERSION,
        job_id: &receipt.job_id,
        tenant_id: &receipt.tenant_id,
        namespace: &receipt.namespace,
        inputs: receipt
            .inputs
            .iter()
            .map(|(k, d)| (k.as_str(), d.0.as_str()))
            .collect(),
        code: &receipt.code.0,
        engine_version: &receipt.engine_version,
        outputs: receipt
            .outputs
            .iter()
            .map(|(k, d)| (k.as_str(), d.0.as_str()))
            .collect(),
        started_at_unix_ms,
        duration_ms: receipt.duration.as_millis() as u64,
        cpu_usec: receipt.cpu_usec,
        memory_peak_bytes: receipt.memory_peak_bytes,
        lineage: receipt.lineage.as_ref(),
        runtime_image: receipt.runtime_image.as_deref(),
        isolation: receipt.isolation.as_str(),
        credential_scope: receipt.credential_scope.as_deref(),
        input_markings: receipt
            .input_markings
            .iter()
            .map(|(name, labels)| (name.as_str(), labels.iter().map(String::as_str).collect()))
            .collect(),
        output_markings: receipt
            .output_markings
            .iter()
            .map(|(name, labels)| (name.as_str(), labels.iter().map(String::as_str).collect()))
            .collect(),
        declassify: receipt.declassify.as_ref(),
        key_ids: receipt.key_ids.iter().map(String::as_str).collect(),
        key_state: receipt.key_state.as_deref(),
        model: receipt.model.as_ref(),
    };
    let mut json = serde_json::to_string_pretty(&body)
        .map_err(|error| format!("serialize receipt: {error}"))?;
    json.push('\n');
    Ok(json)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Digest, Receipt};
    use std::time::{Duration, SystemTime};

    #[test]
    fn json_contains_required_fields() {
        let receipt = Receipt {
            job_id: "j1".into(),
            tenant_id: "t".into(),
            namespace: "ns".into(),
            inputs: vec![("in".into(), Digest("sha256:aa".into()))],
            code: Digest("sha256:bb".into()),
            engine_version: "duckdb 1.4.5".into(),
            outputs: vec![("out.csv".into(), Digest("sha256:cc".into()))],
            started_at: SystemTime::UNIX_EPOCH + Duration::from_millis(1_700_000_000_000),
            duration: Duration::from_millis(42),
            cpu_usec: None,
            memory_peak_bytes: None,
            lineage: None,
            runtime_image: None,
            isolation: crate::IsolationTier::LinuxNative,
            credential_scope: None,
            input_markings: Vec::new(),
            output_markings: Vec::new(),
            declassify: None,
            key_ids: Vec::new(),
            key_state: None,
            model: None,
        };
        let json = to_json(&receipt).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["receipt_version"], 6);
        assert_eq!(v["job_id"], "j1");
        assert_eq!(v["tenant_id"], "t");
        assert_eq!(v["namespace"], "ns");
        assert_eq!(v["inputs"]["in"], "sha256:aa");
        assert_eq!(v["code"], "sha256:bb");
        assert_eq!(v["engine_version"], "duckdb 1.4.5");
        assert_eq!(v["outputs"]["out.csv"], "sha256:cc");
        assert_eq!(v["started_at_unix_ms"], 1_700_000_000_000_u64);
        assert_eq!(v["duration_ms"], 42);
        assert!(v.get("cpu_usec").is_none(), "{json}");
        assert!(v.get("memory_peak_bytes").is_none(), "{json}");
        assert!(v.get("lineage").is_none(), "{json}");
        assert!(v.get("runtime_image").is_none(), "{json}");
        assert!(v.get("credential_scope").is_none(), "{json}");
        assert_eq!(v["isolation"], "linux_native");
        assert!(v.get("key_ids").is_none(), "{json}");
        assert!(v.get("key_state").is_none(), "{json}");
    }

    #[test]
    fn json_echoes_measured_usage() {
        let receipt = Receipt {
            job_id: "j1".into(),
            tenant_id: "t".into(),
            namespace: "ns".into(),
            inputs: vec![],
            code: Digest("sha256:bb".into()),
            engine_version: "duckdb 1.4.5".into(),
            outputs: vec![],
            started_at: SystemTime::UNIX_EPOCH,
            duration: Duration::from_millis(1),
            cpu_usec: Some(12_345),
            memory_peak_bytes: Some(4_096),
            lineage: None,
            runtime_image: Some("python-lockfile:sha256:aa".into()),
            isolation: crate::IsolationTier::Userspace,
            credential_scope: Some("cred-7".into()),
            input_markings: vec![("in".into(), vec!["pii".into()])],
            output_markings: vec![("out.csv".into(), vec!["pii".into()])],
            declassify: Some(crate::DeclassifyDecision {
                id: "dec-1".into(),
                from: vec!["secret".into()],
                reason: "public aggregate".into(),
            }),
            key_ids: Vec::new(),
            key_state: None,
            model: None,
        };
        let json = to_json(&receipt).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["cpu_usec"], 12_345);
        assert_eq!(v["memory_peak_bytes"], 4_096);
        assert_eq!(v["runtime_image"], "python-lockfile:sha256:aa");
        assert_eq!(v["isolation"], "userspace");
        assert_eq!(v["credential_scope"], "cred-7");
        assert_eq!(v["input_markings"]["in"], serde_json::json!(["pii"]));
        assert_eq!(v["output_markings"]["out.csv"], serde_json::json!(["pii"]));
        assert_eq!(v["declassify"]["id"], "dec-1");
        assert_eq!(v["declassify"]["from"], serde_json::json!(["secret"]));
        assert_eq!(v["declassify"]["reason"], "public aggregate");
        assert!(v.get("key_ids").is_none(), "{json}");
        assert!(v.get("key_state").is_none(), "{json}");
    }

    #[test]
    fn json_echoes_key_ids() {
        let receipt = Receipt {
            job_id: "j1".into(),
            tenant_id: "t".into(),
            namespace: "ns".into(),
            inputs: vec![],
            code: Digest("sha256:bb".into()),
            engine_version: "duckdb 1.4.5".into(),
            outputs: vec![],
            started_at: SystemTime::UNIX_EPOCH,
            duration: Duration::from_millis(1),
            cpu_usec: None,
            memory_peak_bytes: None,
            lineage: None,
            runtime_image: None,
            isolation: crate::IsolationTier::LinuxNative,
            credential_scope: None,
            input_markings: Vec::new(),
            output_markings: Vec::new(),
            declassify: None,
            key_ids: vec!["dek-1".into(), "kek-1".into()],
            key_state: Some("ready".into()),
            model: None,
        };
        let json = to_json(&receipt).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["receipt_version"], 6);
        assert_eq!(v["key_ids"], serde_json::json!(["dek-1", "kek-1"]));
        assert_eq!(v["key_state"], "ready");
    }
}
