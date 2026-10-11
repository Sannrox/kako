//! In-process DuckDB execution used by the sandboxed worker child.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use duckdb::Connection;
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

fn execute_in_current_dir(scratch: &Path) -> Result<String, String> {
    let raw =
        fs::read_to_string("manifest.json").map_err(|error| format!("read manifest: {error}"))?;
    let manifest: Manifest =
        serde_json::from_str(&raw).map_err(|error| format!("parse manifest: {error}"))?;
    let conn = Connection::open_in_memory().map_err(|error| format!("open duckdb: {error}"))?;
    let tmp = scratch.join("tmp");
    let tmp = tmp.to_string_lossy().replace('\'', "''");
    conn.execute_batch(&format!(
        "PRAGMA threads=1;\n\
         SET autoload_known_extensions=false;\n\
         SET autoinstall_known_extensions=false;\n\
         SET temp_directory='{tmp}';"
    ))
    .map_err(|error| format!("duckdb sandbox settings: {error}"))?;
    for input in &manifest.inputs {
        let view = quote_ident(&input.name);
        let abs = scratch.join(&input.relative);
        let path = abs.to_string_lossy().replace('\'', "''");
        let from = scan_sql(&path);
        let sql = format!("CREATE OR REPLACE VIEW {view} AS SELECT * FROM {from}");
        conn.execute_batch(&sql)
            .map_err(|error| format!("create view `{}`: {error}", input.name))?;
    }
    conn.execute_batch(&manifest.sql)
        .map_err(|error| format!("sql: {error}"))?;
    let version: String = conn
        .query_row("SELECT version()", [], |row| row.get(0))
        .map_err(|error| format!("duckdb version: {error}"))?;
    let version = version.trim().trim_start_matches('v');
    Ok(format!("duckdb {version}"))
}

fn quote_ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
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
}
