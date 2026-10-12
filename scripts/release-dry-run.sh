#!/usr/bin/env bash
# Package kako release artifacts into --out and write SHA256SUMS.
#
# Always writes the runner tarball and the Python catalog tree. With
# --with-images, also builds and saves the SQL and Python container images
# when docker is on PATH.
#
# Usage:
#   bash scripts/release-dry-run.sh --out dist
#   bash scripts/release-dry-run.sh --out dist --skip-build --binary path/to/kako
#   bash scripts/release-dry-run.sh --out dist --version 0.1.0 --with-images
set -euo pipefail

root="$(cd "$(dirname "$0")/.." && pwd)"
out=""
skip_build=false
with_images=false
with_site_packages=false
binary=""
version_override=""
python_dir="$root/runtime/python"
sql_dockerfile="$root/runtime/sql/Dockerfile"

usage() {
  sed -n '2,14p' "$0" | sed 's/^# \{0,1\}//'
  exit "${1:-2}"
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --out)
      out="${2:?--out needs a directory}"
      shift 2
      ;;
    --binary)
      binary="${2:?--binary needs a path}"
      shift 2
      ;;
    --python-dir)
      python_dir="${2:?--python-dir needs a path}"
      shift 2
      ;;
    --version)
      version_override="${2:?--version needs a value}"
      shift 2
      ;;
    --skip-build) skip_build=true; shift ;;
    --with-images) with_images=true; shift ;;
    --with-site-packages) with_site_packages=true; shift ;;
    -h | --help) usage 0 ;;
    *) usage 1 ;;
  esac
done

[[ -n "$out" ]] || usage 1
mkdir -p "$out"
out="$(cd "$out" && pwd)"

cargo_version="$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)"
[[ -n "$cargo_version" ]] || {
  echo "release-dry-run: could not read workspace version" >&2
  exit 1
}
if [[ -n "$version_override" ]]; then
  if [[ "$version_override" != "$cargo_version" ]]; then
    echo "release-dry-run: --version $version_override does not match Cargo.toml $cargo_version" >&2
    exit 1
  fi
  version="$version_override"
else
  version="$cargo_version"
fi

checksum() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$@"
  else
    shasum -a 256 "$@"
  fi
}

if [[ -z "$binary" ]]; then
  if [[ "$skip_build" == true ]]; then
    echo "release-dry-run: --skip-build requires --binary" >&2
    exit 1
  fi
  (cd "$root" && cargo build --locked --release -p kako)
  binary="$root/target/release/kako"
fi
[[ -f "$binary" ]] || {
  echo "release-dry-run: binary not found: $binary" >&2
  exit 1
}

triple="$(rustc -vV | sed -n 's/^host: //p')"
[[ -n "$triple" ]] || {
  echo "release-dry-run: could not read rustc host triple" >&2
  exit 1
}

stage="$(mktemp -d "${TMPDIR:-/tmp}/kako-release.XXXXXX")"
cleanup() { rm -rf "$stage"; }
trap cleanup EXIT

mkdir -p "$stage/bin"
cp "$binary" "$stage/bin/kako"
chmod +x "$stage/bin/kako"
bin_tar="$out/kako-${version}-${triple}.tar.gz"
tar -C "$stage/bin" -czf "$bin_tar" kako

[[ -f "$python_dir/requirements.txt" && -f "$python_dir/kako_run.py" && -f "$python_dir/Dockerfile" ]] || {
  echo "release-dry-run: python dir needs requirements.txt, kako_run.py, Dockerfile" >&2
  exit 1
}
lock_hex="$(checksum "$python_dir/requirements.txt" | awk '{print $1}')"
[[ ${#lock_hex} -eq 64 ]] || {
  echo "release-dry-run: could not hash python lockfile" >&2
  exit 1
}
catalog="$stage/catalog"
mkdir -p "$catalog/$lock_hex"
cp "$python_dir/requirements.txt" "$catalog/$lock_hex/requirements.txt"
printf 'python-lockfile:sha256:%s\n' "$lock_hex" >"$catalog/current"
if [[ "$with_site_packages" == true ]]; then
  python3 -m pip install --require-hashes --no-cache-dir --disable-pip-version-check \
    -r "$python_dir/requirements.txt" --target "$catalog/$lock_hex/site-packages"
fi
py_tar="$out/kako-python-catalog.tar.gz"
tar -C "$catalog" -czf "$py_tar" current "$lock_hex"

if [[ "$with_images" == true ]]; then
  command -v docker >/dev/null 2>&1 || {
    echo "release-dry-run: --with-images requires docker" >&2
    exit 1
  }
  [[ -f "$sql_dockerfile" ]] || {
    echo "release-dry-run: missing $sql_dockerfile" >&2
    exit 1
  }
  sql_ctx="$stage/sql"
  mkdir -p "$sql_ctx"
  cp "$binary" "$sql_ctx/kako"
  docker build -f "$sql_dockerfile" -t "kako-sql:${version}" "$sql_ctx"
  docker save "kako-sql:${version}" -o "$out/kako-sql-image.tar"
  docker build -t "kako-python:${version}" "$python_dir"
  docker save "kako-python:${version}" -o "$out/kako-python-image.tar"
fi

(
  cd "$out"
  files=()
  for f in kako-*.tar.gz kako-*-image.tar; do
    [[ -e "$f" ]] || continue
    files+=("$f")
  done
  checksum "${files[@]}" >SHA256SUMS
)

echo "release-dry-run: wrote $out"
ls -1 "$out"
