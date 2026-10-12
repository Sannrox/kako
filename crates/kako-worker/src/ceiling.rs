//! Measure DuckDB and Polars join+agg time on `execute_in_scratch`.
//!
//! That is the shipped engine worker, not a full sandboxed `kako run`.
//! Default size is small so `cargo test` stays cheap. Operators record a
//! larger sweep with `KAKO_CEILING_CUSTOMERS=2000,20000,100000`.

use std::fs;
use std::path::Path;
use std::time::Instant;

use kako_core::digest;
use kako_core::manifest::write_manifest;
use kako_core::stage::Scratch;
use kako_core::{Engine, Input};

use crate::execute_in_scratch;

const JOIN_SQL: &str = "COPY (
  SELECT c.id AS customer_id, c.name, sum(o.amount) AS total
  FROM customers c
  JOIN orders o ON o.customer_id = c.id
  GROUP BY c.id, c.name
  ORDER BY c.id
) TO 'outputs/totals.csv' (HEADER, DELIMITER ',');";

const JOIN_POLARS: &str = r#"
import polars as pl

def transform(inputs):
    customers = inputs["customers"]
    orders = inputs["orders"]
    totals = (
        orders.join(customers, left_on="customer_id", right_on="id")
        .group_by(["customer_id", "name"])
        .agg(total=pl.col("amount").sum())
        .sort("customer_id")
    )
    return {"totals.csv": totals}
"#;

fn write_fixture(dir: &Path, customers: u32) {
    let mut customers_csv = String::from("id,name\n");
    let mut orders_csv = String::from("customer_id,amount\n");
    for id in 1..=customers {
        customers_csv.push_str(&format!("{id},c{id}\n"));
        orders_csv.push_str(&format!("{id},1\n{id},2\n"));
    }
    fs::write(dir.join("customers.csv"), customers_csv).unwrap();
    fs::write(dir.join("orders.csv"), orders_csv).unwrap();
}

struct Measurement {
    engine: &'static str,
    customers: u32,
    duration_ms: u128,
    totals_sha256: String,
    output_rows: usize,
}

fn count_data_rows(csv: &str) -> usize {
    csv.lines().skip(1).filter(|line| !line.is_empty()).count()
}

fn measure(engine: Engine, customers: u32, src: &Path) -> Measurement {
    let parent = tempfile::tempdir().unwrap();
    let scratch = Scratch::create(parent.path(), "ceiling").unwrap();
    let staged = scratch
        .stage_inputs(&[
            Input {
                name: "customers".into(),
                path: src.join("customers.csv"),
            },
            Input {
                name: "orders".into(),
                path: src.join("orders.csv"),
            },
        ])
        .unwrap();
    let code = match engine {
        Engine::DuckDbSql => JOIN_SQL,
        Engine::Python => JOIN_POLARS,
        Engine::Mizuchi => panic!("ceiling measures DuckDB and Python only"),
    };
    write_manifest(
        scratch.path(),
        engine,
        code,
        &staged,
        &["totals.csv".into()],
    )
    .unwrap();
    let started = Instant::now();
    execute_in_scratch(scratch.path()).unwrap();
    let duration_ms = started.elapsed().as_millis();
    let csv = fs::read_to_string(scratch.outputs_dir().join("totals.csv")).unwrap();
    let totals_sha256 = digest::hash_bytes(csv.as_bytes()).0;
    Measurement {
        engine: match engine {
            Engine::DuckDbSql => "duckdb_sql",
            Engine::Python => "python",
            Engine::Mizuchi => "mizuchi",
        },
        customers,
        duration_ms,
        totals_sha256,
        output_rows: count_data_rows(&csv),
    }
}

fn customer_sizes() -> Vec<u32> {
    match std::env::var("KAKO_CEILING_CUSTOMERS") {
        Ok(raw) => raw
            .split(',')
            .map(|part| part.trim().parse::<u32>().expect("KAKO_CEILING_CUSTOMERS"))
            .collect(),
        Err(_) => vec![2_000],
    }
}

#[test]
fn join_agg_ceiling_records_duration_and_matching_outputs() {
    let sizes = customer_sizes();
    let mut report = Vec::new();
    for customers in sizes {
        let src = tempfile::tempdir().unwrap();
        write_fixture(src.path(), customers);
        let duck = measure(Engine::DuckDbSql, customers, src.path());
        assert_eq!(duck.output_rows, customers as usize, "duckdb row count");
        if kako_engine_python::resolve_interpreter().is_ok() {
            let python = measure(Engine::Python, customers, src.path());
            assert_eq!(python.output_rows, customers as usize, "python row count");
            assert_eq!(
                duck.totals_sha256, python.totals_sha256,
                "DuckDB and Polars totals must match at {customers} customers"
            );
            report.push(python);
        }
        report.push(duck);
    }
    let json = serde_json::to_string_pretty(
        &report
            .iter()
            .map(|row| {
                serde_json::json!({
                    "engine": row.engine,
                    "customers": row.customers,
                    "orders": row.customers * 2,
                    "duration_ms": row.duration_ms,
                    "output_rows": row.output_rows,
                    "totals": row.totals_sha256,
                })
            })
            .collect::<Vec<_>>(),
    )
    .unwrap();
    println!("{json}");
    assert!(json.contains("duration_ms"), "{json}");
    assert!(json.contains("duckdb_sql"), "{json}");
}
