//! Local/CLI job source: one JSON job file, outputs written to a directory.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::receipt;
use crate::run::DEFAULT_WALL_CLOCK;
use crate::{
    Engine, Failure, Input, IsolationTier, Job, JobSource, Output, QuotaKey, Receipt, WriteMode,
};

#[derive(Debug)]
pub struct LocalError(pub String);

impl std::fmt::Display for LocalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for LocalError {}

impl From<String> for LocalError {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<io::Error> for LocalError {
    fn from(value: io::Error) -> Self {
        Self(value.to_string())
    }
}

#[derive(Deserialize)]
struct JobFile {
    id: Option<String>,
    engine: Option<String>,
    sql: Option<String>,
    code: Option<String>,
    inputs: HashMap<String, String>,
    #[serde(default)]
    input_markings: HashMap<String, Vec<String>>,
    #[serde(default)]
    input_column_markings: HashMap<String, HashMap<String, Vec<String>>>,
    outputs: Vec<String>,
    #[serde(default)]
    output_markings: HashMap<String, Vec<String>>,
    declassify: Option<kako_core::DeclassifyDecision>,
    timeout_ms: Option<u64>,
    cpu_time_secs: Option<u64>,
    memory_bytes: Option<u64>,
    tenant_id: Option<String>,
    namespace: Option<String>,
    runtime_image: Option<String>,
    isolation: Option<String>,
    write_mode: Option<String>,
    /// A batch scoring job from a registry descriptor ([`crate::scoring`]).
    scoring: Option<ScoringFile>,
}

/// Paths, relative to the job file, of a scoring job's signed batch
/// descriptor, the registry keys it trusts, and the model artifact.
#[derive(Debug, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ScoringFile {
    descriptor: String,
    trusted_keys: String,
    artifact: String,
}

/// Identity the local source applies when a job file omits tenant/namespace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalIdentity {
    pub tenant_id: String,
    pub namespace: String,
    /// When true, the job file must name both fields; defaults are not applied.
    pub require_job_identity: bool,
}

impl Default for LocalIdentity {
    fn default() -> Self {
        Self {
            tenant_id: "local".into(),
            namespace: "default".into(),
            require_job_identity: false,
        }
    }
}

/// One-shot source used by `kako run`.
pub struct LocalSource {
    job: Option<Job>,
    output_dir: PathBuf,
}

impl LocalSource {
    pub fn from_job_file(job_file: &Path, output_dir: PathBuf) -> Result<Self, LocalError> {
        Self::from_job_file_with(job_file, output_dir, LocalIdentity::default())
    }

    pub fn from_job_file_with(
        job_file: &Path,
        output_dir: PathBuf,
        identity: LocalIdentity,
    ) -> Result<Self, LocalError> {
        let job = load_job_with(job_file, &identity)?;
        Ok(Self {
            job: Some(job),
            output_dir,
        })
    }
}

impl JobSource for LocalSource {
    type Error = LocalError;

    fn next_job(&mut self) -> Result<Option<Job>, Self::Error> {
        Ok(self.job.take())
    }

    fn complete(
        &mut self,
        job: &Job,
        outputs: &[Output],
        receipt: &Receipt,
    ) -> Result<(), Self::Error> {
        let _ = job;
        write_completion(&self.output_dir, outputs, receipt)
    }

    fn fail(&mut self, job: &Job, failure: &Failure) -> Result<(), Self::Error> {
        // Drop a leftover receipt so a prior success cannot be read as this
        // run. Other files in the operator-chosen output directory stay.
        fs::create_dir_all(&self.output_dir)?;
        let body = format!("{failure}\njob: {}\n", job.id);
        let failure_path = self.output_dir.join("failure.txt");
        reject_symlink(&failure_path)?;
        fs::write(&failure_path, body)?;
        match failure {
            Failure::KeyRevoked { dek_id, kek_id } => {
                let receipt = revoked_receipt(job, dek_id, kek_id);
                let json = receipt::to_json(&receipt).map_err(LocalError)?;
                let receipt_path = self.output_dir.join("receipt.json");
                reject_symlink(&receipt_path)?;
                fs::write(&receipt_path, json)?;
            }
            _ => {
                let _ = fs::remove_file(self.output_dir.join("receipt.json"));
            }
        }
        Ok(())
    }

    fn release(&mut self, job: &Job) -> Result<(), Self::Error> {
        if self.job.is_some() {
            return Err(LocalError(
                "cannot hand back a job while another is already queued".into(),
            ));
        }
        self.job = Some(job.clone());
        Ok(())
    }
}

/// Copy outputs into `output_dir`, write `receipt.json`, and drop a stale
/// `failure.txt`. Shared by `kako run` and each replay step.
pub(crate) fn write_completion(
    output_dir: &Path,
    outputs: &[Output],
    receipt: &Receipt,
) -> Result<(), LocalError> {
    fs::create_dir_all(output_dir).map_err(|error| {
        LocalError(format!(
            "create output dir {}: {error}",
            output_dir.display()
        ))
    })?;
    for output in outputs {
        let dest = output_dir.join(&output.name);
        if dest.parent() != Some(output_dir) {
            return Err(LocalError(format!(
                "output `{}` must be a file name in the output directory",
                output.name
            )));
        }
        copy_regular_file(&output.path, &dest, output_dir)?;
    }
    let json = receipt::to_json(receipt).map_err(LocalError)?;
    let receipt_path = output_dir.join("receipt.json");
    reject_symlink(&receipt_path)?;
    fs::write(&receipt_path, json)?;
    let _ = fs::remove_file(output_dir.join("failure.txt"));
    Ok(())
}

fn revoked_receipt(job: &Job, dek_id: &str, kek_id: &str) -> Receipt {
    Receipt {
        job_id: job.id.clone(),
        inputs: Vec::new(),
        code: crate::digest::hash_bytes(job.code.as_bytes()),
        engine_version: String::new(),
        outputs: Vec::new(),
        started_at: std::time::SystemTime::now(),
        duration: std::time::Duration::from_millis(0),
        tenant_id: job.tenant_id.clone(),
        namespace: job.namespace.clone(),
        cpu_usec: None,
        memory_peak_bytes: None,
        lineage: None,
        runtime_image: None,
        isolation: job.isolation,
        credential_scope: None,
        input_markings: Vec::new(),
        output_markings: Vec::new(),
        declassify: None,
        key_ids: vec![dek_id.to_string(), kek_id.to_string()],
        key_state: Some("revoked".into()),
        model: None,
    }
}

fn reject_symlink(path: &Path) -> Result<(), LocalError> {
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => Err(LocalError(format!(
            "refusing to write through symlink {}",
            path.display()
        ))),
        _ => Ok(()),
    }
}

fn copy_regular_file(src: &Path, dest: &Path, output_dir: &Path) -> Result<(), LocalError> {
    reject_symlink(dest)?;
    let tmp_name = dest.file_name().and_then(|n| n.to_str()).unwrap_or("out");
    let tmp = dest.with_file_name(format!(".{tmp_name}.kako-tmp"));
    reject_symlink(&tmp)?;
    fs::copy(src, &tmp).map(|_| ()).map_err(|error| {
        LocalError(format!(
            "copy output {} -> {}: {error}",
            src.display(),
            tmp.display()
        ))
    })?;
    let tmp_canon = match tmp.canonicalize() {
        Ok(path) => path,
        Err(error) => {
            let _ = fs::remove_file(&tmp);
            return Err(LocalError(format!(
                "canonicalize {}: {error}",
                tmp.display()
            )));
        }
    };
    let out_canon = match output_dir.canonicalize() {
        Ok(path) => path,
        Err(error) => {
            let _ = fs::remove_file(&tmp);
            return Err(LocalError(format!(
                "canonicalize {}: {error}",
                output_dir.display()
            )));
        }
    };
    if !tmp_canon.starts_with(&out_canon) {
        let _ = fs::remove_file(&tmp_canon);
        return Err(LocalError(format!(
            "output landed outside {} at {}",
            output_dir.display(),
            tmp_canon.display()
        )));
    }
    fs::rename(&tmp, dest).map_err(|error| {
        let _ = fs::remove_file(&tmp);
        LocalError(format!(
            "rename {} -> {}: {error}",
            tmp.display(),
            dest.display()
        ))
    })
}

fn relative_leaf(path: &str) -> bool {
    let path = Path::new(path);
    !path.as_os_str().is_empty() && path.components().all(|c| matches!(c, Component::Normal(_)))
}

fn relative_file_name(path: &str) -> bool {
    let mut comps = Path::new(path).components();
    matches!(comps.next(), Some(Component::Normal(_))) && comps.next().is_none()
}

/// Parse a job JSON file. Input paths are relative to the file's directory.
pub fn load_job(path: &Path) -> Result<Job, LocalError> {
    load_job_with(path, &LocalIdentity::default())
}

fn load_job_with(path: &Path, identity: &LocalIdentity) -> Result<Job, LocalError> {
    let raw = fs::read_to_string(path)
        .map_err(|error| LocalError(format!("read {}: {error}", path.display())))?;
    let parsed: JobFile = serde_json::from_str(&raw)
        .map_err(|error| LocalError(format!("parse {}: {error}", path.display())))?;
    let (tenant_id, namespace) = resolve_identity(&parsed, identity)?;
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    let default_engine = if parsed.scoring.is_some() {
        "python"
    } else {
        "duckdb_sql"
    };
    let engine = match parsed.engine.as_deref().unwrap_or(default_engine) {
        _ if parsed.scoring.is_some()
            && parsed.engine.as_deref().is_some_and(|e| e != "python") =>
        {
            return Err(LocalError("a scoring job runs on the python engine".into()));
        }
        "duckdb_sql" | "sql" => Engine::DuckDbSql,
        "mizuchi" => Engine::Mizuchi,
        "python" => Engine::Python,
        other => {
            return Err(LocalError(format!(
                "unknown engine `{other}` (want duckdb_sql, mizuchi, or python)"
            )));
        }
    };
    let mut code = match &parsed.scoring {
        Some(_) if parsed.code.is_some() || parsed.sql.is_some() => {
            return Err(LocalError(
                "a scoring job's program comes from its descriptor; drop `code` and `sql`".into(),
            ));
        }
        Some(_) => String::new(),
        None => job_code(&parsed, engine)?,
    };
    let id = parsed.id.unwrap_or_else(|| {
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("job")
            .to_string()
    });
    if parsed.outputs.is_empty() {
        return Err(LocalError("job must name at least one output".into()));
    }
    let mut inputs = Vec::new();
    let mut names = parsed.inputs.keys().cloned().collect::<Vec<_>>();
    names.sort();
    for name in names {
        validate_name(&name)?;
        let rel = &parsed.inputs[&name];
        if !relative_leaf(rel) {
            return Err(LocalError(format!(
                "input `{name}` path must be relative to the job file"
            )));
        }
        let input_path = base.join(rel);
        let markings = kako_core::marking::normalize(
            parsed
                .input_markings
                .get(&name)
                .cloned()
                .unwrap_or_default(),
        );
        let column_markings = parsed
            .input_column_markings
            .get(&name)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(|(column, labels)| (column, kako_core::marking::normalize(labels)))
            .collect();
        inputs.push(Input {
            name,
            path: input_path,
            markings,
            column_markings,
        });
    }
    let mut model = None;
    if let Some(spec) = &parsed.scoring {
        let ([data], [output]) = (inputs.as_slice(), parsed.outputs.as_slice()) else {
            return Err(LocalError(
                "a scoring job scores one input into one output".into(),
            ));
        };
        if data.name == crate::scoring::MODEL_INPUT {
            return Err(LocalError(format!(
                "input name `{}` is the scoring job's model",
                crate::scoring::MODEL_INPUT
            )));
        }
        let path = |what: &str, rel: &str| {
            if relative_leaf(rel) {
                Ok(base.join(rel))
            } else {
                Err(LocalError(format!(
                    "scoring {what} path must be relative to the job file"
                )))
            }
        };
        // The runner passes a staged `.onnx` input as a path, not a frame.
        if !spec.artifact.ends_with(".onnx") {
            return Err(LocalError(
                "a scoring job's artifact must be an `.onnx` file".into(),
            ));
        }
        let artifact = path("artifact", &spec.artifact)?;
        let scoring = crate::scoring::prepare(
            &path("descriptor", &spec.descriptor)?,
            &path("trusted_keys", &spec.trusted_keys)?,
            &artifact,
            &data.name,
            output,
        )
        .map_err(LocalError)?;
        code = scoring.code;
        // Labels declared for the model follow it like any input's.
        inputs.push(Input {
            name: crate::scoring::MODEL_INPUT.into(),
            path: artifact,
            markings: kako_core::marking::normalize(
                parsed
                    .input_markings
                    .get(crate::scoring::MODEL_INPUT)
                    .cloned()
                    .unwrap_or_default(),
            ),
            column_markings: Default::default(),
        });
        if parsed
            .input_column_markings
            .contains_key(crate::scoring::MODEL_INPUT)
        {
            return Err(LocalError(
                "the scoring model has no columns to mark".into(),
            ));
        }
        model = Some(scoring.model);
    }
    let mut seen_outputs = HashSet::new();
    for output in &parsed.outputs {
        if !relative_file_name(output) {
            return Err(LocalError(format!(
                "output `{output}` must be a relative file name"
            )));
        }
        if output == "receipt.json" || output == "failure.txt" {
            return Err(LocalError(format!(
                "output `{output}` collides with the local source report file"
            )));
        }
        if !seen_outputs.insert(output) {
            return Err(LocalError(format!("duplicate output `{output}`")));
        }
    }
    let wall_clock = parsed
        .timeout_ms
        .map(Duration::from_millis)
        .unwrap_or(DEFAULT_WALL_CLOCK);
    if wall_clock.is_zero() {
        return Err(LocalError("timeout_ms must be greater than zero".into()));
    }
    let cpu_time = match parsed.cpu_time_secs {
        Some(0) => return Err(LocalError("cpu_time_secs must be greater than zero".into())),
        Some(secs) => Some(Duration::from_secs(secs)),
        None => None,
    };
    if parsed.memory_bytes == Some(0) {
        return Err(LocalError("memory_bytes must be greater than zero".into()));
    }
    let runtime_image = match parsed.runtime_image {
        Some(_) if engine != Engine::Python => {
            return Err(LocalError(
                "runtime_image is only valid for python jobs".into(),
            ));
        }
        Some(id) => Some(kako_engine_python::validate_runtime_image_id(&id)?),
        None => None,
    };
    let isolation = match parsed.isolation.as_deref() {
        None | Some("") => IsolationTier::LinuxNative,
        Some(value) => value.parse().map_err(LocalError)?,
    };
    let write_mode = match parsed.write_mode.as_deref() {
        None | Some("") => WriteMode::Overwrite,
        Some(value) => value.parse().map_err(LocalError)?,
    };
    if write_mode != WriteMode::Overwrite && engine != Engine::Mizuchi {
        return Err(LocalError(
            "write_mode is only valid for mizuchi jobs".into(),
        ));
    }
    let mut output_markings = BTreeMap::new();
    for (name, labels) in parsed.output_markings {
        if !parsed.outputs.iter().any(|output| output == &name) {
            return Err(LocalError(format!(
                "output_markings names unknown output `{name}`"
            )));
        }
        output_markings.insert(name, kako_core::marking::normalize(labels));
    }
    for name in parsed.input_markings.keys() {
        let scoring_model = parsed.scoring.is_some() && name == crate::scoring::MODEL_INPUT;
        if !parsed.inputs.contains_key(name) && !scoring_model {
            return Err(LocalError(format!(
                "input_markings names unknown input `{name}`"
            )));
        }
    }
    for name in parsed.input_column_markings.keys() {
        if !parsed.inputs.contains_key(name) {
            return Err(LocalError(format!(
                "input_column_markings names unknown input `{name}`"
            )));
        }
    }
    let declassify = match parsed.declassify {
        Some(decision) if decision.id.is_empty() || decision.from.is_empty() => {
            return Err(LocalError(
                "declassify requires a nonempty id and from list".into(),
            ));
        }
        Some(mut decision) => {
            decision.from = kako_core::marking::normalize(decision.from);
            Some(decision)
        }
        None => None,
    };
    Ok(Job {
        id,
        engine,
        code,
        inputs,
        outputs: parsed.outputs,
        write_mode,
        wall_clock,
        cpu_time,
        memory_bytes: parsed.memory_bytes,
        tenant_id,
        namespace,
        runtime_image,
        isolation,
        output_markings,
        declassify,
        encryption: None,
        model,
    })
}

/// Fill tenant/namespace from the job file, or from configured defaults.
/// Multi-tenant sources (`require_job_identity`) refuse a missing field.
fn resolve_identity(
    parsed: &JobFile,
    identity: &LocalIdentity,
) -> Result<(String, String), LocalError> {
    let file_tenant = parsed
        .tenant_id
        .as_deref()
        .filter(|value| !value.is_empty());
    let file_namespace = parsed
        .namespace
        .as_deref()
        .filter(|value| !value.is_empty());
    if identity.require_job_identity {
        let tenant_id = file_tenant.ok_or_else(|| LocalError("tenant_id is required".into()))?;
        let namespace = file_namespace.ok_or_else(|| LocalError("namespace is required".into()))?;
        QuotaKey::new(tenant_id, namespace).map_err(LocalError)?;
        return Ok((tenant_id.to_string(), namespace.to_string()));
    }
    let tenant_id = file_tenant.unwrap_or(identity.tenant_id.as_str());
    let namespace = file_namespace.unwrap_or(identity.namespace.as_str());
    QuotaKey::new(tenant_id, namespace).map_err(LocalError)?;
    Ok((tenant_id.to_string(), namespace.to_string()))
}

fn job_code(parsed: &JobFile, engine: Engine) -> Result<String, LocalError> {
    match engine {
        Engine::Python => parsed
            .code
            .clone()
            .or_else(|| parsed.sql.clone())
            .ok_or_else(|| LocalError("python job must set `code`".into())),
        Engine::DuckDbSql | Engine::Mizuchi => parsed
            .sql
            .clone()
            .ok_or_else(|| LocalError("sql job must set `sql`".into())),
    }
}

fn validate_name(name: &str) -> Result<(), LocalError> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(LocalError(format!(
            "input name `{name}` must be alphanumeric or underscore"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Failure, JobSource};

    #[test]
    fn load_job_resolves_paths_relative_to_the_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("in.csv"), "a\n1\n").unwrap();
        let job_path = dir.path().join("job.json");
        fs::write(
            &job_path,
            r#"{
              "sql": "COPY (SELECT * FROM in) TO 'outputs/out.csv' (HEADER);",
              "inputs": {"in": "in.csv"},
              "outputs": ["out.csv"],
              "timeout_ms": 5000
            }"#,
        )
        .unwrap();
        let job = load_job(&job_path).unwrap();
        assert_eq!(job.id, "job");
        assert_eq!(job.engine, Engine::DuckDbSql);
        assert_eq!(job.inputs[0].name, "in");
        assert_eq!(job.inputs[0].path, dir.path().join("in.csv"));
        assert_eq!(job.wall_clock, Duration::from_millis(5000));
        assert_eq!(job.tenant_id, "local");
        assert_eq!(job.namespace, "default");
        assert_eq!(job.cpu_time, None);
        assert_eq!(job.memory_bytes, None);
        let key = QuotaKey::from_job(&job).unwrap();
        assert_eq!(key.tenant_id, "local");
        assert_eq!(key.namespace, "default");
    }

    #[test]
    fn load_job_reads_python_code() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("in.csv"), "a\n1\n").unwrap();
        let job_path = dir.path().join("job.json");
        fs::write(
            &job_path,
            r#"{
              "engine": "python",
              "code": "def transform(inputs):\n    return {\"out.csv\": inputs[\"in\"]}\n",
              "inputs": {"in": "in.csv"},
              "outputs": ["out.csv"]
            }"#,
        )
        .unwrap();
        let job = load_job(&job_path).unwrap();
        assert_eq!(job.engine, Engine::Python);
        assert!(job.code.contains("def transform"), "{}", job.code);
        assert_eq!(job.runtime_image, None);
    }

    #[test]
    fn load_job_reads_python_runtime_image_pin() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("in.csv"), "a\n1\n").unwrap();
        let job_path = dir.path().join("job.json");
        let pin = format!("python-lockfile:sha256:{}", "ab".repeat(32));
        let body = serde_json::json!({
            "engine": "python",
            "code": "def transform(inputs):\n    return {\"out.csv\": inputs[\"in\"]}\n",
            "inputs": {"in": "in.csv"},
            "outputs": ["out.csv"],
            "runtime_image": pin,
        });
        fs::write(&job_path, body.to_string()).unwrap();
        let job = load_job(&job_path).unwrap();
        assert_eq!(job.runtime_image.as_deref(), Some(pin.as_str()));
    }

    #[test]
    fn load_job_rejects_runtime_image_on_sql() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("in.csv"), "a\n1\n").unwrap();
        let job_path = dir.path().join("job.json");
        let pin = format!("python-lockfile:sha256:{}", "ab".repeat(32));
        let body = serde_json::json!({
            "sql": "SELECT 1",
            "inputs": {"in": "in.csv"},
            "outputs": ["out.csv"],
            "runtime_image": pin,
        });
        fs::write(&job_path, body.to_string()).unwrap();
        let err = load_job(&job_path).unwrap_err();
        assert!(err.0.contains("runtime_image"), "{err}");
    }

    #[test]
    fn load_job_rejects_python_without_code() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("in.csv"), "a\n1\n").unwrap();
        let job_path = dir.path().join("job.json");
        fs::write(
            &job_path,
            r#"{
              "engine": "python",
              "inputs": {"in": "in.csv"},
              "outputs": ["out.csv"]
            }"#,
        )
        .unwrap();
        let err = load_job(&job_path).unwrap_err();
        assert!(err.0.contains("code"), "{err}");
    }

    #[test]
    fn load_job_selects_mizuchi_when_named() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("in.csv"), "a\n1\n").unwrap();
        let job_path = dir.path().join("job.json");
        fs::write(
            &job_path,
            r#"{
              "engine": "mizuchi",
              "sql": "COPY (SELECT * FROM in) TO 'outputs/out.csv' (HEADER);",
              "inputs": {"in": "in.csv"},
              "outputs": ["out.csv"]
            }"#,
        )
        .unwrap();
        let job = load_job(&job_path).unwrap();
        assert_eq!(job.engine, Engine::Mizuchi);
    }

    #[test]
    fn load_job_reads_markings_and_declassify() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("in.csv"), "id,ssn\n1,x\n").unwrap();
        let job_path = dir.path().join("job.json");
        fs::write(
            &job_path,
            r#"{
              "sql": "SELECT id FROM in",
              "inputs": {"in": "in.csv"},
              "outputs": ["out.csv"],
              "input_markings": {"in": ["restricted"]},
              "input_column_markings": {"in": {"ssn": ["pii"]}},
              "output_markings": {"out.csv": ["restricted"]},
              "declassify": {"id": "dec-1", "from": ["pii"], "reason": "drop identifier"}
            }"#,
        )
        .unwrap();
        let job = load_job(&job_path).unwrap();
        assert_eq!(job.inputs[0].markings, ["restricted"]);
        assert_eq!(
            job.inputs[0].column_markings.get("ssn").unwrap(),
            &["pii".to_string()]
        );
        assert_eq!(
            job.output_markings.get("out.csv").unwrap(),
            &["restricted".to_string()]
        );
        let dec = job.declassify.expect("declassify");
        assert_eq!(dec.id, "dec-1");
        assert_eq!(dec.from, ["pii"]);
    }

    #[test]
    fn load_job_reads_per_run_limits() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("in.csv"), "a\n1\n").unwrap();
        let job_path = dir.path().join("job.json");
        fs::write(
            &job_path,
            r#"{
              "sql": "SELECT 1",
              "inputs": {"in": "in.csv"},
              "outputs": ["out.csv"],
              "cpu_time_secs": 2,
              "memory_bytes": 1048576
            }"#,
        )
        .unwrap();
        let job = load_job(&job_path).unwrap();
        assert_eq!(job.cpu_time, Some(Duration::from_secs(2)));
        assert_eq!(job.memory_bytes, Some(1_048_576));
    }

    #[test]
    fn load_job_rejects_receipt_output_name() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("in.csv"), "a\n1\n").unwrap();
        let job_path = dir.path().join("job.json");
        fs::write(
            &job_path,
            r#"{
              "sql": "SELECT 1",
              "inputs": {"in": "in.csv"},
              "outputs": ["receipt.json"]
            }"#,
        )
        .unwrap();
        let err = load_job(&job_path).unwrap_err();
        assert!(err.0.contains("receipt.json"), "{err}");
    }

    #[test]
    fn load_job_rejects_parent_input_paths() {
        let dir = tempfile::tempdir().unwrap();
        let job_path = dir.path().join("job.json");
        fs::write(
            &job_path,
            r#"{
              "sql": "SELECT 1",
              "inputs": {"in": "../secret.csv"},
              "outputs": ["out.csv"]
            }"#,
        )
        .unwrap();
        let err = load_job(&job_path).unwrap_err();
        assert!(err.0.contains("relative"), "{err}");
    }

    #[test]
    fn load_job_rejects_duplicate_outputs() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("in.csv"), "a\n1\n").unwrap();
        let job_path = dir.path().join("job.json");
        fs::write(
            &job_path,
            r#"{
              "sql": "SELECT 1",
              "inputs": {"in": "in.csv"},
              "outputs": ["out.csv", "out.csv"]
            }"#,
        )
        .unwrap();
        let err = load_job(&job_path).unwrap_err();
        assert!(err.0.contains("duplicate"), "{err}");
    }

    #[test]
    fn load_job_rejects_nested_output_paths() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("in.csv"), "a\n1\n").unwrap();
        let job_path = dir.path().join("job.json");
        fs::write(
            &job_path,
            r#"{
              "sql": "SELECT 1",
              "inputs": {"in": "in.csv"},
              "outputs": ["nested/out.csv"]
            }"#,
        )
        .unwrap();
        let err = load_job(&job_path).unwrap_err();
        assert!(err.0.contains("file name"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn complete_does_not_write_through_a_parent_symlink() {
        use crate::{Digest, Engine, Output, Receipt};
        use std::time::{Duration, SystemTime};

        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("out");
        fs::create_dir_all(&out).unwrap();
        let outside = dir.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, out.join("nested")).unwrap();
        let produced = dir.path().join("produced.csv");
        fs::write(&produced, "data\n").unwrap();
        let mut source = LocalSource {
            job: None,
            output_dir: out,
        };
        let job = crate::Job {
            id: "j".into(),
            engine: Engine::DuckDbSql,
            code: "SELECT 1".into(),
            inputs: vec![],
            outputs: vec!["nested/out.csv".into()],
            wall_clock: Duration::from_secs(1),
            cpu_time: None,
            memory_bytes: None,
            tenant_id: "local".into(),
            namespace: "default".into(),
            runtime_image: None,
            write_mode: WriteMode::Overwrite,
            isolation: IsolationTier::LinuxNative,
            output_markings: Default::default(),
            declassify: None,
            encryption: None,
            model: None,
        };
        let outputs = vec![Output {
            name: "nested/out.csv".into(),
            path: produced,
            digest: Digest("sha256:x".into()),
        }];
        let receipt = Receipt {
            job_id: "j".into(),
            inputs: vec![],
            code: Digest("sha256:y".into()),
            engine_version: "duckdb test".into(),
            outputs: vec![],
            started_at: SystemTime::UNIX_EPOCH,
            duration: Duration::from_millis(1),
            tenant_id: "local".into(),
            namespace: "default".into(),
            cpu_usec: None,
            memory_peak_bytes: None,
            lineage: None,
            runtime_image: None,
            isolation: IsolationTier::LinuxNative,
            credential_scope: None,
            input_markings: Vec::new(),
            output_markings: Vec::new(),
            declassify: None,
            key_ids: Vec::new(),
            key_state: None,
            model: None,
        };
        let err = source.complete(&job, &outputs, &receipt).unwrap_err();
        assert!(err.0.contains("file name"), "{err}");
        assert!(!outside.join("out.csv").exists());
        assert!(!outside.join(".out.csv.kako-tmp").exists());
    }

    #[test]
    fn load_job_reads_identity_from_the_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("in.csv"), "a\n1\n").unwrap();
        let job_path = dir.path().join("job.json");
        fs::write(
            &job_path,
            r#"{
              "sql": "SELECT 1",
              "inputs": {"in": "in.csv"},
              "outputs": ["out.csv"],
              "tenant_id": "t",
              "namespace": "ns"
            }"#,
        )
        .unwrap();
        let job = load_job(&job_path).unwrap();
        assert_eq!(job.tenant_id, "t");
        assert_eq!(job.namespace, "ns");
        assert_eq!(job.isolation, IsolationTier::LinuxNative);
    }

    #[test]
    fn load_job_reads_isolation_floor() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("in.csv"), "a\n1\n").unwrap();
        let job_path = dir.path().join("job.json");
        fs::write(
            &job_path,
            r#"{
              "sql": "SELECT 1",
              "inputs": {"in": "in.csv"},
              "outputs": ["out.csv"],
              "isolation": "userspace"
            }"#,
        )
        .unwrap();
        let job = load_job(&job_path).unwrap();
        assert_eq!(job.isolation, IsolationTier::Userspace);
    }

    #[test]
    fn load_job_reads_mizuchi_write_mode() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("in.csv"), "a\n1\n").unwrap();
        let job_path = dir.path().join("job.json");
        fs::write(
            &job_path,
            r#"{
              "engine": "mizuchi",
              "sql": "COPY (SELECT * FROM in) TO 'outputs/out.csv' (HEADER, DELIMITER ',');",
              "inputs": {"in": "in.csv"},
              "outputs": ["out.csv"],
              "write_mode": "append"
            }"#,
        )
        .unwrap();
        let job = load_job(&job_path).unwrap();
        assert_eq!(job.write_mode, WriteMode::Append);
    }

    #[test]
    fn load_job_defaults_write_mode_to_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("in.csv"), "a\n1\n").unwrap();
        let job_path = dir.path().join("job.json");
        fs::write(
            &job_path,
            r#"{
              "sql": "SELECT 1",
              "inputs": {"in": "in.csv"},
              "outputs": ["out.csv"]
            }"#,
        )
        .unwrap();
        let job = load_job(&job_path).unwrap();
        assert_eq!(job.write_mode, WriteMode::Overwrite);
    }

    #[test]
    fn load_job_rejects_write_mode_on_duckdb() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("in.csv"), "a\n1\n").unwrap();
        let job_path = dir.path().join("job.json");
        fs::write(
            &job_path,
            r#"{
              "sql": "SELECT 1",
              "inputs": {"in": "in.csv"},
              "outputs": ["out.csv"],
              "write_mode": "append"
            }"#,
        )
        .unwrap();
        let err = load_job(&job_path).unwrap_err();
        assert!(err.0.contains("write_mode"), "{err}");
        assert!(err.0.contains("mizuchi"), "{err}");
    }

    #[test]
    fn load_job_rejects_unknown_write_mode() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("in.csv"), "a\n1\n").unwrap();
        let job_path = dir.path().join("job.json");
        fs::write(
            &job_path,
            r#"{
              "engine": "mizuchi",
              "sql": "SELECT 1",
              "inputs": {"in": "in.csv"},
              "outputs": ["out.csv"],
              "write_mode": "merge"
            }"#,
        )
        .unwrap();
        let err = load_job(&job_path).unwrap_err();
        assert!(err.0.contains("write_mode"), "{err}");
        assert!(err.0.contains("merge"), "{err}");
    }

    #[test]
    fn load_job_rejects_unknown_isolation() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("in.csv"), "a\n1\n").unwrap();
        let job_path = dir.path().join("job.json");
        fs::write(
            &job_path,
            r#"{
              "sql": "SELECT 1",
              "inputs": {"in": "in.csv"},
              "outputs": ["out.csv"],
              "isolation": "rlimit"
            }"#,
        )
        .unwrap();
        let err = load_job(&job_path).unwrap_err();
        assert!(err.0.contains("isolation"), "{err}");
        assert!(err.0.contains("rlimit"), "{err}");
    }

    #[test]
    fn multi_tenant_source_refuses_missing_tenant() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("in.csv"), "a\n1\n").unwrap();
        let job_path = dir.path().join("job.json");
        fs::write(
            &job_path,
            r#"{
              "sql": "SELECT 1",
              "inputs": {"in": "in.csv"},
              "outputs": ["out.csv"]
            }"#,
        )
        .unwrap();
        let identity = LocalIdentity {
            tenant_id: "unused".into(),
            namespace: "unused".into(),
            require_job_identity: true,
        };
        let err = match LocalSource::from_job_file_with(&job_path, dir.path().join("out"), identity)
        {
            Err(error) => error,
            Ok(_) => panic!("multi-tenant source accepted a job without tenant_id"),
        };
        assert!(err.0.contains("tenant_id"), "{err}");
    }

    #[test]
    fn complete_receipt_echoes_job_identity() {
        use crate::{Digest, Output, Receipt};
        use std::time::{Duration, SystemTime};

        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("in.csv"), "a\n1\n").unwrap();
        let job_path = dir.path().join("job.json");
        fs::write(
            &job_path,
            r#"{
              "sql": "SELECT 1",
              "inputs": {"in": "in.csv"},
              "outputs": ["out.csv"],
              "tenant_id": "t",
              "namespace": "ns"
            }"#,
        )
        .unwrap();
        let out = dir.path().join("out");
        fs::create_dir_all(&out).unwrap();
        let produced = dir.path().join("produced.csv");
        fs::write(&produced, "data\n").unwrap();
        let mut source = LocalSource::from_job_file(&job_path, out.clone()).unwrap();
        let job = source.next_job().unwrap().unwrap();
        assert_eq!(job.tenant_id, "t");
        assert_eq!(job.namespace, "ns");
        let outputs = vec![Output {
            name: "out.csv".into(),
            path: produced,
            digest: Digest("sha256:x".into()),
        }];
        let receipt = Receipt {
            job_id: job.id.clone(),
            inputs: vec![],
            code: Digest("sha256:y".into()),
            engine_version: "duckdb test".into(),
            outputs: vec![("out.csv".into(), Digest("sha256:x".into()))],
            started_at: SystemTime::UNIX_EPOCH,
            duration: Duration::from_millis(1),
            tenant_id: job.tenant_id.clone(),
            namespace: job.namespace.clone(),
            cpu_usec: None,
            memory_peak_bytes: None,
            lineage: None,
            runtime_image: None,
            isolation: IsolationTier::LinuxNative,
            credential_scope: None,
            input_markings: Vec::new(),
            output_markings: Vec::new(),
            declassify: None,
            key_ids: Vec::new(),
            key_state: None,
            model: None,
        };
        source.complete(&job, &outputs, &receipt).unwrap();
        let json = fs::read_to_string(out.join("receipt.json")).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(v["tenant_id"], job.tenant_id);
        assert_eq!(v["namespace"], job.namespace);
    }

    #[test]
    fn fail_replaces_a_stale_receipt() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("in.csv"), "a\n1\n").unwrap();
        let job_path = dir.path().join("job.json");
        fs::write(
            &job_path,
            r#"{
              "sql": "SELECT 1",
              "inputs": {"in": "in.csv"},
              "outputs": ["out.csv"]
            }"#,
        )
        .unwrap();
        let out = dir.path().join("out");
        fs::create_dir_all(&out).unwrap();
        fs::write(out.join("receipt.json"), "{}\n").unwrap();
        let mut source = LocalSource::from_job_file(&job_path, out.clone()).unwrap();
        let job = source.next_job().unwrap().unwrap();
        source.fail(&job, &Failure::Job("nope".into())).unwrap();
        assert!(!out.join("receipt.json").exists());
        assert!(out.join("failure.txt").exists());
    }

    #[test]
    fn release_requeues_without_writing_outputs() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("in.csv"), "a\n1\n").unwrap();
        let job_path = dir.path().join("job.json");
        fs::write(
            &job_path,
            r#"{
              "sql": "SELECT 1",
              "inputs": {"in": "in.csv"},
              "outputs": ["out.csv"]
            }"#,
        )
        .unwrap();
        let out = dir.path().join("out");
        let mut source = LocalSource::from_job_file(&job_path, out.clone()).unwrap();
        let job = source.next_job().unwrap().unwrap();
        source.release(&job).unwrap();
        assert!(!out.join("receipt.json").exists());
        assert!(!out.join("failure.txt").exists());
        let again = source.next_job().unwrap().unwrap();
        assert_eq!(again.id, job.id);
        assert!(source.next_job().unwrap().is_none());
    }
}
