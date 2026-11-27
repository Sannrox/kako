#!/usr/bin/env bash
# Run the sqllogictest corpus through `kako run`.
#
# Usage: run.sh <duckdb_sql|mizuchi|both> <results-dir>
# Needs kekkai linux_native and jq is not required. Exit 2 when that backend
# is absent. Writes <results-dir>/slt.json and prints the markdown summary.
set -euo pipefail

engine="${1:?usage: run.sh <duckdb_sql|mizuchi|both> <results-dir>}"
results="${2:?usage: run.sh <duckdb_sql|mizuchi|both> <results-dir>}"
case "$engine" in
  duckdb_sql | mizuchi | both) ;;
  *) echo "slt: engine must be duckdb_sql, mizuchi, or both" >&2; exit 2 ;;
esac

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../../.." && pwd)"
cd "$root"

cargo build --locked --bin kako >/dev/null
kako="$root/target/debug/kako"
if ! "$kako" doctor | grep -q 'backend_selected: linux_native'; then
  echo "slt: skip; kako run needs linux_native" >&2
  exit 2
fi

mkdir -p "$results"
results="$(cd "$results" && pwd)"
python3 "$here/harness.py" run --kako "$kako" --engine "$engine" --results "$results"
