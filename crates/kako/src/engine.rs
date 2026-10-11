//! In-process DuckDB execution used by the sandboxed worker child.

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use duckdb::{Config, Connection};
use serde::Deserialize;

static CWD_LOCK: Mutex<()> = Mutex::new(());

#[derive(Deserialize)]
struct Manifest {
    sql: String,
    inputs: Vec<StagedInput>,
}

#[derive(Deserialize)]
struct StagedInput {
    name: String,
    relative: String,
}

/// Write the worker manifest the child will read.
pub fn write_manifest(
    scratch: &Path,
    sql: &str,
    inputs: &[(String, PathBuf)],
) -> Result<(), String> {
    let body = serde_json::json!({
        "sql": sql,
        "inputs": inputs.iter().map(|(name, relative)| {
            serde_json::json!({ "name": name, "relative": relative.display().to_string() })
        }).collect::<Vec<_>>(),
    });
    fs::write(scratch.join("manifest.json"), body.to_string())
        .map_err(|error| format!("write manifest: {error}"))
}

/// Run the staged SQL inside this process. Prints nothing; returns engine version.
///
/// User SQL uses paths relative to the scratch directory (`inputs/`, `outputs/`),
/// so this function runs with that directory as the process cwd. A lock keeps
/// in-process tests from racing on cwd.
pub fn execute_in_scratch(scratch: &Path) -> Result<String, String> {
    let _cwd = CWD_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    let previous = std::env::current_dir().ok();
    std::env::set_current_dir(scratch)
        .map_err(|error| format!("chdir {}: {error}", scratch.display()))?;
    let result = execute_in_current_dir(scratch);
    if let Some(previous) = previous {
        let _ = std::env::set_current_dir(previous);
    }
    result
}

fn engine_step(step: &str) {
    let mut err = io::stderr();
    let _ = writeln!(err, "kako-engine: {step}");
    let _ = err.flush();
}

fn execute_in_current_dir(scratch: &Path) -> Result<String, String> {
    let raw =
        fs::read_to_string("manifest.json").map_err(|error| format!("read manifest: {error}"))?;
    let manifest: Manifest =
        serde_json::from_str(&raw).map_err(|error| format!("parse manifest: {error}"))?;
    let scratch_abs = scratch
        .canonicalize()
        .map_err(|error| format!("canonicalize scratch: {error}"))?;
    let tmp = scratch_abs.join("tmp");
    let db_path = scratch_abs.join("engine.duckdb");
    let tmp_s = tmp.to_string_lossy().into_owned();
    let home_s = scratch_abs.to_string_lossy().into_owned();
    let config = Config::default()
        .threads(1)
        .map_err(|error| format!("duckdb config threads: {error}"))?
        .enable_autoload_extension(false)
        .map_err(|error| format!("duckdb config autoload: {error}"))?
        .max_memory("256MB")
        .map_err(|error| format!("duckdb config memory: {error}"))?
        .with("temp_directory", &tmp_s)
        .map_err(|error| format!("duckdb config temp: {error}"))?
        .with("home_directory", &home_s)
        .map_err(|error| format!("duckdb config home: {error}"))?
        .with("extension_directory", &tmp_s)
        .map_err(|error| format!("duckdb config extensions: {error}"))?
        .with("enable_external_file_cache", "false")
        .map_err(|error| format!("duckdb config file cache: {error}"))?
        .with("allocator_background_threads", "false")
        .map_err(|error| format!("duckdb config allocator: {error}"))?;
    engine_step("open");
    let conn = Connection::open_with_flags(&db_path, config)
        .map_err(|error| format!("open duckdb: {error}"))?;
    engine_step("settings");
    // Open with external access so construction can read host identity files
    // the kernel sandbox grants. Then allow only the scratch tree and lock
    // the configuration so user SQL cannot follow those grants.
    let scratch_sql = sql_string(&home_s);
    conn.execute_batch(&format!(
        "SET enable_progress_bar=false;\n\
         SET autoload_known_extensions=false;\n\
         SET autoinstall_known_extensions=false;\n\
         SET enable_external_file_cache=false;\n\
         SET allocator_background_threads=false;\n\
         SET allowed_directories = ['{scratch_sql}', 'inputs', 'outputs', 'tmp'];\n\
         SET enable_external_access = false;\n\
         SET lock_configuration = true;"
    ))
    .map_err(|error| format!("duckdb settings: {error}"))?;
    engine_step("views");
    for input in &manifest.inputs {
        let view = quote_ident(&input.name);
        let abs = scratch_abs.join(&input.relative);
        let path = sql_string(&abs.to_string_lossy());
        let from = scan_sql(&path);
        let sql = format!("CREATE OR REPLACE VIEW {view} AS SELECT * FROM {from}");
        conn.execute_batch(&sql)
            .map_err(|error| format!("create view `{}`: {error}", input.name))?;
    }
    engine_step("sql");
    conn.execute_batch(&manifest.sql)
        .map_err(|error| format!("sql: {error}"))?;
    engine_step("version");
    let version: String = conn
        .query_row("SELECT version()", [], |row| row.get(0))
        .map_err(|error| format!("duckdb version: {error}"))?;
    let version = version.trim().trim_start_matches('v');
    Ok(format!("duckdb {version}"))
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

fn sql_string(value: &str) -> String {
    value.replace('\'', "''")
}

fn scan_sql(relative: &str) -> String {
    let lower = relative.to_ascii_lowercase();
    if lower.ends_with(".parquet") {
        format!("read_parquet('{relative}')")
    } else if lower.ends_with(".tsv") {
        format!("read_csv_auto('{relative}', header=true, sep='\t')")
    } else {
        format!("read_csv_auto('{relative}', header=true)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Input;
    use crate::stage::Scratch;
    use std::fs;

    #[test]
    fn copy_constant_row_writes_output() {
        let parent = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        let input = src.path().join("in.csv");
        fs::write(&input, "x\n1\n").unwrap();
        let scratch = Scratch::create(parent.path(), "probe").unwrap();
        let staged = scratch
            .stage_inputs(&[Input {
                name: "in".into(),
                path: input,
            }])
            .unwrap();
        let sql = "COPY (SELECT 1 AS x) TO 'outputs/one.csv' (HEADER, DELIMITER ',');";
        write_manifest(scratch.path(), sql, &staged).unwrap();
        let version = execute_in_scratch(scratch.path()).unwrap();
        assert!(version.starts_with("duckdb "), "{version}");
        let csv = fs::read_to_string(scratch.outputs_dir().join("one.csv")).unwrap();
        assert!(csv.contains('x'), "{csv}");
        assert!(csv.contains('1'), "{csv}");
    }

    #[test]
    fn join_and_aggregate_writes_output() {
        let parent = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        let customers = src.path().join("customers.csv");
        let orders = src.path().join("orders.csv");
        fs::write(&customers, "id,name\n1,a\n2,b\n").unwrap();
        fs::write(&orders, "customer_id,amount\n1,10\n1,15\n2,7\n").unwrap();
        let scratch = Scratch::create(parent.path(), "engine").unwrap();
        let staged = scratch
            .stage_inputs(&[
                Input {
                    name: "customers".into(),
                    path: customers,
                },
                Input {
                    name: "orders".into(),
                    path: orders,
                },
            ])
            .unwrap();
        let sql = "COPY (
  SELECT c.id AS customer_id, c.name, sum(o.amount) AS total
  FROM customers c
  JOIN orders o ON o.customer_id = c.id
  GROUP BY c.id, c.name
  ORDER BY c.id
) TO 'outputs/totals.csv' (HEADER, DELIMITER ',');";
        write_manifest(scratch.path(), sql, &staged).unwrap();
        let version = execute_in_scratch(scratch.path()).unwrap();
        assert!(version.starts_with("duckdb "), "{version}");
        let csv = fs::read_to_string(scratch.outputs_dir().join("totals.csv")).unwrap();
        assert!(csv.contains("customer_id"), "{csv}");
        assert!(csv.contains('a'), "{csv}");
        assert!(csv.contains('b'), "{csv}");
        assert!(
            csv.contains("25") || csv.contains("25.0"),
            "expected aggregate 25, got {csv}"
        );
        assert!(csv.contains('7'), "expected aggregate 7, got {csv}");
    }

    #[test]
    fn user_sql_cannot_read_outside_scratch() {
        let parent = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        let input = src.path().join("in.csv");
        fs::write(&input, "x\n1\n").unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        fs::write(outside.path(), "secret,val\nleak,1\n").unwrap();
        let scratch = Scratch::create(parent.path(), "confine").unwrap();
        let staged = scratch
            .stage_inputs(&[Input {
                name: "in".into(),
                path: input,
            }])
            .unwrap();
        let leak = sql_string(&outside.path().to_string_lossy());
        let sql = format!(
            "COPY (SELECT * FROM read_csv_auto('{leak}')) TO 'outputs/leak.csv' (HEADER, DELIMITER ',');"
        );
        write_manifest(scratch.path(), &sql, &staged).unwrap();
        let error = execute_in_scratch(scratch.path()).unwrap_err();
        assert!(
            error.contains("sql:") && error.contains("Cannot access"),
            "{error}"
        );
        assert!(
            !scratch.outputs_dir().join("leak.csv").exists(),
            "outside read must not produce an output"
        );
    }
}
