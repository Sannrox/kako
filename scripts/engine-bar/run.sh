#!/usr/bin/env bash
# Run the named engine-bar fixture jobs through `kako run` and print one JSON
# object per job. Needs kekkai linux_native. Exit 2 when that backend is absent.
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
bar="$(cd "$(dirname "$0")" && pwd)"
cd "$root"

if ! command -v jq >/dev/null 2>&1; then
  echo "engine-bar: jq is required" >&2
  exit 1
fi

cargo build --locked --bin kako >/dev/null
kako="$root/target/debug/kako"

doctor="$("$kako" doctor)"
if ! grep -q 'backend_selected: linux_native' <<<"$doctor"; then
  echo "engine-bar: skip; kako run needs linux_native" >&2
  echo "$doctor" >&2
  exit 2
fi

run_job() {
  local job="$1"
  local work
  work="$(mktemp -d "${TMPDIR:-/tmp}/kako-engine-bar.XXXXXX")"
  trap 'rm -rf "$work"' RETURN
  cp "$bar/customers.csv" "$bar/orders.csv" "$bar/in.csv" "$work/"
  cp "$job" "$work/job.json"
  local out="$work/out"
  "$kako" run "$work/job.json" --output "$out"
  jq -c '{
    id: .job_id,
    engine_version: .engine_version,
    duration_ms: .duration_ms,
    outputs: .outputs
  }' "$out/receipt.json"
}

run_job "$bar/join.json"
run_job "$bar/probe.json"
