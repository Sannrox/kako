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
    /// Preview or validation row cap. When set, each input is its first this
    /// many rows and direct reads of staged input files are refused.
    #[serde(default)]
    pub sample_rows: Option<u64>,
    /// With `sample_rows`, write [`PREVIEW_FILE`] with each output's rows.
    #[serde(default)]
    pub capture: bool,
}

/// Scratch-relative file a preview run writes: per-output schema and rows.
pub const PREVIEW_FILE: &str = "preview.json";

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
    write_manifest_inputs(
        scratch,
        engine,
        sql,
        inputs,
        &[],
        outputs,
        WriteMode::Overwrite,
        None,
    )
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
        None,
    )
}

/// Write a sampled manifest: inputs capped at `sample_rows`, nothing committed.
/// `capture` asks the engine for [`PREVIEW_FILE`].
pub fn write_preview_manifest(
    scratch: &Path,
    job: &Job,
    staged: &[(String, PathBuf)],
    sample_rows: u64,
    capture: bool,
) -> Result<(), String> {
    write_manifest_inputs(
        scratch,
        job.engine,
        &job.code,
        staged,
        &job.inputs,
        &job.outputs,
        job.write_mode,
        Some((sample_rows, capture)),
    )
}

#[allow(clippy::too_many_arguments)]
fn write_manifest_inputs(
    scratch: &Path,
    engine: Engine,
    sql: &str,
    staged: &[(String, PathBuf)],
    job_inputs: &[Input],
    outputs: &[String],
    write_mode: WriteMode,
    sample: Option<(u64, bool)>,
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
        "sample_rows": sample.map(|(rows, _)| rows),
        "capture": sample.is_some_and(|(_, capture)| capture),
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
