#!/usr/bin/env bash
# Run a command in a systemd-delegated cgroup v2 subtree so kekkai can
# set memory.max. The hosted runner's service cgroup is not writable.
set -euo pipefail

inside() {
  local rel parent
  rel="$(sed -n 's/^0:://p' /proc/self/cgroup)"
  rel="${rel#/}"
  if [[ "$rel" != */main ]]; then
    echo "ci-cgroup-delegate: expected DelegateSubgroup=main, got 0::$rel" >&2
    exit 1
  fi
  parent="/sys/fs/cgroup/${rel%/main}"
  if [[ ! -f "$parent/cgroup.controllers" ]]; then
    echo "ci-cgroup-delegate: $parent is not a cgroup" >&2
    exit 1
  fi
  echo '+memory +pids' >"$parent/cgroup.subtree_control"
  echo 0 >"$parent/memory.swap.max"
  export KAKO_CGROUP_ROOT="$parent"
  echo "ci-cgroup-delegate: KAKO_CGROUP_ROOT=$parent"
  exec "$@"
}

if [[ "${1:-}" == "--inside" ]]; then
  shift
  inside "$@"
fi

if [[ $# -eq 0 ]]; then
  echo "usage: ci-cgroup-delegate.sh <command>..." >&2
  exit 2
fi

if [[ "$(uname -s)" != "Linux" ]]; then
  echo "ci-cgroup-delegate: Linux only" >&2
  exit 1
fi

if [[ ! -f /sys/fs/cgroup/cgroup.controllers ]]; then
  echo "ci-cgroup-delegate: cgroup v2 is not mounted" >&2
  exit 1
fi

if [[ -n "${KAKO_CGROUP_ROOT:-}" ]]; then
  exec "$@"
fi

self="$(cd "$(dirname "$0")" && pwd)/$(basename "$0")"
unit="kako-ci-$$"
env_file="$(mktemp)"
cleanup() { rm -f "$env_file"; }
trap cleanup EXIT

python3 -c '
import os
for key, value in os.environ.items():
    if key.startswith("BASH_FUNC_") or "\n" in value or "\0" in value:
        continue
    print(f"{key}={value}")
' >"$env_file"

sudo systemd-run --quiet --collect --wait --pipe \
  --unit="$unit" \
  --uid="$(id -u)" --gid="$(id -g)" \
  --property=Delegate=yes \
  --property=DelegateSubgroup=main \
  --property="EnvironmentFile=$env_file" \
  --working-directory="$(pwd)" \
  "$self" --inside "$@"
