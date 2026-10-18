//! Opt-in mizuchi SQL worker used by the sandboxed child.

use std::fs::File;
use std::path::{Path, PathBuf};

use arrow::csv::WriterBuilder;
use mizuchi::{
    CancelToken, CsvReadOptions, Error, ParquetReadOptions, QueryReport, Session, SessionConfig,
    VERSION,
};

use kako_core::manifest::StagedInput;
use kako_core::{Lineage, LineageRead, LineageWrite, WriteMode};

/// Run staged SQL through mizuchi. Returns `mizuchi <version>`.
pub fn execute(
    sql: &str,
    inputs: &[StagedInput],
    scratch: &Path,
    write_mode: WriteMode,
) -> Result<String, String> {
    execute_with(
        sql,
        inputs,
        scratch,
        SessionLimits::default(),
        CancelToken::new(),
        write_mode,
    )
}

struct SessionLimits {
    memory_limit: Option<u64>,
    workers: usize,
}

impl Default for SessionLimits {
    fn default() -> Self {
        Self {
            memory_limit: Some(256 * 1024 * 1024),
            workers: 1,
        }
    }
}

fn execute_with(
    sql: &str,
    inputs: &[StagedInput],
    scratch: &Path,
    limits: SessionLimits,
    cancel: CancelToken,
    write_mode: WriteMode,
) -> Result<String, String> {
    let scratch_abs = scratch
        .canonicalize()
        .map_err(|error| format!("canonicalize scratch: {error}"))?;
    let tmp = scratch_abs.join("tmp");
    std::fs::create_dir_all(&tmp).map_err(|error| format!("create spill dir: {error}"))?;
    let cfg = SessionConfig {
        memory_limit: limits.memory_limit,
        workers: Some(limits.workers),
        spill_dir: Some(tmp),
        allowed_read_paths: vec![scratch_abs.clone()],
        allowed_write_paths: vec![scratch_abs.clone()],
        ..SessionConfig::default()
    };
    let mut session = Session::from_config(cfg)
        .map_err(map_error)?
        .with_cancel(cancel);
    for input in inputs {
        let path = scratch_abs.join(&input.relative);
        register_input(&mut session, &input.name, &path)?;
    }
    if write_mode == WriteMode::Append {
        return Err("mizuchi COPY writes a new CSV; write_mode append is not supported".into());
    }
    let (query, dest) = copy_select_to(sql)?;
    let dest_abs = scratch_abs.join(&dest);
    if !dest_abs.starts_with(scratch_abs.join("outputs")) {
        return Err(format!(
            "output `{}` must be under outputs/",
            dest.display()
        ));
    }
    let result = session.execute_sql_reported(&query).map_err(map_error)?;
    let batches = result.batches;
    let schema = if let Some(first) = batches.first() {
        first.schema()
    } else {
        session
            .logical_plan(&query)
            .map_err(map_error)?
            .schema()
            .clone()
    };
    write_csv(&dest_abs, &schema, &batches)?;
    let mut lineage = lineage_from_report(&result.report, &scratch_abs);
    let rows: u64 = batches.iter().map(|batch| batch.num_rows() as u64).sum();
    let bytes = std::fs::metadata(&dest_abs)
        .map(|meta| meta.len())
        .unwrap_or(0);
    let rel = dest.to_string_lossy().replace('\\', "/");
    let mode = write_mode.as_str();
    lineage.writes.push(LineageWrite {
        path: rel,
        partitions: Vec::new(),
        mode: mode.into(),
        rows,
        bytes,
        commit_id: format!("{mode}:{rows}:{bytes}:"),
    });
    write_lineage(scratch, &lineage)?;
    Ok(format!("mizuchi {VERSION}"))
}

fn register_input(session: &mut Session, name: &str, path: &Path) -> Result<(), String> {
    let lower = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if path.is_dir() || lower == "parquet" {
        session
            .register_parquet(name, path, ParquetReadOptions::default())
            .map_err(map_error)
    } else if lower == "tsv" {
        Err("mizuchi adapter does not read TSV inputs; convert to CSV or parquet".into())
    } else {
        session
            .register_csv(
                name,
                path,
                CsvReadOptions {
                    has_header: true,
                    infer_schema: true,
                    projection: None,
                    policy: mizuchi::BatchPolicy::default(),
                },
            )
            .map_err(map_error)
    }
}

fn relativize(scratch: &Path, path: &str) -> String {
    let given = PathBuf::from(path);
    if let Ok(rel) = given.strip_prefix(scratch) {
        return rel.to_string_lossy().replace('\\', "/");
    }
    let scratch_c = scratch.canonicalize().ok();
    let given_c = given.canonicalize().ok();
    if let (Some(scratch_c), Some(given_c)) = (scratch_c, given_c)
        && let Ok(rel) = given_c.strip_prefix(&scratch_c)
    {
        return rel.to_string_lossy().replace('\\', "/");
    }
    path.replace('\\', "/")
}

pub(crate) fn lineage_from_report(report: &QueryReport, scratch: &Path) -> Lineage {
    Lineage {
        plan: report.plan.clone(),
        reads: report
            .inputs
            .iter()
            .map(|scan| LineageRead {
                dataset: scan.dataset.clone(),
                path: scan.path.as_deref().map(|p| relativize(scratch, p)),
                partitions: scan.partitions.clone(),
                columns: scan.columns.clone(),
            })
            .collect(),
        writes: report
            .outputs
            .iter()
            .map(|write| LineageWrite {
                path: relativize(scratch, &write.path),
                partitions: write.partitions.clone(),
                mode: write.mode.clone(),
                rows: write.rows,
                bytes: write.bytes,
                commit_id: write.commit_id.clone(),
            })
            .collect(),
    }
}

fn write_lineage(scratch: &Path, lineage: &Lineage) -> Result<(), String> {
    let mut json = serde_json::to_string_pretty(lineage)
        .map_err(|error| format!("serialize lineage: {error}"))?;
    json.push('\n');
    std::fs::write(scratch.join("lineage.json"), json)
        .map_err(|error| format!("write lineage.json: {error}"))
}

fn map_error(error: Error) -> String {
    match error {
        Error::Memory(reason) => format!("limit: memory: {reason}"),
        Error::Cancelled => "limit: cancelled".into(),
        other => format!("sql: {other}"),
    }
}

/// Strip an ASCII keyword at the start of `s`, ignoring case. The keyword
/// must be a whole word (not a prefix of a longer identifier).
fn strip_ascii_keyword<'a>(s: &'a str, keyword: &str) -> Option<&'a str> {
    let s = s.trim_start();
    let n = keyword.len();
    if s.len() >= n && s.as_bytes()[..n].eq_ignore_ascii_case(keyword.as_bytes()) {
        let rest = &s[n..];
        if rest
            .chars()
            .next()
            .is_none_or(|c| !c.is_ascii_alphanumeric() && c != '_')
        {
            return Some(rest.trim_start());
        }
    }
    None
}

/// Index of the closing `)` that matches the `(` at `s[0]`, skipping quotes.
fn closing_paren(s: &str) -> Option<usize> {
    if !s.starts_with('(') {
        return None;
    }
    let mut depth = 0i32;
    let mut in_single = false;
    let mut in_double = false;
    for (i, ch) in s.char_indices() {
        if in_single {
            if ch == '\'' {
                in_single = false;
            }
            continue;
        }
        if in_double {
            if ch == '"' {
                in_double = false;
            }
            continue;
        }
        match ch {
            '\'' => in_single = true,
            '"' => in_double = true,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// SQL single- or double-quoted string. Doubled quotes are an escaped quote.
fn take_quoted(s: &str) -> Result<(String, &str), String> {
    let bytes = s.as_bytes();
    let quote = match bytes.first().copied() {
        Some(q @ (b'\'' | b'"')) => q,
        _ => return Err("COPY TO path must be a quoted string".into()),
    };
    let mut out = String::new();
    let mut i = 1;
    while i < bytes.len() {
        if bytes[i] == quote {
            if i + 1 < bytes.len() && bytes[i + 1] == quote {
                out.push(quote as char);
                i += 2;
                continue;
            }
            return Ok((out, s[i + 1..].trim_start()));
        }
        let ch = s[i..].chars().next().ok_or("unterminated COPY TO path")?;
        out.push(ch);
        i += ch.len_utf8();
    }
    Err("unterminated COPY TO path".into())
}

/// kako jobs write files with DuckDB COPY syntax. Strip that wrapper so the
/// inner SELECT runs on mizuchi, then write the named output ourselves.
fn copy_select_to(sql: &str) -> Result<(String, PathBuf), String> {
    let trimmed = sql.trim().trim_end_matches(';').trim();
    let rest = strip_ascii_keyword(trimmed, "COPY").ok_or_else(|| {
        "mizuchi adapter expects COPY (SELECT ...) TO 'outputs/<file>'".to_string()
    })?;
    let end = closing_paren(rest).ok_or("mizuchi adapter expects COPY (SELECT ...)")?;
    let inner = rest[1..end].trim().to_string();
    let after = strip_ascii_keyword(rest[end + 1..].trim_start(), "TO").ok_or("COPY missing TO")?;
    let (dest_str, leftover) = take_quoted(after)?;
    let dest = PathBuf::from(dest_str);
    if dest.as_os_str().is_empty()
        || dest.components().next() != Some(std::path::Component::Normal("outputs".as_ref()))
        || dest.components().count() != 2
    {
        return Err(format!(
            "COPY TO `{}` must be a file under outputs/",
            dest.display()
        ));
    }
    let leftover = leftover.trim();
    let options = if leftover.is_empty() {
        String::new()
    } else if leftover.starts_with('(') {
        let end = closing_paren(leftover).ok_or("unbalanced COPY options")?;
        let after_opts = leftover[end + 1..].trim().trim_end_matches(';').trim();
        if !after_opts.is_empty() {
            return Err("mizuchi adapter accepts a single COPY statement".into());
        }
        leftover[..=end].to_ascii_lowercase()
    } else {
        return Err("mizuchi adapter accepts a single COPY statement".into());
    };
    if options.contains("parquet") || options.contains("format") {
        return Err(
            "mizuchi adapter writes CSV; FORMAT/PARQUET COPY options are not supported".into(),
        );
    }
    if options.contains("header false") || options.contains("header 0") {
        return Err("mizuchi adapter always writes a CSV header".into());
    }
    if let Some(rest) = options.split("delimiter").nth(1)
        && !rest.contains("','")
        && !rest.contains("\",\"")
        && !rest.contains("comma")
    {
        return Err("mizuchi adapter only writes comma-delimited CSV".into());
    }
    Ok((inner, dest))
}

fn write_csv(
    path: &Path,
    schema: &mizuchi::SchemaRef,
    batches: &[mizuchi::Batch],
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    let file = File::create(path).map_err(|error| format!("create {}: {error}", path.display()))?;
    let mut writer = WriterBuilder::new().with_header(true).build(file);
    if batches.is_empty() {
        let empty = mizuchi::RecordBatch::new_empty(schema.clone());
        writer
            .write(&empty)
            .map_err(|error| format!("write {}: {error}", path.display()))?;
    } else {
        for batch in batches {
            writer
                .write(batch.as_arrow())
                .map_err(|error| format!("write {}: {error}", path.display()))?;
        }
    }
    // arrow-csv Writer::write flushes; into_inner returns the File, not a Result.
    drop(writer);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use kako_core::Input;
    use kako_core::WriteMode;
    use kako_core::manifest::StagedInput;
    use kako_core::stage::Scratch;
    use std::fs;

    fn mizuchi_write_mode(mode: WriteMode) -> mizuchi::WriteMode {
        match mode {
            WriteMode::Overwrite => mizuchi::WriteMode::Overwrite,
            WriteMode::Append => mizuchi::WriteMode::Append,
        }
    }

    fn engine_bar_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/engine-bar")
    }

    fn as_inputs(staged: &[(String, PathBuf)]) -> Vec<StagedInput> {
        staged
            .iter()
            .map(|(name, relative)| StagedInput {
                name: name.clone(),
                relative: relative.display().to_string(),
            })
            .collect()
    }

    fn run_sql(
        scratch: &Scratch,
        sql: &str,
        staged: &[(String, PathBuf)],
        mode: WriteMode,
    ) -> Result<String, String> {
        execute(sql, &as_inputs(staged), scratch.path(), mode)
    }

    #[test]
    fn copy_select_to_extracts_inner_query_and_output() {
        let sql = "COPY (\n  SELECT 1 AS x\n) TO 'outputs/one.csv' (HEADER, DELIMITER ',');";
        let (inner, dest) = copy_select_to(sql).unwrap();
        assert!(inner.contains("SELECT 1 AS x"), "{inner}");
        assert_eq!(dest, PathBuf::from("outputs/one.csv"));
        let (inner, dest) =
            copy_select_to("COPY (SELECT ')' AS x) TO 'outputs/one.csv' (HEADER, DELIMITER ',');")
                .unwrap();
        assert_eq!(inner, "SELECT ')' AS x");
        assert_eq!(dest, PathBuf::from("outputs/one.csv"));
        let error =
            copy_select_to("COPY (SELECT 1 AS x) TO 'outputs/one.parquet' (FORMAT PARQUET);")
                .unwrap_err();
        assert!(error.contains("CSV"), "{error}");
        let (inner, dest) =
            copy_select_to("Copy (SELECT 1 AS x) To 'outputs/one.csv' (Header, Delimiter ',');")
                .unwrap();
        assert_eq!(inner, "SELECT 1 AS x");
        assert_eq!(dest, PathBuf::from("outputs/one.csv"));
        let (inner, dest) = copy_select_to("cOpY (SELECT 2 AS y) tO \"outputs/two.csv\";").unwrap();
        assert_eq!(inner, "SELECT 2 AS y");
        assert_eq!(dest, PathBuf::from("outputs/two.csv"));
        let (inner, dest) = copy_select_to("COPY (SELECT 1 AS x) TO 'outputs/a''b.csv';").unwrap();
        assert_eq!(inner, "SELECT 1 AS x");
        assert_eq!(dest, PathBuf::from("outputs/a'b.csv"));
        assert!(copy_select_to("COPYING (SELECT 1) TO 'outputs/one.csv'").is_err());
        let error = copy_select_to(
            "COPY (SELECT 1 AS x) TO 'outputs/one.csv' (HEADER, DELIMITER ','); DROP TABLE t",
        )
        .unwrap_err();
        assert!(error.contains("single COPY"), "{error}");
    }

    #[test]
    fn join_fixture_matches_engine_bar() {
        let bar = engine_bar_dir();
        let job: serde_json::Value = serde_json::from_str(
            &fs::read_to_string(bar.join("join-mizuchi.json")).expect("join-mizuchi.json"),
        )
        .unwrap();
        let sql = job["sql"].as_str().expect("sql").to_string();
        let (inner, dest) = copy_select_to(&sql).unwrap();
        assert_eq!(dest, PathBuf::from("outputs/totals.csv"));
        assert!(inner.contains("JOIN orders"), "{inner}");
        let parent = tempfile::tempdir().unwrap();
        let scratch = Scratch::create(parent.path(), "mizuchi-join").unwrap();
        let staged = scratch
            .stage_inputs(&[
                Input {
                    name: "customers".into(),
                    path: bar.join("customers.csv"),
                },
                Input {
                    name: "orders".into(),
                    path: bar.join("orders.csv"),
                },
            ])
            .unwrap();
        let version = run_sql(&scratch, &sql, &staged, WriteMode::Overwrite).unwrap();
        assert!(version.starts_with("mizuchi "), "{version}");
        let csv = fs::read_to_string(scratch.outputs_dir().join("totals.csv")).unwrap();
        assert!(csv.contains("customer_id") || csv.contains("id"), "{csv}");
        assert!(csv.contains('a'), "{csv}");
        assert!(csv.contains('b'), "{csv}");
        assert!(
            csv.contains("25") || csv.contains("25.0"),
            "expected aggregate 25, got {csv}"
        );
        assert!(csv.contains('7'), "expected aggregate 7, got {csv}");
        let lineage: kako_core::Lineage =
            serde_json::from_str(&fs::read_to_string(scratch.path().join("lineage.json")).unwrap())
                .unwrap();
        assert!(lineage.plan.contains("Logical Plan"), "{}", lineage.plan);
        let datasets: Vec<_> = lineage.reads.iter().map(|r| r.dataset.as_str()).collect();
        assert!(datasets.contains(&"customers"), "{datasets:?}");
        assert!(datasets.contains(&"orders"), "{datasets:?}");
        assert_eq!(lineage.writes.len(), 1);
        assert_eq!(lineage.writes[0].path, "outputs/totals.csv");
        assert_eq!(lineage.writes[0].mode, "overwrite");
        let parent2 = tempfile::tempdir().unwrap();
        let scratch2 = Scratch::create(parent2.path(), "mizuchi-join-2").unwrap();
        let staged2 = scratch2
            .stage_inputs(&[
                Input {
                    name: "customers".into(),
                    path: bar.join("customers.csv"),
                },
                Input {
                    name: "orders".into(),
                    path: bar.join("orders.csv"),
                },
            ])
            .unwrap();
        run_sql(&scratch2, &sql, &staged2, WriteMode::Overwrite).unwrap();
        let lineage2: kako_core::Lineage = serde_json::from_str(
            &fs::read_to_string(scratch2.path().join("lineage.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(lineage, lineage2);
        let snap = receipt_snapshot(&lineage);
        let snap2 = receipt_snapshot(&lineage2);
        assert_eq!(snap, snap2);
        assert_eq!(snap["lineage"]["writes"][0]["mode"], "overwrite");
    }

    fn receipt_snapshot(lineage: &kako_core::Lineage) -> serde_json::Value {
        let receipt = kako_core::Receipt {
            job_id: "snap".into(),
            inputs: vec![],
            code: kako_core::Digest("sha256:code".into()),
            engine_version: "mizuchi test".into(),
            outputs: vec![],
            started_at: std::time::SystemTime::UNIX_EPOCH,
            duration: std::time::Duration::from_millis(1),
            tenant_id: "local".into(),
            namespace: "default".into(),
            cpu_usec: None,
            memory_peak_bytes: None,
            lineage: Some(lineage.clone()),
            runtime_image: None,
            isolation: kako_core::IsolationTier::LinuxNative,
            credential_scope: None,
        };
        let json = kako_core::receipt::to_json(&receipt).unwrap();
        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        let obj = value.as_object_mut().unwrap();
        obj.remove("started_at_unix_ms");
        obj.remove("duration_ms");
        value
    }

    fn run_copy_with_mode(
        scratch: &Scratch,
        staged: &[(String, PathBuf)],
        sql: &str,
        write_mode: WriteMode,
    ) -> kako_core::Lineage {
        run_sql(scratch, sql, staged, write_mode).unwrap();
        serde_json::from_str(&fs::read_to_string(scratch.path().join("lineage.json")).unwrap())
            .unwrap()
    }

    #[test]
    fn write_mode_receipts_are_stable_for_append_and_overwrite() {
        let src = tempfile::tempdir().unwrap();
        let input = src.path().join("in.csv");
        fs::write(&input, "x\n1\n").unwrap();
        let sql = "COPY (SELECT * FROM in) TO 'outputs/one.csv' (HEADER, DELIMITER ',');";
        let parent = tempfile::tempdir().unwrap();
        let scratch = Scratch::create(parent.path(), "mode-over").unwrap();
        let staged = scratch
            .stage_inputs(&[Input {
                name: "in".into(),
                path: input.clone(),
            }])
            .unwrap();
        let first = run_copy_with_mode(&scratch, &staged, sql, WriteMode::Overwrite);
        let parent2 = tempfile::tempdir().unwrap();
        let scratch2 = Scratch::create(parent2.path(), "mode-over-2").unwrap();
        let staged2 = scratch2
            .stage_inputs(&[Input {
                name: "in".into(),
                path: input.clone(),
            }])
            .unwrap();
        let second = run_copy_with_mode(&scratch2, &staged2, sql, WriteMode::Overwrite);
        assert_eq!(first.writes[0].mode, "overwrite");
        assert_eq!(receipt_snapshot(&first), receipt_snapshot(&second));

        let err = run_sql(&scratch, sql, &staged, WriteMode::Append).unwrap_err();
        assert!(err.contains("append"), "{err}");
        assert!(err.contains("COPY"), "{err}");
    }

    #[test]
    fn tsv_input_is_rejected() {
        let parent = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        let input = src.path().join("in.tsv");
        fs::write(&input, "x\ty\n1\t2\n").unwrap();
        let scratch = Scratch::create(parent.path(), "mizuchi-tsv").unwrap();
        let staged = scratch
            .stage_inputs(&[Input {
                name: "in".into(),
                path: input,
            }])
            .unwrap();
        let error = run_sql(
            &scratch,
            "COPY (SELECT * FROM in) TO 'outputs/one.csv' (HEADER, DELIMITER ',');",
            &staged,
            WriteMode::Overwrite,
        )
        .unwrap_err();
        assert!(error.to_ascii_lowercase().contains("tsv"), "{error}");
    }

    #[test]
    fn cancelled_token_is_a_limit() {
        let parent = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        let input = src.path().join("in.csv");
        fs::write(&input, "x\n1\n").unwrap();
        let scratch = Scratch::create(parent.path(), "mizuchi-cancel").unwrap();
        let staged = scratch
            .stage_inputs(&[Input {
                name: "in".into(),
                path: input,
            }])
            .unwrap();
        let cancel = CancelToken::new();
        cancel.cancel();
        let inputs: Vec<StagedInput> = staged
            .iter()
            .map(|(name, relative)| StagedInput {
                name: name.clone(),
                relative: relative.display().to_string(),
            })
            .collect();
        let error = execute_with(
            "COPY (SELECT 1 AS x) TO 'outputs/one.csv' (HEADER, DELIMITER ',');",
            &inputs,
            scratch.path(),
            SessionLimits::default(),
            cancel,
            WriteMode::Overwrite,
        )
        .unwrap_err();
        assert!(
            error.contains("limit:") && error.contains("cancelled"),
            "{error}"
        );
    }

    #[test]
    fn tiny_memory_cap_fails_closed() {
        let bar = engine_bar_dir();
        let parent = tempfile::tempdir().unwrap();
        let scratch = Scratch::create(parent.path(), "mizuchi-mem").unwrap();
        let staged = scratch
            .stage_inputs(&[
                Input {
                    name: "customers".into(),
                    path: bar.join("customers.csv"),
                },
                Input {
                    name: "orders".into(),
                    path: bar.join("orders.csv"),
                },
            ])
            .unwrap();
        let inputs: Vec<StagedInput> = staged
            .iter()
            .map(|(name, relative)| StagedInput {
                name: name.clone(),
                relative: relative.display().to_string(),
            })
            .collect();
        let sql = fs::read_to_string(bar.join("join-mizuchi.json")).unwrap();
        let job: serde_json::Value = serde_json::from_str(&sql).unwrap();
        let sql = job["sql"].as_str().unwrap();
        let error = execute_with(
            sql,
            &inputs,
            scratch.path(),
            SessionLimits {
                memory_limit: Some(1),
                workers: 1,
            },
            CancelToken::new(),
            WriteMode::Overwrite,
        )
        .unwrap_err();
        assert!(
            error.contains("limit:") || error.contains("sql:"),
            "{error}"
        );
    }

    fn year_batch() -> mizuchi::Batch {
        use std::sync::Arc;
        let schema = Arc::new(mizuchi::Schema::new(vec![
            mizuchi::Field::new("year", mizuchi::DataType::Int64, true),
            mizuchi::Field::new("id", mizuchi::DataType::Int64, true),
            mizuchi::Field::new("name", mizuchi::DataType::Utf8, true),
        ]));
        let batch = mizuchi::RecordBatch::try_new(
            schema,
            vec![
                Arc::new(arrow::array::Int64Array::from(vec![
                    Some(2020),
                    Some(2020),
                    Some(2021),
                ])),
                Arc::new(arrow::array::Int64Array::from(vec![
                    Some(1),
                    Some(2),
                    Some(3),
                ])),
                Arc::new(arrow::array::StringArray::from(vec![
                    Some("a"),
                    Some("b"),
                    Some("c"),
                ])),
            ],
        )
        .unwrap();
        mizuchi::Batch::from_arrow(batch).unwrap()
    }

    #[test]
    fn partitioned_read_report_prunes() {
        let parent = tempfile::tempdir().unwrap();
        let scratch = Scratch::create(parent.path(), "mizuchi-prune").unwrap();
        let dest = scratch.inputs_dir().join("sales");
        let abs = scratch.path().canonicalize().unwrap();
        let cfg = SessionConfig {
            allowed_read_paths: vec![abs.clone()],
            allowed_write_paths: vec![abs.clone()],
            ..SessionConfig::default()
        };
        let session = Session::from_config(cfg).unwrap();
        session
            .write_parquet(
                &dest,
                &[year_batch()],
                mizuchi::ParquetWriteOptions {
                    partition_by: &[String::from("year")],
                    mode: mizuchi_write_mode(WriteMode::Overwrite),
                },
            )
            .unwrap();
        run_sql(
            &scratch,
            "COPY (SELECT id FROM sales WHERE year = 2020) TO 'outputs/ids.csv' (HEADER, DELIMITER ',');",
            &[("sales".into(), PathBuf::from("inputs/sales"))],
            WriteMode::Overwrite,
        )
        .unwrap();
        let lineage: kako_core::Lineage =
            serde_json::from_str(&fs::read_to_string(scratch.path().join("lineage.json")).unwrap())
                .unwrap();
        assert_eq!(lineage.reads.len(), 1);
        assert_eq!(lineage.reads[0].dataset, "sales");
        assert_eq!(lineage.reads[0].partitions, vec!["year=2020"]);
        assert!(lineage.reads[0].columns.contains(&"id".to_string()));
        let path = lineage.reads[0].path.as_deref().unwrap_or("");
        assert!(
            path.starts_with("inputs/"),
            "path should be scratch-relative, got {path}"
        );
    }

    #[test]
    fn partitioned_write_reports_append_and_overwrite() {
        let parent = tempfile::tempdir().unwrap();
        let scratch = Scratch::create(parent.path(), "mizuchi-write").unwrap();
        let abs = scratch.path().canonicalize().unwrap();
        let dest = scratch.outputs_dir().join("sales");
        let cfg = SessionConfig {
            allowed_read_paths: vec![abs.clone()],
            allowed_write_paths: vec![abs.clone()],
            ..SessionConfig::default()
        };
        let session = Session::from_config(cfg).unwrap();
        let batch = year_batch();
        let opts_over = mizuchi::ParquetWriteOptions {
            partition_by: &[String::from("year")],
            mode: mizuchi_write_mode(WriteMode::Overwrite),
        };
        let first = session
            .write_parquet_reported(&dest, std::slice::from_ref(&batch), opts_over)
            .unwrap();
        let mapped = lineage_from_report(&first, &abs);
        assert_eq!(mapped.writes.len(), 1);
        assert_eq!(mapped.writes[0].mode, "overwrite");
        assert_eq!(mapped.writes[0].rows, 3);
        assert_eq!(mapped.writes[0].partitions, vec!["year=2020", "year=2021"]);
        assert_eq!(mapped.writes[0].path, "outputs/sales");
        let again = session
            .write_parquet_reported(&dest, std::slice::from_ref(&batch), opts_over)
            .unwrap();
        assert_eq!(
            lineage_from_report(&first, &abs),
            lineage_from_report(&again, &abs)
        );
        let opts_append = mizuchi::ParquetWriteOptions {
            partition_by: &[String::from("year")],
            mode: mizuchi_write_mode(WriteMode::Append),
        };
        let appended = session
            .write_parquet_reported(&dest, &[batch], opts_append)
            .unwrap();
        let mapped = lineage_from_report(&appended, &abs);
        assert_eq!(mapped.writes[0].mode, "append");
        assert_eq!(mapped.writes[0].rows, 3);
        let overwrite_snap = receipt_snapshot(&lineage_from_report(&first, &abs));
        let overwrite_again = receipt_snapshot(&lineage_from_report(&again, &abs));
        assert_eq!(overwrite_snap, overwrite_again);
        assert_eq!(overwrite_snap["lineage"]["writes"][0]["mode"], "overwrite");
        let append_snap = receipt_snapshot(&mapped);
        assert_eq!(append_snap["lineage"]["writes"][0]["mode"], "append");
        assert_ne!(
            overwrite_snap["lineage"]["writes"][0]["mode"],
            append_snap["lineage"]["writes"][0]["mode"]
        );
    }
}
