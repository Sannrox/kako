//! In-process DuckDB execution used by the sandboxed worker child.

use std::io::{self, Write};
use std::path::Path;
use std::sync::Mutex;

use duckdb::{Config, Connection};
use kako_core::manifest::{PREVIEW_FILE, StagedInput};

static CWD_LOCK: Mutex<()> = Mutex::new(());

/// Most output rows a preview records, whatever the input sample.
pub const MAX_CAPTURE_ROWS: u64 = 10_000;

/// Preview or validation request: each input is its first `rows` rows. With
/// `capture`, the schema and first rows of every declared output are recorded.
#[derive(Debug, Clone, Copy)]
pub struct Sample<'a> {
    pub rows: u64,
    pub capture: bool,
    pub outputs: &'a [String],
}

/// Run staged SQL through DuckDB. Returns `duckdb <version>`.
///
/// User SQL uses paths relative to the scratch directory (`inputs/`, `outputs/`),
/// so this function runs with that directory as the process cwd. A lock keeps
/// in-process tests from racing on cwd.
pub fn execute(sql: &str, inputs: &[StagedInput], scratch: &Path) -> Result<String, String> {
    execute_sampled(sql, inputs, scratch, None)
}

/// [`execute`], or a preview run when `sample` is set: inputs are capped and
/// [`PREVIEW_FILE`] records each output's columns and rows.
pub fn execute_sampled(
    sql: &str,
    inputs: &[StagedInput],
    scratch: &Path,
    sample: Option<Sample<'_>>,
) -> Result<String, String> {
    let _cwd = CWD_LOCK.lock().unwrap_or_else(|poison| poison.into_inner());
    let previous = std::env::current_dir().ok();
    std::env::set_current_dir(scratch)
        .map_err(|error| format!("chdir {}: {error}", scratch.display()))?;
    let result = execute_in_current_dir(sql, inputs, scratch, sample);
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

fn execute_in_current_dir(
    sql: &str,
    inputs: &[StagedInput],
    scratch: &Path,
    sample: Option<Sample<'_>>,
) -> Result<String, String> {
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
    // Preview and validate load each input's first rows into a table, with
    // the types a full run infers, before access is locked. The staged
    // inputs then leave the allowed paths, so a direct file scan fails closed
    // instead of reading past the sample.
    if let Some(sample) = sample {
        engine_step("sample");
        for input in inputs {
            let table = quote_ident(&input.name);
            let path = sql_string(&scratch_abs.join(&input.relative).to_string_lossy());
            conn.execute_batch(&format!(
                "CREATE TABLE {table} AS SELECT * FROM {} LIMIT {}",
                scan_sql(&path),
                sample.rows
            ))
            .map_err(|error| format!("sample input `{}`: {error}", input.name))?;
        }
    }
    engine_step("settings");
    // Open with external access so construction can read host identity files
    // the kernel sandbox grants. Then allow only the scratch tree and lock
    // the configuration so user SQL cannot follow those grants.
    let allowed = if sample.is_some() {
        let outputs = sql_string(&scratch_abs.join("outputs").to_string_lossy());
        let tmp = sql_string(&tmp_s);
        format!("'{outputs}', '{tmp}', 'outputs', 'tmp'")
    } else {
        format!("'{}', 'inputs', 'outputs', 'tmp'", sql_string(&home_s))
    };
    conn.execute_batch(&format!(
        "SET enable_progress_bar=false;\n\
         SET autoload_known_extensions=false;\n\
         SET autoinstall_known_extensions=false;\n\
         SET enable_external_file_cache=false;\n\
         SET allocator_background_threads=false;\n\
         SET allowed_directories = [{allowed}];\n\
         SET enable_external_access = false;\n\
         SET lock_configuration = true;"
    ))
    .map_err(|error| format!("duckdb settings: {error}"))?;
    if sample.is_none() {
        engine_step("views");
        for input in inputs {
            let view = quote_ident(&input.name);
            let abs = scratch_abs.join(&input.relative);
            let path = sql_string(&abs.to_string_lossy());
            let from = scan_sql(&path);
            let sql = format!("CREATE OR REPLACE VIEW {view} AS SELECT * FROM {from}");
            conn.execute_batch(&sql)
                .map_err(|error| format!("create view `{}`: {error}", input.name))?;
        }
    }
    engine_step("sql");
    conn.execute_batch(sql)
        .map_err(|error| format!("sql: {error}"))?;
    if let Some(sample) = sample.filter(|sample| sample.capture) {
        engine_step("preview");
        write_preview(&conn, &scratch_abs, sample)?;
    }
    engine_step("version");
    engine_version(&conn)
}

/// `duckdb <version>` of the linked library, as receipts spell it. Replay
/// checks a pinned engine against this before running anything.
pub fn version() -> Result<String, String> {
    let conn = Connection::open_in_memory().map_err(|error| format!("duckdb open: {error}"))?;
    engine_version(&conn)
}

fn engine_version(conn: &Connection) -> Result<String, String> {
    let version: String = conn
        .query_row("SELECT version()", [], |row| row.get(0))
        .map_err(|error| format!("duckdb version: {error}"))?;
    let version = version.trim().trim_start_matches('v');
    Ok(format!("duckdb {version}"))
}

/// Column names and DuckDB types of `SELECT * FROM {from}`.
fn describe(conn: &Connection, from: &str) -> Result<Vec<(String, String)>, duckdb::Error> {
    let mut describe = conn.prepare(&format!("DESCRIBE SELECT * FROM {from}"))?;
    describe
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect()
}

/// Record each declared output's columns and first rows (as text) in
/// [`PREVIEW_FILE`]. A declared output the SQL did not write is an error.
fn write_preview(conn: &Connection, scratch_abs: &Path, sample: Sample<'_>) -> Result<(), String> {
    let mut outputs = Vec::new();
    for name in sample.outputs {
        let path = scratch_abs.join("outputs").join(name);
        if !path.is_file() {
            return Err(format!("preview: declared output `{name}` was not written"));
        }
        let preview_error = |error: duckdb::Error| format!("preview `{name}`: {error}");
        let columns = describe(conn, &output_scan_sql(&path, false)).map_err(preview_error)?;
        let mut select = conn
            .prepare(&format!(
                "SELECT COLUMNS(*)::VARCHAR FROM {} LIMIT {}",
                output_scan_sql(&path, true),
                sample.rows.min(MAX_CAPTURE_ROWS)
            ))
            .map_err(preview_error)?;
        let rows = select
            .query_map([], |row| {
                (0..columns.len())
                    .map(|index| row.get::<_, Option<String>>(index))
                    .collect::<Result<Vec<_>, _>>()
            })
            .map_err(preview_error)?
            .collect::<Result<Vec<_>, _>>()
            .map_err(preview_error)?;
        outputs.push(serde_json::json!({
            "name": name,
            "columns": columns
                .iter()
                .map(|(name, kind)| serde_json::json!({"name": name, "type": kind}))
                .collect::<Vec<_>>(),
            "rows": rows,
        }));
    }
    let body = serde_json::json!({ "outputs": outputs });
    std::fs::write(scratch_abs.join(PREVIEW_FILE), body.to_string())
        .map_err(|error| format!("write preview: {error}"))
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

/// Scan of a written output. Parquet is recognized by its magic bytes, not
/// the file name; anything else is read as headered delimited text. With
/// `as_text`, delimited values are read as written (`001` stays `001`).
fn output_scan_sql(path: &Path, as_text: bool) -> String {
    let mut magic = [0u8; 4];
    let parquet = std::fs::File::open(path)
        .and_then(|mut file| std::io::Read::read_exact(&mut file, &mut magic))
        .is_ok()
        && &magic == b"PAR1";
    let quoted = sql_string(&path.to_string_lossy());
    if parquet {
        return format!("read_parquet('{quoted}')");
    }
    let sep = if quoted.to_ascii_lowercase().ends_with(".tsv") {
        ", sep='\t'"
    } else {
        ""
    };
    let text = if as_text { ", all_varchar=true" } else { "" };
    format!("read_csv_auto('{quoted}', header=true{sep}{text})")
}

#[cfg(test)]
mod tests {
    use super::*;
    use kako_core::Input;
    use kako_core::stage::Scratch;
    use std::fs;
    use std::path::PathBuf;

    fn staged(scratch: &Scratch, inputs: &[Input]) -> Vec<StagedInput> {
        scratch
            .stage_inputs(inputs)
            .unwrap()
            .into_iter()
            .map(|(name, relative)| StagedInput {
                name,
                relative: relative.display().to_string(),
                ..Default::default()
            })
            .collect()
    }

    #[test]
    fn copy_constant_row_writes_output() {
        let parent = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        let input = src.path().join("in.csv");
        fs::write(&input, "x\n1\n").unwrap();
        let scratch = Scratch::create(parent.path(), "probe").unwrap();
        let inputs = staged(
            &scratch,
            &[Input {
                name: "in".into(),
                path: input,
                markings: Vec::new(),
                column_markings: Default::default(),
                object_set: String::new(),
            }],
        );
        let sql = "COPY (SELECT 1 AS x) TO 'outputs/one.csv' (HEADER, DELIMITER ',');";
        let version = execute(sql, &inputs, scratch.path()).unwrap();
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
        let inputs = staged(
            &scratch,
            &[
                Input {
                    name: "customers".into(),
                    path: customers,
                    markings: Vec::new(),
                    column_markings: Default::default(),
                    object_set: String::new(),
                },
                Input {
                    name: "orders".into(),
                    path: orders,
                    markings: Vec::new(),
                    column_markings: Default::default(),
                    object_set: String::new(),
                },
            ],
        );
        let sql = "COPY (
  SELECT c.id AS customer_id, c.name, sum(o.amount) AS total
  FROM customers c
  JOIN orders o ON o.customer_id = c.id
  GROUP BY c.id, c.name
  ORDER BY c.id
) TO 'outputs/totals.csv' (HEADER, DELIMITER ',');";
        let version = execute(sql, &inputs, scratch.path()).unwrap();
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

    fn engine_bar_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/engine-bar")
    }

    #[test]
    fn engine_bar_join_matches_fixture() {
        let bar = engine_bar_dir();
        let job: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(bar.join("join.json")).expect("scripts/engine-bar/join.json"),
        )
        .unwrap();
        let sql = job["sql"].as_str().expect("join.json sql").to_string();
        let parent = tempfile::tempdir().unwrap();
        let scratch = Scratch::create(parent.path(), "engine-bar").unwrap();
        let inputs = staged(
            &scratch,
            &[
                Input {
                    name: "customers".into(),
                    path: bar.join("customers.csv"),
                    markings: Vec::new(),
                    column_markings: Default::default(),
                    object_set: String::new(),
                },
                Input {
                    name: "orders".into(),
                    path: bar.join("orders.csv"),
                    markings: Vec::new(),
                    column_markings: Default::default(),
                    object_set: String::new(),
                },
            ],
        );
        let version = execute(&sql, &inputs, scratch.path()).unwrap();
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
        let inputs = staged(
            &scratch,
            &[Input {
                name: "in".into(),
                path: input,
                markings: Vec::new(),
                column_markings: Default::default(),
                object_set: String::new(),
            }],
        );
        let leak = sql_string(&outside.path().to_string_lossy());
        let sql = format!(
            "COPY (SELECT * FROM read_csv_auto('{leak}')) TO 'outputs/leak.csv' (HEADER, DELIMITER ',');"
        );
        let error = execute(&sql, &inputs, scratch.path()).unwrap_err();
        assert!(
            error.contains("sql:") && error.contains("Cannot access"),
            "{error}"
        );
        assert!(
            !scratch.outputs_dir().join("leak.csv").exists(),
            "outside read must not produce an output"
        );
    }

    const JOIN_SQL: &str = "COPY (SELECT c.name, sum(o.amount) AS total FROM customers c JOIN orders o ON o.customer_id = c.id GROUP BY c.name ORDER BY c.name) TO 'outputs/totals.csv' (HEADER, DELIMITER ',');";

    fn input(name: &str, path: PathBuf) -> Input {
        Input {
            name: name.into(),
            path,
            markings: Vec::new(),
            column_markings: Default::default(),
            object_set: String::new(),
        }
    }

    /// Stage `customers` and `orders` with `orders_rows` order lines.
    fn join_inputs(scratch: &Scratch, src: &Path, orders_rows: usize) -> Vec<StagedInput> {
        let customers = src.join("customers.csv");
        fs::write(&customers, "id,name\n1,a\n2,b\n").unwrap();
        let orders = src.join("orders.csv");
        let mut body = String::from("customer_id,amount\n");
        for row in 0..orders_rows {
            body.push_str(&format!("{},{}\n", row % 2 + 1, row % 7));
        }
        fs::write(&orders, body).unwrap();
        staged(
            scratch,
            &[input("customers", customers), input("orders", orders)],
        )
    }

    fn preview(rows: u64, outputs: &[String]) -> Option<Sample<'_>> {
        Some(Sample {
            rows,
            capture: true,
            outputs,
        })
    }

    fn validation() -> Option<Sample<'static>> {
        Some(Sample {
            rows: 0,
            capture: false,
            outputs: &[],
        })
    }

    fn read_preview(scratch: &Scratch) -> serde_json::Value {
        serde_json::from_str(&fs::read_to_string(scratch.path().join(PREVIEW_FILE)).unwrap())
            .unwrap()
    }

    #[test]
    fn preview_caps_inputs_and_records_output_schema_and_rows() {
        let parent = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        let scratch = Scratch::create(parent.path(), "preview").unwrap();
        let inputs = join_inputs(&scratch, src.path(), 6);
        let outputs = ["totals.csv".to_string()];
        execute_sampled(JOIN_SQL, &inputs, scratch.path(), preview(2, &outputs)).unwrap();
        let out = &read_preview(&scratch)["outputs"][0];
        assert_eq!(out["name"], "totals.csv");
        assert_eq!(out["columns"][0]["name"], "name");
        assert_eq!(out["columns"][1]["name"], "total");
        // Two order lines survive the cap: amounts 0 (a) and 1 (b).
        assert_eq!(out["rows"], serde_json::json!([["a", "0"], ["b", "1"]]));
    }

    #[test]
    fn validation_binds_typed_empty_inputs_and_reports_errors_with_location() {
        let parent = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        // sum(o.amount) only binds when amount keeps its inferred type.
        let scratch = Scratch::create(parent.path(), "valid").unwrap();
        let inputs = join_inputs(&scratch, src.path(), 3);
        execute_sampled(JOIN_SQL, &inputs, scratch.path(), validation()).unwrap();
        assert!(!scratch.path().join(PREVIEW_FILE).exists());
        for (run, from, to) in [
            ("key", "o.customer_id", "o.customer"),
            ("type", "sum(o.amount)", "sum(c.name)"),
        ] {
            let scratch = Scratch::create(parent.path(), run).unwrap();
            let inputs = join_inputs(&scratch, src.path(), 1);
            let sql = JOIN_SQL.replace(from, to);
            let error = execute_sampled(&sql, &inputs, scratch.path(), validation()).unwrap_err();
            assert!(
                error.contains("Binder Error") && error.contains("LINE 1"),
                "{run}: {error}"
            );
        }
    }

    #[test]
    fn sampled_runs_refuse_direct_scans_of_staged_inputs() {
        let parent = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        let scratch = Scratch::create(parent.path(), "direct").unwrap();
        let inputs = join_inputs(&scratch, src.path(), 3);
        let sql = "COPY (SELECT count(*) AS n FROM read_csv_auto('inputs/orders.csv')) TO 'outputs/n.csv' (HEADER);";
        let error = execute_sampled(sql, &inputs, scratch.path(), validation()).unwrap_err();
        assert!(error.contains("Permission Error"), "{error}");
    }

    #[test]
    fn preview_reports_a_declared_output_the_sql_did_not_write() {
        let parent = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        let scratch = Scratch::create(parent.path(), "missing").unwrap();
        let inputs = join_inputs(&scratch, src.path(), 1);
        let outputs = ["other.csv".to_string()];
        let error =
            execute_sampled(JOIN_SQL, &inputs, scratch.path(), preview(5, &outputs)).unwrap_err();
        assert!(error.contains("`other.csv` was not written"), "{error}");
    }

    #[test]
    fn preview_reads_outputs_by_content_and_keeps_text() {
        let parent = tempfile::tempdir().unwrap();
        let scratch = Scratch::create(parent.path(), "formats").unwrap();
        let sql = "COPY (SELECT '001' AS code) TO 'outputs/codes.csv' (HEADER); \
                   COPY (SELECT 7 AS n) TO 'outputs/result.bin' (FORMAT PARQUET);";
        let outputs = ["codes.csv".to_string(), "result.bin".to_string()];
        execute_sampled(sql, &[], scratch.path(), preview(5, &outputs)).unwrap();
        let preview = read_preview(&scratch);
        assert_eq!(preview["outputs"][0]["rows"], serde_json::json!([["001"]]));
        assert_eq!(preview["outputs"][1]["columns"][0]["type"], "INTEGER");
        assert_eq!(preview["outputs"][1]["rows"], serde_json::json!([["7"]]));
    }

    #[test]
    fn preview_over_a_million_rows_reads_only_the_sample() {
        let parent = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        let scratch = Scratch::create(parent.path(), "large").unwrap();
        let inputs = join_inputs(&scratch, src.path(), 1_000_000);
        let outputs = ["totals.csv".to_string()];
        let started = std::time::Instant::now();
        execute_sampled(JOIN_SQL, &inputs, scratch.path(), preview(10, &outputs)).unwrap();
        let elapsed = started.elapsed();
        // Ten order lines aggregate to totals 0+2+4+6+1 (a) and 1+3+5+0+2 (b).
        assert_eq!(
            read_preview(&scratch)["outputs"][0]["rows"],
            serde_json::json!([["a", "13"], ["b", "11"]])
        );
        assert!(
            elapsed < std::time::Duration::from_secs(20),
            "preview took {elapsed:?}"
        );
    }
}
