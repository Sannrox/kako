//! Live Sekai adapter: join+agg against a local isolated-process plane.
//!
//! Ignored in offline `cargo test`. Enable with
//! `cargo test -p kako --features sekai -- --ignored`.
//!
//! Prerequisites:
//! - a local Sekai instance with the isolated-process profile
//! - one claimable transform: SQL join + aggregate over two datasets
//! - engine `mizuchi` (lineage on the kako receipt) or `duckdb_sql`
//! - `KAKO_SEKAI_TARGET`, `KAKO_SEKAI_NAMESPACE`, and `KAKO_SEKAI_CODE`
//!   (isolated-process JobSpec omits SQL/Python; ADR 0012)
//! - optional `KAKO_SEKAI_PRINCIPAL`, `KAKO_SEKAI_TOKEN`,
//!   `KAKO_SEKAI_ALLOW_INSECURE=1`
//! - linux_native sandbox for the claimed isolation tier

#![cfg(feature = "sekai")]

use std::path::PathBuf;

use kako::run::{JobFinish, RunnerConfig, run_source};
use kako::sekai_source::{LiveClaimTransport, SekaiSource};

#[ignore = "needs a local Sekai plane; set KAKO_SEKAI_TARGET, KAKO_SEKAI_NAMESPACE, and KAKO_SEKAI_CODE. Optional: KAKO_SEKAI_PRINCIPAL, KAKO_SEKAI_TOKEN, KAKO_SEKAI_ALLOW_INSECURE=1. The plane must expose one claimable isolated_process transform: a join and aggregate over two datasets (mizuchi for lineage)."]
#[test]
fn live_join_agg_commits_output_and_receipt_with_lineage() {
    let dir = tempfile::tempdir().unwrap();
    let transport = LiveClaimTransport::connect_from_env(dir.path().join("claim")).unwrap();
    let mut source = SekaiSource::new(transport, "kako-live");
    let config = RunnerConfig {
        worker: PathBuf::from(env!("CARGO_BIN_EXE_kako")),
        scratch_parent: dir.path().join("runs"),
        keys: None,
    };
    let finishes = run_source(&mut source, &config).unwrap();
    assert_eq!(finishes.len(), 1, "{finishes:?}");
    match &finishes[0] {
        JobFinish::Completed { .. } => {}
        other => panic!("expected completed join+agg, got {other:?}"),
    }
    let receipt = source
        .into_transport()
        .last_receipt()
        .cloned()
        .expect("write-back receipt");
    assert!(
        receipt.lineage.is_some() || receipt.outputs.len() == 1,
        "mizuchi receipts carry lineage; duckdb_sql carries the output digest: {receipt:?}"
    );
    if let Some(lineage) = &receipt.lineage {
        assert!(!lineage.reads.is_empty(), "{lineage:?}");
        assert!(!lineage.writes.is_empty(), "{lineage:?}");
    }
}
