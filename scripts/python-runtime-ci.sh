#!/usr/bin/env bash
# Build the Python runtime image twice, require matching lockfile identity,
# and run the Polars/pandas join fixtures inside that image.
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
python_dir="$root/runtime/python"
bar="$root/scripts/engine-bar"
lock="$python_dir/requirements.txt"

if [[ ! -f "$lock" ]]; then
  echo "python-runtime-ci: missing $lock" >&2
  exit 1
fi

lock_hex="$(sha256sum "$lock" | awk '{print $1}')"
expected="python-lockfile:sha256:${lock_hex}"

build() {
  local tag="$1"
  docker build --no-cache --pull=false -t "$tag" "$python_dir"
}

echo "python-runtime-ci: building kako-python:a"
build kako-python:a
echo "python-runtime-ci: building kako-python:b"
build kako-python:b

id_a="$(docker inspect -f '{{.Id}}' kako-python:a)"
id_b="$(docker inspect -f '{{.Id}}' kako-python:b)"
img_a="$(docker run --rm kako-python:a cat /opt/kako/image-id | tr -d '\n')"
img_b="$(docker run --rm kako-python:b cat /opt/kako/image-id | tr -d '\n')"

if [[ "$img_a" != "$expected" || "$img_b" != "$expected" ]]; then
  echo "python-runtime-ci: image-id mismatch" >&2
  echo "  expected $expected" >&2
  echo "  a $img_a" >&2
  echo "  b $img_b" >&2
  exit 1
fi

if [[ "$id_a" != "$id_b" ]]; then
  echo "python-runtime-ci: docker image ids differ; lockfile digest still matched" >&2
  echo "  a $id_a" >&2
  echo "  b $id_b" >&2
  echo "python-runtime-ci: comparing package freeze instead of image id"
fi

freeze_a="$(docker run --rm kako-python:a pip freeze --disable-pip-version-check)"
freeze_b="$(docker run --rm kako-python:b pip freeze --disable-pip-version-check)"
if [[ "$freeze_a" != "$freeze_b" ]]; then
  echo "python-runtime-ci: pip freeze differs across builds" >&2
  diff -u <(printf '%s\n' "$freeze_a") <(printf '%s\n' "$freeze_b") >&2 || true
  exit 1
fi

# The digest kako records is the lockfile identity; both builds must share it.
digest_a="$(printf '%s' "$img_a" | sha256sum | awk '{print $1}')"
digest_b="$(printf '%s' "$img_b" | sha256sum | awk '{print $1}')"
if [[ "$digest_a" != "$digest_b" ]]; then
  echo "python-runtime-ci: lockfile image digest differed" >&2
  exit 1
fi
echo "python-runtime-ci: digest $img_a"

run_join() {
  local src="$1"
  local scratch
  scratch="$(mktemp -d)"
  mkdir -p "$scratch/inputs" "$scratch/outputs"
  cp "$bar/customers.csv" "$scratch/inputs/customers.csv"
  cp "$bar/orders.csv" "$scratch/inputs/orders.csv"
  cp "$bar/$src" "$scratch/user.py"
  cat >"$scratch/manifest.json" <<EOF
{"engine":"python","sql":"","inputs":[{"name":"customers","relative":"inputs/customers.csv"},{"name":"orders","relative":"inputs/orders.csv"}],"outputs":["totals.csv"]}
EOF
  docker run --rm \
    -v "$scratch":/scratch \
    -w /scratch \
    kako-python:a \
    python /opt/kako/kako_run.py
  if ! grep -Eq '25(\.0)?' "$scratch/outputs/totals.csv"; then
    echo "python-runtime-ci: $src missing aggregate 25" >&2
    cat "$scratch/outputs/totals.csv" >&2 || true
    rm -rf "$scratch"
    exit 1
  fi
  if ! grep -q '7' "$scratch/outputs/totals.csv"; then
    echo "python-runtime-ci: $src missing aggregate 7" >&2
    rm -rf "$scratch"
    exit 1
  fi
  rm -rf "$scratch"
  echo "python-runtime-ci: $src ok"
}

run_join join_polars.py
run_join join_pandas.py
echo "python-runtime-ci: ok"
