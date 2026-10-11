//! Versioned JSON receipt written next to outputs.

use std::collections::BTreeMap;
use std::time::UNIX_EPOCH;

use serde::Serialize;

use crate::{Lineage, Receipt};

/// Schema 2 adds required `tenant_id` and `namespace` on every receipt.
const RECEIPT_VERSION: u32 = 2;

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
    lineage: Option<&'a Lineage>,
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
        lineage: receipt.lineage.as_ref(),
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
            lineage: None,
        };
        let json = to_json(&receipt).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["receipt_version"], 2);
        assert_eq!(v["job_id"], "j1");
        assert_eq!(v["tenant_id"], "t");
        assert_eq!(v["namespace"], "ns");
        assert_eq!(v["inputs"]["in"], "sha256:aa");
        assert_eq!(v["code"], "sha256:bb");
        assert_eq!(v["engine_version"], "duckdb 1.4.5");
        assert_eq!(v["outputs"]["out.csv"], "sha256:cc");
        assert_eq!(v["started_at_unix_ms"], 1_700_000_000_000_u64);
        assert_eq!(v["duration_ms"], 42);
        assert!(v.get("lineage").is_none(), "{json}");
    }
}
