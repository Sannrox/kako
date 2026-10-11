//! Local/CLI job source: one JSON job file, outputs written to a directory.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

use crate::receipt;
use crate::run::DEFAULT_WALL_CLOCK;
use crate::{Engine, Failure, Input, Job, JobSource, Output, Receipt};

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
    sql: String,
    inputs: HashMap<String, String>,
    outputs: Vec<String>,
    timeout_ms: Option<u64>,
}

/// One-shot source used by `kako run`.
pub struct LocalSource {
    job: Option<Job>,
    output_dir: PathBuf,
}

impl LocalSource {
    pub fn from_job_file(job_file: &Path, output_dir: PathBuf) -> Result<Self, LocalError> {
        let job = load_job(job_file)?;
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
        fs::create_dir_all(&self.output_dir).map_err(|error| {
            LocalError(format!(
                "create output dir {}: {error}",
                self.output_dir.display()
            ))
        })?;
        for output in outputs {
            let dest = self.output_dir.join(&output.name);
            if dest.parent() != Some(self.output_dir.as_path()) {
                return Err(LocalError(format!(
                    "output `{}` must be a file name in the output directory",
                    output.name
                )));
            }
            copy_regular_file(&output.path, &dest, &self.output_dir)?;
        }
        let json = receipt::to_json(receipt).map_err(LocalError)?;
        let receipt_path = self.output_dir.join("receipt.json");
        reject_symlink(&receipt_path)?;
        fs::write(&receipt_path, json)?;
        let _ = fs::remove_file(self.output_dir.join("failure.txt"));
        let _ = job;
        Ok(())
    }

    fn fail(&mut self, job: &Job, failure: &Failure) -> Result<(), Self::Error> {
        // Drop a leftover receipt so a prior success cannot be read as this
        // run. Other files in the operator-chosen output directory stay.
        fs::create_dir_all(&self.output_dir)?;
        let body = format!("{failure}\njob: {}\n", job.id);
        let failure_path = self.output_dir.join("failure.txt");
        reject_symlink(&failure_path)?;
        fs::write(&failure_path, body)?;
        let _ = fs::remove_file(self.output_dir.join("receipt.json"));
        Ok(())
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
    let raw = fs::read_to_string(path)
        .map_err(|error| LocalError(format!("read {}: {error}", path.display())))?;
    let parsed: JobFile = serde_json::from_str(&raw)
        .map_err(|error| LocalError(format!("parse {}: {error}", path.display())))?;
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    let id = parsed.id.unwrap_or_else(|| {
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("job")
            .to_string()
    });
    let engine = match parsed.engine.as_deref().unwrap_or("duckdb_sql") {
        "duckdb_sql" | "sql" => Engine::DuckDbSql,
        "mizuchi" => Engine::Mizuchi,
        "python" => Engine::Python,
        other => {
            return Err(LocalError(format!(
                "unknown engine `{other}` (want duckdb_sql, mizuchi, or python)"
            )));
        }
    };
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
        inputs.push(Input {
            name,
            path: input_path,
        });
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
    Ok(Job {
        id,
        engine,
        code: parsed.sql,
        inputs,
        outputs: parsed.outputs,
        wall_clock,
    })
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
        };
        let err = source.complete(&job, &outputs, &receipt).unwrap_err();
        assert!(err.0.contains("file name"), "{err}");
        assert!(!outside.join("out.csv").exists());
        assert!(!outside.join(".out.csv.kako-tmp").exists());
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
}
