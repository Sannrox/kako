//! Versioned JSON receipt written next to outputs.
//!
//! [`to_json`] writes the current schema. [`from_json`] reads it and the
//! previous one (N-1), so a release can open receipts its predecessor wrote,
//! and refuses any other version instead of guessing at its fields.

use std::collections::BTreeMap;
use std::time::{Duration, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::{Digest, IsolationTier, Lineage, Receipt};

/// Schema 3 adds required `isolation` (the tier that ran). Schema 4 adds
/// optional `credential_scope` (the job-scoped credential id, never the secret).
/// Schema 5 adds input/output markings and an optional declassify decision.
/// Schema 6 adds optional `key_ids` and `key_state` for encrypted runs.
const RECEIPT_VERSION: u32 = 6;
/// Oldest schema [`from_json`] opens: one version back.
const OLDEST_READABLE_VERSION: u32 = RECEIPT_VERSION - 1;

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

/// Owned mirror of [`FileReceipt`] for reading. Fields added after
/// [`OLDEST_READABLE_VERSION`] default when absent.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredReceipt {
    /// Checked by [`VersionOnly`] before the strict parse, so a newer receipt
    /// reports its version rather than an unknown field.
    #[serde(rename = "receipt_version")]
    _receipt_version: u32,
    job_id: String,
    tenant_id: String,
    namespace: String,
    inputs: BTreeMap<String, String>,
    code: String,
    engine_version: String,
    outputs: BTreeMap<String, String>,
    started_at_unix_ms: u64,
    duration_ms: u64,
    #[serde(default)]
    cpu_usec: Option<u64>,
    #[serde(default)]
    memory_peak_bytes: Option<u64>,
    #[serde(default)]
    lineage: Option<Lineage>,
    #[serde(default)]
    runtime_image: Option<String>,
    isolation: String,
    #[serde(default)]
    credential_scope: Option<String>,
    input_markings: BTreeMap<String, Vec<String>>,
    output_markings: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    declassify: Option<crate::DeclassifyDecision>,
    #[serde(default)]
    key_ids: Vec<String>,
    #[serde(default)]
    key_state: Option<String>,
    #[serde(default)]
    model: Option<crate::ModelRef>,
}

/// Parse a receipt written by this release or the previous one. Any other
/// `receipt_version`, including a newer one, fails closed.
pub fn from_json(json: &str) -> Result<Receipt, String> {
    let version: VersionOnly =
        serde_json::from_str(json).map_err(|error| format!("parse receipt: {error}"))?;
    if !(OLDEST_READABLE_VERSION..=RECEIPT_VERSION).contains(&version.receipt_version) {
        return Err(format!(
            "receipt_version {} is not supported; this release reads {OLDEST_READABLE_VERSION} through {RECEIPT_VERSION}",
            version.receipt_version
        ));
    }
    let stored: StoredReceipt =
        serde_json::from_str(json).map_err(|error| format!("parse receipt: {error}"))?;
    let isolation = stored
        .isolation
        .parse::<IsolationTier>()
        .map_err(|error| format!("parse receipt isolation: {error}"))?;
    Ok(Receipt {
        job_id: stored.job_id,
        inputs: digests(stored.inputs),
        code: Digest(stored.code),
        engine_version: stored.engine_version,
        outputs: digests(stored.outputs),
        started_at: UNIX_EPOCH + Duration::from_millis(stored.started_at_unix_ms),
        duration: Duration::from_millis(stored.duration_ms),
        tenant_id: stored.tenant_id,
        namespace: stored.namespace,
        cpu_usec: stored.cpu_usec,
        memory_peak_bytes: stored.memory_peak_bytes,
        lineage: stored.lineage,
        runtime_image: stored.runtime_image,
        isolation,
        credential_scope: stored.credential_scope,
        input_markings: stored.input_markings.into_iter().collect(),
        output_markings: stored.output_markings.into_iter().collect(),
        declassify: stored.declassify,
        key_ids: stored.key_ids,
        key_state: stored.key_state,
        model: stored.model,
    })
}

#[derive(Deserialize)]
struct VersionOnly {
    receipt_version: u32,
}

fn digests(map: BTreeMap<String, String>) -> Vec<(String, Digest)> {
    map.into_iter()
        .map(|(name, digest)| (name, Digest(digest)))
        .collect()
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

    /// A receipt written by the previous release (schema 5, before key ids).
    /// When `RECEIPT_VERSION` moves on, this test fails until the new N-1
    /// fixture is committed alongside it.
    const RECEIPT_V5: &str = include_str!("../tests/fixtures/receipt-v5.json");

    #[test]
    fn previous_schema_fixture_opens() {
        let receipt = from_json(RECEIPT_V5).unwrap();
        assert_eq!(receipt.job_id, "nightly-orders");
        assert_eq!(receipt.isolation, IsolationTier::LinuxNative);
        assert_eq!(receipt.credential_scope.as_deref(), Some("cred-orders-ro"));
        assert_eq!(
            receipt.input_markings,
            vec![("orders.csv".to_string(), vec!["pii".to_string()])]
        );
        assert!(receipt.key_ids.is_empty());
        assert_eq!(receipt.key_state, None);
    }

    #[test]
    fn current_schema_round_trips() {
        let original = from_json(RECEIPT_V5).unwrap();
        let mut current = original.clone();
        current.key_ids = vec!["dek-1".into(), "kek-1".into()];
        current.key_state = Some("ready".into());
        let json = to_json(&current).unwrap();
        assert_eq!(from_json(&json).unwrap(), current);
    }

    #[test]
    fn unknown_versions_fail_closed() {
        for version in [RECEIPT_VERSION + 1, OLDEST_READABLE_VERSION - 1, 0] {
            let json = RECEIPT_V5.replace(
                "\"receipt_version\": 5",
                &format!("\"receipt_version\": {version}"),
            );
            let error = from_json(&json).unwrap_err();
            assert!(error.contains("not supported"), "{version}: {error}");
        }
        // A newer schema's extra field is reported as a version problem first.
        let future = RECEIPT_V5
            .replace("\"receipt_version\": 5", "\"receipt_version\": 7")
            .replace("\"job_id\"", "\"retention\": \"30d\",\n  \"job_id\"");
        assert!(from_json(&future).unwrap_err().contains("not supported"));
    }
}
