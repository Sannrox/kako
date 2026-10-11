//! Opt-in mizuchi SQL worker used by the sandboxed child.

use std::fs::File;
use std::path::{Path, PathBuf};

use arrow::csv::WriterBuilder;
use mizuchi::{
    CancelToken, CsvReadOptions, Error, ParquetReadOptions, Session, SessionConfig, VERSION,
};

use crate::engine::StagedInput;

/// Run staged SQL through mizuchi. Returns `mizuchi <version>`.
pub(crate) fn execute(sql: &str, inputs: &[StagedInput], scratch: &Path) -> Result<String, String> {
    execute_with(
        sql,
        inputs,
        scratch,
        SessionLimits::default(),
        CancelToken::new(),
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
    let (query, dest) = copy_select_to(sql)?;
    let dest_abs = scratch_abs.join(&dest);
    if !dest_abs.starts_with(scratch_abs.join("outputs")) {
        return Err(format!(
            "output `{}` must be under outputs/",
            dest.display()
        ));
    }
    let batches = session.execute_sql(&query).map_err(map_error)?;
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
    Ok(format!("mizuchi {VERSION}"))
}

fn register_input(session: &mut Session, name: &str, path: &Path) -> Result<(), String> {
    let lower = path
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match lower.as_str() {
        "parquet" => session
            .register_parquet(name, path, ParquetReadOptions::default())
            .map_err(map_error),
        "tsv" => Err("mizuchi adapter does not read TSV inputs; convert to CSV or parquet".into()),
        _ => session
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
            .map_err(map_error),
    }
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
    use crate::engine::{execute_in_scratch, write_manifest};
    use crate::stage::Scratch;
    use crate::{Engine, Input};
    use std::fs;

    fn engine_bar_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/engine-bar")
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
        write_manifest(scratch.path(), Engine::Mizuchi, &sql, &staged).unwrap();
        let version = execute_in_scratch(scratch.path()).unwrap();
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
        write_manifest(
            scratch.path(),
            Engine::Mizuchi,
            "COPY (SELECT * FROM in) TO 'outputs/one.csv' (HEADER, DELIMITER ',');",
            &staged,
        )
        .unwrap();
        let error = execute_in_scratch(scratch.path()).unwrap_err();
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
        )
        .unwrap_err();
        assert!(
            error.contains("limit:") || error.contains("sql:"),
            "{error}"
        );
    }
}
