#!/usr/bin/env bash
# Measure DuckDB and Polars join+agg duration on the shipped execute path.
# Prints the test's JSON report. Needs the Python runtime for the Polars rows.
#
# Usage:
#   bash scripts/single-node-ceiling.sh
#   KAKO_CEILING_CUSTOMERS=2000,20000,100000 bash scripts/single-node-ceiling.sh
set -euo pipefail
root="$(cd "$(dirname "$0")/.." && pwd)"
cd "$root"
export KAKO_CEILING_CUSTOMERS="${KAKO_CEILING_CUSTOMERS:-2000,20000,100000}"
exec cargo test --locked --lib join_agg_ceiling_records_duration -- --nocapture
