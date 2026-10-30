//! Map kako jobs and receipts onto the Sekai governed-transform JobSpec and
//! run receipt ([ADR 0012](../../../docs/decisions/0012-governed-receipt-mapping.md)).
//!
//! kako-core stays free of Sekai crates. The digest and JSON field names
//! match the plane's `definition_digest` and `TransformRun` contract.

use std::collections::BTreeMap;

use serde::Serialize;
use sha2::{Digest as Sha256Digest, Sha256};

use crate::{Digest, Job, Lineage, Receipt};

/// Plane JobSpec contract id hashed into every definition digest.
pub const CONTRACT_VERSION: &str = "sekai.governed-transform/v1";
/// JobSpec profile kako runs: a sandboxed child, not in-process projection.
pub const ISOLATED_CLASS: &str = "isolated_process";
/// Placement label a runtime must register to be offered object-set work.
pub const OBJECT_SET_INPUT_LABEL: &str = "sekai.input.object-set";
/// The object-set row format a labelled runtime understands.
pub const OBJECT_SET_INPUT_LABEL_VALUE: &str = "v1";
/// Prefix of an object-set input's snapshot `dataset_id`.
pub const OBJECT_SET_PIN_PREFIX: &str = "object-set:";
const TRANSFORM_WORK_COMPLETED: &str = "completed";
const TRANSFORM_TRIGGER_MANUAL: &str = "manual";

/// Named isolated-process input (`alias` / `dataset_id` or `object_set`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct TransformInput {
    pub alias: String,
    /// Empty for an object-set input.
    #[serde(default)]
    pub dataset_id: String,
    /// Canonical object-set descriptor JSON. Omitted when empty so dataset
    /// inputs keep their digest.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub object_set: String,
}

impl TransformInput {
    pub fn is_object_set(&self) -> bool {
        !self.object_set.is_empty()
    }
}

/// In-process step. Isolated-process JobSpecs keep this empty.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct TransformStep {
    pub kind: String,
    pub column: String,
    pub op: String,
    pub value: String,
    pub columns: Vec<String>,
}

/// Isolated-process envelope hashed into `definition_digest`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct IsolatedProcessSpec {
    pub engine: String,
    pub runtime_image: String,
    pub inputs: Vec<TransformInput>,
    /// Placement labels a claiming runtime must carry. Empty keeps the
    /// digest of definitions stored before placement existed.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub requires: BTreeMap<String, String>,
    /// The code `engine` runs. Empty on definitions stored before it existed.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub code: String,
}

/// Sekai `GovernedTransform` isolated-process JobSpec. JSON of this type is
/// the definition the plane content-addresses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobSpec {
    pub contract_version: String,
    pub namespace: String,
    pub transform_id: String,
    pub input_dataset_id: String,
    pub output_dataset_id: String,
    pub steps: Vec<TransformStep>,
    pub quality_rule: String,
    pub definition_digest: String,
    pub isolated_process: IsolatedProcessSpec,
}

/// Sekai `TransformRun` field set. JSON of this type is a run receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransformRun {
    pub run_id: String,
    pub namespace: String,
    pub transform_id: String,
    pub definition_digest: String,
    pub input_digest: String,
    pub output_digest: String,
    pub last_input_row_id: i64,
    pub incremental: bool,
    pub quarantined: bool,
    pub quality_rule: String,
    pub rows_in: i32,
    pub rows_out: i32,
    pub lineage_parent: String,
    pub created_at_ms: i64,
    pub markings_in: String,
    pub markings_out: String,
    pub profile: String,
    pub engine: String,
    pub runtime_image: String,
    pub dispatch_status: String,
    pub trigger_cause: String,
    pub trigger_ref: String,
    pub input_partitions: Vec<TransformPartition>,
    pub output_partitions: Vec<TransformPartition>,
}

/// Partition identity on a mapped run receipt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct TransformPartition {
    pub partition_key: String,
    pub digest: String,
    pub rows: i32,
}

/// Build the isolated-process JobSpec for a kako job and content-address it.
pub fn job_spec(job: &Job) -> JobSpec {
    isolated_job_spec(
        &job.namespace,
        &job.id,
        job.outputs.first().cloned().unwrap_or_default(),
        IsolatedProcessSpec {
            engine: job.engine.as_str().to_string(),
            runtime_image: job.runtime_image.clone().unwrap_or_default(),
            inputs: transform_inputs_from_job(job),
            requires: object_set_requires(
                job.inputs.iter().any(|input| !input.object_set.is_empty()),
            ),
            code: String::new(),
        },
    )
}

/// Isolated-process JobSpec from plane fields, content-addressed the same way.
pub fn isolated_job_spec(
    namespace: impl Into<String>,
    transform_id: impl Into<String>,
    output_dataset_id: impl Into<String>,
    isolated: IsolatedProcessSpec,
) -> JobSpec {
    let input_dataset_id = isolated
        .inputs
        .iter()
        .find(|input| !input.is_object_set())
        .map(|input| input.dataset_id.clone())
        .unwrap_or_default();
    let mut spec = JobSpec {
        contract_version: CONTRACT_VERSION.into(),
        namespace: namespace.into(),
        transform_id: transform_id.into(),
        input_dataset_id,
        output_dataset_id: output_dataset_id.into(),
        steps: Vec::new(),
        quality_rule: String::new(),
        definition_digest: String::new(),
        isolated_process: isolated,
    };
    spec.definition_digest = definition_digest(&spec);
    spec
}

/// Recompute the isolated-process digest and refuse a mismatch.
pub fn require_matching_digest(spec: &JobSpec, claimed: &str) -> Result<(), String> {
    let computed = definition_digest(spec);
    if computed != claimed {
        return Err(
            "claimed definition digest does not match the isolated-process envelope".into(),
        );
    }
    Ok(())
}

fn transform_inputs_from_job(job: &Job) -> Vec<TransformInput> {
    job.inputs
        .iter()
        .map(|input| {
            if input.object_set.is_empty() {
                TransformInput {
                    alias: input.name.clone(),
                    dataset_id: input.name.clone(),
                    object_set: String::new(),
                }
            } else {
                TransformInput {
                    alias: input.name.clone(),
                    dataset_id: String::new(),
                    object_set: canonical_object_set(&input.object_set),
                }
            }
        })
        .collect()
}

fn object_set_requires(present: bool) -> BTreeMap<String, String> {
    if present {
        BTreeMap::from([(
            OBJECT_SET_INPUT_LABEL.into(),
            OBJECT_SET_INPUT_LABEL_VALUE.into(),
        )])
    } else {
        BTreeMap::new()
    }
}

/// Compact object-set JSON with keys in order, matching the plane descriptor.
pub fn canonical_object_set(json: &str) -> String {
    serde_json::from_str::<serde_json::Value>(json)
        .ok()
        .filter(serde_json::Value::is_object)
        .and_then(|value| serde_json::to_string(&value).ok())
        .unwrap_or_else(|| json.to_string())
}

/// Map a completed kako receipt onto a Sekai run receipt.
pub fn transform_run(job: &Job, receipt: &Receipt) -> TransformRun {
    let spec = job_spec(job);
    let (rows_in, rows_out, output_partitions) = lineage_counts(receipt.lineage.as_ref());
    TransformRun {
        run_id: receipt.job_id.clone(),
        namespace: receipt.namespace.clone(),
        transform_id: spec.transform_id.clone(),
        definition_digest: spec.definition_digest,
        input_digest: combined_digest(&receipt.inputs),
        output_digest: combined_digest(&receipt.outputs),
        last_input_row_id: 0,
        incremental: false,
        quarantined: false,
        quality_rule: String::new(),
        rows_in,
        rows_out,
        lineage_parent: String::new(),
        created_at_ms: unix_ms(receipt.started_at),
        markings_in: crate::marking::joined(&receipt.input_markings),
        markings_out: crate::marking::joined(&receipt.output_markings),
        profile: ISOLATED_CLASS.into(),
        engine: spec.isolated_process.engine,
        runtime_image: receipt
            .runtime_image
            .clone()
            .unwrap_or(spec.isolated_process.runtime_image),
        dispatch_status: TRANSFORM_WORK_COMPLETED.into(),
        trigger_cause: TRANSFORM_TRIGGER_MANUAL.into(),
        trigger_ref: String::new(),
        input_partitions: Vec::new(),
        output_partitions,
    }
}

/// Stable JSON of a mapped run receipt, trailing newline.
pub fn transform_run_json(job: &Job, receipt: &Receipt) -> Result<String, String> {
    let mut json = serde_json::to_string_pretty(&transform_run(job, receipt))
        .map_err(|error| format!("serialize transform run: {error}"))?;
    json.push('\n');
    Ok(json)
}

fn definition_digest(spec: &JobSpec) -> String {
    let mut hasher = Sha256::new();
    hash_field(&mut hasher, CONTRACT_VERSION);
    hash_field(&mut hasher, &spec.namespace);
    hash_field(&mut hasher, &spec.transform_id);
    hash_field(&mut hasher, &spec.input_dataset_id);
    hash_field(&mut hasher, &spec.output_dataset_id);
    hash_field(&mut hasher, &spec.quality_rule);
    let steps = serde_json::to_vec(&spec.steps).expect("steps json");
    hasher.update((steps.len() as u64).to_le_bytes());
    hasher.update(&steps);
    hash_field(&mut hasher, ISOLATED_CLASS);
    hash_field(&mut hasher, &spec.isolated_process.engine);
    hash_field(&mut hasher, &spec.isolated_process.runtime_image);
    let inputs_json =
        serde_json::to_vec(&spec.isolated_process.inputs).expect("job spec inputs json");
    hasher.update((inputs_json.len() as u64).to_le_bytes());
    hasher.update(&inputs_json);
    if !spec.isolated_process.code.is_empty() {
        hash_field(&mut hasher, "code");
        hash_field(&mut hasher, &spec.isolated_process.code);
    }
    if !spec.isolated_process.requires.is_empty() {
        hash_field(&mut hasher, "requires");
        hasher.update((spec.isolated_process.requires.len() as u64).to_le_bytes());
        for (key, value) in &spec.isolated_process.requires {
            hash_field(&mut hasher, key);
            hash_field(&mut hasher, value);
        }
    }
    format!("sha256:{:x}", hasher.finalize())
}

fn hash_field(hasher: &mut Sha256, value: &str) {
    hasher.update((value.len() as u64).to_le_bytes());
    hasher.update(value.as_bytes());
}

fn combined_digest(entries: &[(String, Digest)]) -> String {
    match entries {
        [] => String::new(),
        [(_, digest)] => digest.0.clone(),
        many => {
            let mut hasher = Sha256::new();
            for (name, digest) in many {
                hash_field(&mut hasher, name);
                hash_field(&mut hasher, &digest.0);
            }
            format!("sha256:{:x}", hasher.finalize())
        }
    }
}

fn lineage_counts(lineage: Option<&Lineage>) -> (i32, i32, Vec<TransformPartition>) {
    let Some(lineage) = lineage else {
        return (0, 0, Vec::new());
    };
    let rows_out = lineage
        .writes
        .iter()
        .map(|write| i32::try_from(write.rows).unwrap_or(i32::MAX))
        .fold(0i32, i32::saturating_add);
    let partitions = lineage
        .writes
        .iter()
        .flat_map(|write| {
            let rows = i32::try_from(write.rows).unwrap_or(i32::MAX);
            let per_partition_rows = if write.partitions.len() == 1 { rows } else { 0 };
            write
                .partitions
                .iter()
                .cloned()
                .map(move |partition_key| TransformPartition {
                    partition_key,
                    digest: String::new(),
                    rows: per_partition_rows,
                })
        })
        .collect();
    (0, rows_out, partitions)
}

fn unix_ms(started_at: std::time::SystemTime) -> i64 {
    started_at
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Engine, Input, IsolationTier, LineageWrite, WriteMode};
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime};

    fn sql_job() -> Job {
        Job {
            id: "join-agg".into(),
            engine: Engine::DuckDbSql,
            code: "SELECT 1".into(),
            inputs: vec![
                Input {
                    name: "customers".into(),
                    path: PathBuf::from("customers.csv"),
                    markings: Vec::new(),
                    column_markings: Default::default(),
                    object_set: String::new(),
                },
                Input {
                    name: "orders".into(),
                    path: PathBuf::from("orders.csv"),
                    markings: Vec::new(),
                    column_markings: Default::default(),
                    object_set: String::new(),
                },
            ],
            outputs: vec!["totals.csv".into()],
            write_mode: WriteMode::Overwrite,
            wall_clock: Duration::from_secs(30),
            cpu_time: None,
            memory_bytes: None,
            tenant_id: "local".into(),
            namespace: "ops".into(),
            runtime_image: None,
            isolation: IsolationTier::LinuxNative,
            output_markings: Default::default(),
            declassify: None,
            encryption: None,
            model: None,
        }
    }

    fn python_job() -> Job {
        Job {
            id: "reshape".into(),
            engine: Engine::Python,
            code: "print(1)\n".into(),
            inputs: vec![Input {
                name: "events".into(),
                path: PathBuf::from("events.csv"),
                markings: Vec::new(),
                column_markings: Default::default(),
                object_set: String::new(),
            }],
            outputs: vec!["out.csv".into()],
            write_mode: WriteMode::Overwrite,
            wall_clock: Duration::from_secs(30),
            cpu_time: None,
            memory_bytes: Some(64 * 1024 * 1024),
            tenant_id: "local".into(),
            namespace: "ops".into(),
            runtime_image: Some("python-lockfile:sha256:aa".into()),
            isolation: IsolationTier::LinuxNative,
            output_markings: Default::default(),
            declassify: None,
            encryption: None,
            model: None,
        }
    }

    fn receipt_for(job: &Job, outputs: Vec<(String, Digest)>) -> Receipt {
        Receipt {
            job_id: job.id.clone(),
            inputs: job
                .inputs
                .iter()
                .enumerate()
                .map(|(i, input)| (input.name.clone(), Digest(format!("sha256:in{i}"))))
                .collect(),
            code: crate::digest::hash_bytes(job.code.as_bytes()),
            engine_version: match job.engine {
                Engine::DuckDbSql => "duckdb 1.4.5".into(),
                Engine::Python => "python 3.12".into(),
                Engine::Mizuchi => "mizuchi".into(),
            },
            outputs,
            started_at: SystemTime::UNIX_EPOCH + Duration::from_millis(1_700_000_000_000),
            duration: Duration::from_millis(42),
            tenant_id: job.tenant_id.clone(),
            namespace: job.namespace.clone(),
            cpu_usec: None,
            memory_peak_bytes: None,
            lineage: None,
            runtime_image: job.runtime_image.clone(),
            isolation: job.isolation,
            credential_scope: None,
            input_markings: Vec::new(),
            output_markings: Vec::new(),
            declassify: None,
            key_ids: Vec::new(),
            key_state: None,
            model: None,
        }
    }

    const RUN_FIELDS: &[&str] = &[
        "run_id",
        "namespace",
        "transform_id",
        "definition_digest",
        "input_digest",
        "output_digest",
        "last_input_row_id",
        "incremental",
        "quarantined",
        "quality_rule",
        "rows_in",
        "rows_out",
        "lineage_parent",
        "created_at_ms",
        "markings_in",
        "markings_out",
        "profile",
        "engine",
        "runtime_image",
        "dispatch_status",
        "trigger_cause",
        "trigger_ref",
        "input_partitions",
        "output_partitions",
    ];

    fn parse_run(json: &str) -> TransformRun {
        let value: serde_json::Value = serde_json::from_str(json).expect("run json");
        let mut keys: Vec<&str> = value
            .as_object()
            .expect("run object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        let mut expected = RUN_FIELDS.to_vec();
        expected.sort_unstable();
        assert_eq!(keys, expected, "{json}");
        serde_json::from_value(value).expect("sekai run receipt json")
    }

    #[test]
    fn sql_receipt_validates_as_sekai_run_receipt() {
        let job = sql_job();
        let receipt = receipt_for(
            &job,
            vec![("totals.csv".into(), Digest("sha256:out".into()))],
        );
        let json = transform_run_json(&job, &receipt).unwrap();
        let run = parse_run(&json);
        assert_eq!(run.run_id, "join-agg");
        assert_eq!(run.namespace, "ops");
        assert_eq!(run.transform_id, "join-agg");
        assert_eq!(run.profile, ISOLATED_CLASS);
        assert_eq!(run.engine, "duckdb_sql");
        assert_eq!(run.runtime_image, "");
        assert_eq!(run.dispatch_status, "completed");
        assert_eq!(run.output_digest, "sha256:out");
        assert!(
            run.definition_digest.starts_with("sha256:"),
            "{}",
            run.definition_digest
        );
        assert_eq!(run.definition_digest, job_spec(&job).definition_digest);
        assert_eq!(run.created_at_ms, 1_700_000_000_000);
        assert_eq!(run.input_digest, combined_digest(&receipt.inputs));
        assert_ne!(run.input_digest, "sha256:in0");
    }

    #[test]
    fn python_receipt_validates_as_sekai_run_receipt() {
        let job = python_job();
        let receipt = receipt_for(&job, vec![("out.csv".into(), Digest("sha256:py".into()))]);
        let json = transform_run_json(&job, &receipt).unwrap();
        let run = parse_run(&json);
        assert_eq!(run.run_id, "reshape");
        assert_eq!(run.engine, "python");
        assert_eq!(run.runtime_image, "python-lockfile:sha256:aa");
        assert_eq!(run.profile, ISOLATED_CLASS);
        assert_eq!(run.output_digest, "sha256:py");
        assert_eq!(run.input_digest, "sha256:in0");
        assert_eq!(run.definition_digest, job_spec(&job).definition_digest);
        assert_ne!(
            run.definition_digest,
            job_spec(&sql_job()).definition_digest,
            "python and sql envelopes must not collapse"
        );
    }

    #[test]
    fn job_spec_digest_is_stable_and_includes_engine_envelope() {
        let sql = job_spec(&sql_job());
        assert_eq!(sql.contract_version, CONTRACT_VERSION);
        assert!(sql.steps.is_empty());
        assert_eq!(sql.isolated_process.engine, "duckdb_sql");
        assert_eq!(sql.isolated_process.runtime_image, "");
        assert_eq!(sql.isolated_process.inputs[0].alias, "customers");
        assert_eq!(sql.input_dataset_id, "customers");
        assert_eq!(sql.output_dataset_id, "totals.csv");
        let spec_json = serde_json::to_value(&sql).unwrap();
        let mut spec_keys: Vec<&str> = spec_json
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        spec_keys.sort_unstable();
        let mut expected = vec![
            "contract_version",
            "namespace",
            "transform_id",
            "input_dataset_id",
            "output_dataset_id",
            "steps",
            "quality_rule",
            "definition_digest",
            "isolated_process",
        ];
        expected.sort_unstable();
        assert_eq!(spec_keys, expected, "{spec_json}");
        serde_json::from_value::<JobSpec>(spec_json).unwrap();
        assert_eq!(
            sql.definition_digest,
            "sha256:b1939a06666b24d52a139a893c56fe8f354d329ee5d3d4ad32970d9351a2c50a"
        );
        assert_eq!(
            job_spec(&python_job()).definition_digest,
            "sha256:3ce207e35d744ac5e3bb6381cd3ff177635d39efc688b70e3ce33d3ef80d4b86"
        );
        let again = job_spec(&sql_job());
        assert_eq!(sql.definition_digest, again.definition_digest);
        let mut renamed = sql_job();
        renamed.id = "other".into();
        assert_ne!(sql.definition_digest, job_spec(&renamed).definition_digest);
        let mut image = python_job();
        let base = job_spec(&image).definition_digest;
        image.runtime_image = Some("python-lockfile:sha256:bb".into());
        assert_ne!(base, job_spec(&image).definition_digest);
    }

    #[test]
    fn mapped_receipt_carries_write_lineage_rows() {
        let job = sql_job();
        let mut receipt = receipt_for(
            &job,
            vec![("totals.csv".into(), Digest("sha256:out".into()))],
        );
        receipt.lineage = Some(Lineage {
            plan: "select".into(),
            reads: Vec::new(),
            writes: vec![
                LineageWrite {
                    path: "totals.csv".into(),
                    partitions: vec!["dt=2026-10-18".into()],
                    mode: "overwrite".into(),
                    rows: 3,
                    bytes: 12,
                    commit_id: "sha256:commit".into(),
                },
                LineageWrite {
                    path: "sales".into(),
                    partitions: vec!["year=2020".into(), "year=2021".into()],
                    mode: "overwrite".into(),
                    rows: 5,
                    bytes: 20,
                    commit_id: "sha256:multi".into(),
                },
            ],
            column_markings: Vec::new(),
        });
        let run = transform_run(&job, &receipt);
        assert_eq!(run.rows_out, 8);
        assert_eq!(run.output_partitions.len(), 3);
        assert_eq!(run.output_partitions[0].partition_key, "dt=2026-10-18");
        assert_eq!(run.output_partitions[0].digest, "");
        assert_eq!(run.output_partitions[0].rows, 3);
        assert_eq!(run.output_partitions[1].partition_key, "year=2020");
        assert_eq!(run.output_partitions[1].digest, "");
        assert_eq!(run.output_partitions[1].rows, 0);
        assert_eq!(run.output_partitions[2].partition_key, "year=2021");
        assert_eq!(run.output_partitions[2].rows, 0);
    }

    #[test]
    fn unpinned_python_run_records_resolved_runtime_image() {
        let mut job = python_job();
        job.runtime_image = None;
        let mut receipt = receipt_for(&job, vec![("out.csv".into(), Digest("sha256:py".into()))]);
        receipt.runtime_image = Some("python-lockfile:sha256:catalog".into());
        let run = transform_run(&job, &receipt);
        assert_eq!(run.runtime_image, "python-lockfile:sha256:catalog");
        assert_eq!(job_spec(&job).isolated_process.runtime_image, "");
        assert_ne!(
            run.definition_digest,
            job_spec(&python_job()).definition_digest
        );
    }

    #[test]
    fn object_set_input_parses_and_is_omitted_from_dataset_json() {
        let dataset = TransformInput {
            alias: "orders".into(),
            dataset_id: "orders".into(),
            object_set: String::new(),
        };
        let json = serde_json::to_value(&dataset).unwrap();
        assert_eq!(json["alias"], "orders");
        assert_eq!(json["dataset_id"], "orders");
        assert!(json.get("object_set").is_none());
        let object_set = TransformInput {
            alias: "customers".into(),
            dataset_id: String::new(),
            object_set: r#"{"kind":"Customer","namespace":"sales"}"#.into(),
        };
        let from_plane: TransformInput = serde_json::from_str(
            r#"{"alias":"customers","dataset_id":"","object_set":"{\"kind\":\"Customer\",\"namespace\":\"sales\"}"}"#,
        )
        .unwrap();
        assert_eq!(from_plane, object_set);
        assert!(from_plane.is_object_set());
        assert!(from_plane.dataset_id.is_empty());
        assert_eq!(
            serde_json::to_string(&object_set).unwrap(),
            r#"{"alias":"customers","dataset_id":"","object_set":"{\"kind\":\"Customer\",\"namespace\":\"sales\"}"}"#
        );
        let spec: IsolatedProcessSpec = serde_json::from_str(
            r#"{"engine":"duckdb_sql","runtime_image":"","inputs":[{"alias":"orders","dataset_id":"orders"}]}"#,
        )
        .unwrap();
        assert!(spec.requires.is_empty());
        assert!(spec.code.is_empty());
        assert!(!spec.inputs[0].is_object_set());
    }

    #[test]
    fn object_set_definition_digest_matches_plane_envelope_and_detects_tamper() {
        let descriptor = r#"{"kind":"Customer","namespace":"sales"}"#;
        let isolated = IsolatedProcessSpec {
            engine: "duckdb_sql".into(),
            runtime_image: String::new(),
            inputs: vec![
                TransformInput {
                    alias: "orders".into(),
                    dataset_id: "orders".into(),
                    object_set: String::new(),
                },
                TransformInput {
                    alias: "customers".into(),
                    dataset_id: String::new(),
                    object_set: canonical_object_set(descriptor),
                },
            ],
            requires: object_set_requires(true),
            code: String::new(),
        };
        let spec = isolated_job_spec("ops", "join-objects", "totals.csv", isolated.clone());
        assert_eq!(spec.input_dataset_id, "orders");
        assert_eq!(
            serde_json::to_string(&spec.isolated_process.inputs).unwrap(),
            r#"[{"alias":"orders","dataset_id":"orders"},{"alias":"customers","dataset_id":"","object_set":"{\"kind\":\"Customer\",\"namespace\":\"sales\"}"}]"#
        );
        assert_eq!(
            spec.isolated_process.requires[OBJECT_SET_INPUT_LABEL],
            OBJECT_SET_INPUT_LABEL_VALUE
        );
        require_matching_digest(&spec, &spec.definition_digest).unwrap();
        let mut tampered = isolated;
        tampered.inputs[1].object_set =
            canonical_object_set(r#"{"kind":"Other","namespace":"sales"}"#);
        let other = isolated_job_spec("ops", "join-objects", "totals.csv", tampered);
        assert_ne!(spec.definition_digest, other.definition_digest);
        let error = require_matching_digest(&other, &spec.definition_digest).unwrap_err();
        assert!(error.contains("does not match"), "{error}");
        assert_eq!(
            job_spec(&sql_job()).definition_digest,
            "sha256:b1939a06666b24d52a139a893c56fe8f354d329ee5d3d4ad32970d9351a2c50a"
        );
    }

    #[test]
    fn job_spec_object_set_input_uses_empty_dataset_id() {
        let mut job = sql_job();
        job.inputs[1].object_set = r#"{"namespace":"sales","kind":"Customer"}"#.into();
        let spec = job_spec(&job);
        assert!(spec.isolated_process.inputs[1].is_object_set());
        assert!(spec.isolated_process.inputs[1].dataset_id.is_empty());
        assert_eq!(spec.input_dataset_id, "customers");
        assert!(!spec.isolated_process.requires.is_empty());
    }
}
