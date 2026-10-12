//! Worker manifest the sandboxed child reads from the scratch directory.

use std::fs;
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::{Engine, WriteMode};

/// One staged input in the worker manifest.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct StagedInput {
    pub name: String,
    pub relative: String,
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
    let engine = match engine {
        Engine::DuckDbSql => "duckdb_sql",
        Engine::Mizuchi => "mizuchi",
        Engine::Python => "python",
    };
    let body = serde_json::json!({
        "engine": engine,
        "sql": sql,
        "inputs": inputs.iter().map(|(name, relative)| {
            serde_json::json!({ "name": name, "relative": relative.display().to_string() })
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
