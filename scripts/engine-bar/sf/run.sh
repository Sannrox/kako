#!/usr/bin/env bash
# Scale-factor engine bar. Generate the decision-support tables at one scale,
# run every query under queries/ through `kako run` on duckdb_sql and on
# mizuchi with the same sandbox limits, and write the measurements.
#
# Usage: run.sh <tiny|1|10> <results-dir>
# Writes <results-dir>/runs.jsonl (one object per query and engine),
# <results-dir>/host.json, and each engine's result CSV. Needs kekkai
# linux_native and jq; exits 2 when that backend is absent. Summarize with
# report.py.
set -euo pipefail

scale="${1:?usage: run.sh <tiny|1|10> <results-dir>}"
results="${2:?usage: run.sh <tiny|1|10> <results-dir>}"
case "$scale" in
  tiny) s=1 ;;
  1) s=125000 ;;
  10) s=1250000 ;;
  *) echo "engine-bar: scale must be tiny, 1 or 10" >&2; exit 2 ;;
esac

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../../.." && pwd)"
# Both engines get the same outer limits; each engine also runs one worker
# with a 256 MiB internal memory cap.
timeout_ms=1800000
memory_bytes=$((4 * 1024 * 1024 * 1024))

command -v jq >/dev/null || { echo "engine-bar: jq is required" >&2; exit 1; }
(cd "$root" && cargo build --locked --release --bin kako >/dev/null)
kako="$root/target/release/kako"
if ! "$kako" doctor | grep -q 'backend_selected: linux_native'; then
  echo "engine-bar: skip; kako run needs linux_native" >&2
  exit 2
fi

mkdir -p "$results"
results="$(cd "$results" && pwd)"
work="$(mktemp -d "${TMPDIR:-/tmp}/kako-engine-bar-sf.$$.XXXXXX")"
trap 'rm -rf "$work"' EXIT

jq -n \
  --arg scale "$scale" \
  --argjson cpus "$(getconf _NPROCESSORS_ONLN)" \
  --argjson mem_kib "$(awk '/MemTotal/ {print $2}' /proc/meminfo)" \
  --arg kernel "$(uname -sr)" \
  --argjson memory_bytes "$memory_bytes" \
  --argjson timeout_ms "$timeout_ms" \
  '{scale: $scale, cpus: $cpus, memory_bytes_host: ($mem_kib * 1024), kernel: $kernel,
    job_memory_bytes: $memory_bytes, job_timeout_ms: $timeout_ms}' \
  >"$results/host.json"

tables=(customer orders lineitem item date_dim store_sales)
jq -n \
  --arg sql "$(sed "s/{S}/$s/g" "$here/generate.sql")" \
  --argjson timeout_ms "$timeout_ms" \
  '{id: "generate", sql: $sql, inputs: {}, timeout_ms: $timeout_ms,
    outputs: ($ARGS.positional | map(. + ".parquet"))}' \
  --args "${tables[@]}" >"$work/generate.json"
echo "engine-bar: generating scale $scale" >&2
"$kako" run "$work/generate.json" --output "$work/data"

: >"$results/runs.jsonl"
for sql_file in "$here"/queries/*.sql; do
  query="$(basename "$sql_file" .sql)"
  sql="$(cat "$sql_file")"
  inputs="{}"
  for table in "${tables[@]}"; do
    if grep -qw "$table" "$sql_file"; then
      inputs="$(jq -c --arg t "$table" '. + {($t): ("data/" + $t + ".parquet")}' <<<"$inputs")"
    fi
  done
  for engine in duckdb_sql mizuchi; do
    id="$query-$engine"
    jq -n --arg id "$id" --arg engine "$engine" --arg sql "$sql" --argjson inputs "$inputs" \
      --argjson timeout_ms "$timeout_ms" --argjson memory_bytes "$memory_bytes" \
      '{id: $id, engine: $engine, sql: $sql, inputs: $inputs, outputs: ["result.csv"],
        timeout_ms: $timeout_ms, memory_bytes: $memory_bytes}' >"$work/$id.json"
    echo "engine-bar: $id" >&2
    out="$work/out-$id"
    if "$kako" run "$work/$id.json" --output "$out" 2>"$work/$id.err"; then
      cp "$out/result.csv" "$results/$id.csv"
      jq -c --arg scale "$scale" --arg query "$query" --arg engine "$engine" \
        '{scale: $scale, query: $query, engine: $engine, status: "ok",
          engine_version, duration_ms, cpu_usec, memory_peak_bytes}' \
        "$out/receipt.json" >>"$results/runs.jsonl"
    else
      jq -nc --arg scale "$scale" --arg query "$query" --arg engine "$engine" \
        --arg error "$(tail -c 400 "$work/$id.err")" \
        '{scale: $scale, query: $query, engine: $engine, status: "failed", error: $error}' \
        >>"$results/runs.jsonl"
    fi
    rm -rf "$out"
  done
done
