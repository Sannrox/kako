//! Worker manifest the sandboxed child reads from the scratch directory.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::{Engine, Input, Job, WriteMode};

/// One staged input in the worker manifest.
#[derive(Debug, Clone, PartialEq, Eq, Default, Deserialize)]
pub struct StagedInput {
    pub name: String,
    pub relative: String,
    #[serde(default)]
    pub markings: Vec<String>,
    #[serde(default)]
    pub column_markings: BTreeMap<String, Vec<String>>,
}

/// Manifest the worker child reads as `manifest.json`.
#[derive(Debug, Clone, Deserialize)]
pub struct Manifest {
    #[serde(default)]
    pub engine: Option<String>,
    pub sql: String,
    pub inputs: Vec<StagedInput>,
    #[serde(default)]
    pub outputs: Vec<String>,
    #[serde(default)]
    pub write_mode: Option<String>,
}

impl Manifest {
    /// Parsed output commit mode, defaulting to overwrite.
    pub fn write_mode(&self) -> Result<WriteMode, String> {
        self.write_mode
            .as_deref()
            .unwrap_or("overwrite")
            .parse::<WriteMode>()
    }
}

/// Write the worker manifest the child will read.
pub fn write_manifest(
    scratch: &Path,
    engine: Engine,
    sql: &str,
    inputs: &[(String, PathBuf)],
    outputs: &[String],
) -> Result<(), String> {
    write_manifest_with(scratch, engine, sql, inputs, outputs, WriteMode::Overwrite)
}

/// Write the worker manifest, including the mizuchi output commit mode.
pub fn write_manifest_with(
    scratch: &Path,
    engine: Engine,
    sql: &str,
    inputs: &[(String, PathBuf)],
    outputs: &[String],
    write_mode: WriteMode,
) -> Result<(), String> {
    write_manifest_inputs(scratch, engine, sql, inputs, &[], outputs, write_mode)
}

/// Write the worker manifest, copying marking labels from `job`.
pub fn write_manifest_from_job(
    scratch: &Path,
    job: &Job,
    staged: &[(String, PathBuf)],
) -> Result<(), String> {
    write_manifest_inputs(
        scratch,
        job.engine,
        &job.code,
        staged,
        &job.inputs,
        &job.outputs,
        job.write_mode,
    )
}

fn write_manifest_inputs(
    scratch: &Path,
    engine: Engine,
    sql: &str,
    staged: &[(String, PathBuf)],
    job_inputs: &[Input],
    outputs: &[String],
    write_mode: WriteMode,
) -> Result<(), String> {
    let engine = match engine {
        Engine::DuckDbSql => "duckdb_sql",
        Engine::Mizuchi => "mizuchi",
        Engine::Python => "python",
    };
    let body = serde_json::json!({
        "engine": engine,
        "sql": sql,
        "inputs": staged.iter().map(|(name, relative)| {
            let src = job_inputs.iter().find(|input| &input.name == name);
            serde_json::json!({
                "name": name,
                "relative": relative.display().to_string(),
                "markings": src.map(|input| input.markings.clone()).unwrap_or_default(),
                "column_markings": src.map(|input| input.column_markings.clone()).unwrap_or_default(),
            })
        }).collect::<Vec<_>>(),
        "outputs": outputs,
        "write_mode": write_mode.as_str(),
    });
    fs::write(scratch.join("manifest.json"), body.to_string())
        .map_err(|error| format!("write manifest: {error}"))
}

/// Read `manifest.json` from the scratch directory (or the process cwd if
/// `scratch` is `.` after the worker has already chdir'd).
pub fn read_manifest(scratch: &Path) -> Result<Manifest, String> {
    let path = if scratch.as_os_str() == "." {
        PathBuf::from("manifest.json")
    } else {
        scratch.join("manifest.json")
    };
    let raw = fs::read_to_string(&path).map_err(|error| format!("read manifest: {error}"))?;
    serde_json::from_str(&raw).map_err(|error| format!("parse manifest: {error}"))
}
