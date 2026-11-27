//! The sqllogictest corpus (`scripts/engine-bar/slt`) run in process.
//! Pull-request CI covers the default engine here; `slt/run.sh` runs the
//! same files through `kako run` on both engines when linux_native is present.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use kako::engine::execute_in_scratch;
use kako::manifest::write_manifest;
use kako::stage::Scratch;
use kako::{Engine, Input};
use serde_json::Value;

fn slt_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/engine-bar/slt")
}

fn plan_corpus(work: &Path) -> Value {
    let status = Command::new("python3")
        .arg(slt_dir().join("harness.py"))
        .arg("plan")
        .arg("--work")
        .arg(work)
        .status()
        .expect("python3");
    assert!(status.success(), "slt plan failed: {status}");
    serde_json::from_str(&fs::read_to_string(work.join("plan.json")).unwrap()).unwrap()
}

fn run_query(parent: &Path, work: &Path, query: &Value, engine: Engine) -> Vec<Vec<String>> {
    let id = query["id"].as_str().unwrap();
    let scratch = Scratch::create(parent, &format!("{id}-{engine:?}")).unwrap();
    let inputs: Vec<Input> = query["inputs"]
        .as_object()
        .unwrap()
        .iter()
        .map(|(name, rel)| Input::file(name.clone(), work.join(rel.as_str().unwrap())))
        .collect();
    let staged = scratch.stage_inputs(&inputs).unwrap();
    write_manifest(
        scratch.path(),
        engine,
        query["copy_sql"].as_str().unwrap(),
        &staged,
        &["result.csv".into()],
    )
    .unwrap();
    execute_in_scratch(scratch.path()).unwrap_or_else(|error| panic!("{id} {engine:?}: {error}"));
    let csv = fs::read_to_string(scratch.outputs_dir().join("result.csv")).unwrap();
    let mut rows = Vec::new();
    for (i, line) in csv.lines().enumerate() {
        if i == 0 {
            continue;
        }
        rows.push(parse_csv_line(line));
    }
    rows
}

fn parse_csv_line(line: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = line.chars().peekable();
    let mut in_quotes = false;
    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    chars.next();
                    cur.push('"');
                } else {
                    in_quotes = false;
                }
            } else {
                cur.push(c);
            }
        } else if c == '"' {
            in_quotes = true;
        } else if c == ',' {
            out.push(std::mem::take(&mut cur));
        } else {
            cur.push(c);
        }
    }
    out.push(cur);
    out
}

fn nullish(cell: &str) -> Option<&str> {
    let text = cell.trim();
    if text.is_empty() || text.eq_ignore_ascii_case("NULL") || text == r"\N" {
        None
    } else {
        Some(cell)
    }
}

fn same_cell(left: Option<&str>, right: Option<&str>) -> bool {
    match (left, right) {
        (None, None) => true,
        (Some(a), Some(b)) => match (a.parse::<f64>(), b.parse::<f64>()) {
            (Ok(x), Ok(y)) => (x - y).abs() <= 1e-6 * x.abs().max(y.abs()).max(1e-9),
            _ => a == b,
        },
        _ => false,
    }
}

fn sort_rows(rows: &mut [Vec<String>]) {
    rows.sort_by(|a, b| {
        let left: Vec<&str> = a.iter().map(|c| nullish(c).unwrap_or("")).collect();
        let right: Vec<&str> = b.iter().map(|c| nullish(c).unwrap_or("")).collect();
        left.cmp(&right)
    });
}

fn expected_rows(query: &Value) -> Vec<Vec<String>> {
    query["expected_rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| {
            row.as_array()
                .unwrap()
                .iter()
                .map(|cell| match cell {
                    Value::Null => String::new(),
                    other => other.as_str().unwrap_or("").to_string(),
                })
                .collect()
        })
        .collect()
}

fn engine_name(engine: Engine) -> &'static str {
    engine.as_str()
}

fn run_engine(plan: &Value, work: &Path, parent: &Path, engine: Engine) {
    let name = engine_name(engine);
    for query in plan["queries"].as_array().unwrap() {
        let id = query["id"].as_str().unwrap();
        let skip: Vec<&str> = query["skipif"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|v| v.as_str())
            .collect();
        if skip.contains(&name) {
            continue;
        }
        let mut got = run_query(parent, work, query, engine);
        let mut want = expected_rows(query);
        if query["sort_rows"].as_bool().unwrap_or(true) {
            sort_rows(&mut got);
            sort_rows(&mut want);
        }
        let ncols = query["types"].as_str().unwrap_or("I").len().max(1);
        assert_eq!(got.len(), want.len(), "{id} {name}: {got:?} vs {want:?}");
        for (a, b) in got.iter().zip(&want) {
            let a = pad(a, ncols);
            let b = pad(b, ncols);
            assert!(
                a.iter()
                    .zip(&b)
                    .all(|(x, y)| same_cell(nullish(x), nullish(y))),
                "{id} {name}: {a:?} vs {b:?}"
            );
        }
    }
}

fn pad(row: &[String], ncols: usize) -> Vec<String> {
    let mut out = row.to_vec();
    out.resize(ncols, String::new());
    out
}

#[test]
fn slt_corpus_runs_in_process() {
    let parent = tempfile::tempdir().unwrap();
    let work = parent.path().join("plan");
    let plan = plan_corpus(&work);
    assert!(
        !plan["queries"].as_array().unwrap().is_empty(),
        "no slt queries planned"
    );
    run_engine(&plan, &work, parent.path(), Engine::DuckDbSql);
    run_engine(&plan, &work, parent.path(), Engine::Mizuchi);
}
