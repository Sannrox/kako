//! The scale-factor engine-bar SQL (`scripts/engine-bar/sf`) at the tiny
//! scale, run in process on both engines. Keeps the harness valid without a
//! sandbox; timings come from `run.sh` on a sandboxed host.

use std::fs;
use std::path::{Path, PathBuf};

use kako::engine::execute_in_scratch;
use kako::manifest::write_manifest;
use kako::stage::Scratch;
use kako::{Engine, Input};

const TABLES: [&str; 6] = [
    "customer",
    "orders",
    "lineitem",
    "item",
    "date_dim",
    "store_sales",
];

fn harness() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/engine-bar/sf")
}

/// Generate the tiny tables (generator scale 1) into `dir`.
fn generate(parent: &Path, dir: &Path) {
    let sql = fs::read_to_string(harness().join("generate.sql"))
        .unwrap()
        .replace("{S}", "1");
    let scratch = Scratch::create(parent, "generate").unwrap();
    let outputs: Vec<String> = TABLES.iter().map(|t| format!("{t}.parquet")).collect();
    write_manifest(scratch.path(), Engine::DuckDbSql, &sql, &[], &outputs).unwrap();
    execute_in_scratch(scratch.path()).unwrap();
    for output in &outputs {
        fs::copy(scratch.outputs_dir().join(output), dir.join(output)).unwrap();
    }
}

fn run(parent: &Path, data: &Path, query: &str, sql: &str, engine: Engine) -> Vec<Vec<String>> {
    let scratch = Scratch::create(parent, &format!("{query}-{engine:?}")).unwrap();
    let inputs: Vec<Input> = TABLES
        .iter()
        .filter(|table| sql.contains(*table))
        .map(|table| Input {
            name: table.to_string(),
            path: data.join(format!("{table}.parquet")),
            markings: Vec::new(),
            column_markings: Default::default(),
        })
        .collect();
    let staged = scratch.stage_inputs(&inputs).unwrap();
    write_manifest(scratch.path(), engine, sql, &staged, &["result.csv".into()]).unwrap();
    execute_in_scratch(scratch.path())
        .unwrap_or_else(|error| panic!("{query} {engine:?}: {error}"));
    fs::read_to_string(scratch.outputs_dir().join("result.csv"))
        .unwrap()
        .lines()
        .map(|line| line.split(',').map(str::to_owned).collect())
        .collect()
}

fn same_cell(a: &str, b: &str) -> bool {
    match (a.parse::<f64>(), b.parse::<f64>()) {
        (Ok(x), Ok(y)) => (x - y).abs() <= 1e-6 * x.abs().max(y.abs()).max(1e-3),
        _ => a == b,
    }
}

#[test]
fn harness_queries_agree_on_both_engines_at_tiny_scale() {
    let parent = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    generate(parent.path(), data.path());
    let mut queries: Vec<_> = fs::read_dir(harness().join("queries"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    queries.sort();
    assert_eq!(queries.len(), 4);
    for path in queries {
        let query = path.file_stem().unwrap().to_string_lossy().into_owned();
        let sql = fs::read_to_string(&path).unwrap();
        let current = run(parent.path(), data.path(), &query, &sql, Engine::DuckDbSql);
        let candidate = run(parent.path(), data.path(), &query, &sql, Engine::Mizuchi);
        assert!(current.len() > 1, "{query} returned no rows: {current:?}");
        assert_eq!(
            current.len(),
            candidate.len(),
            "{query}: {current:?} vs {candidate:?}"
        );
        for (a, b) in current.iter().zip(&candidate) {
            assert!(
                a.len() == b.len() && a.iter().zip(b).all(|(x, y)| same_cell(x, y)),
                "{query}: {a:?} vs {b:?}"
            );
        }
    }
}
