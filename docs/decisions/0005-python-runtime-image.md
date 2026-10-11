# ADR 0005: Python engine uses a baked runtime image and cgroup memory

- Status: Accepted
- Date: 2026-10-11
- Supersedes: —

## Context

Python transforms (Polars, pandas, and DuckDB's Python API) are user-supplied
code. They must run inside the kekkai sandbox with no network, so packages
cannot be installed per run. Address-space caps (`RLIMIT_AS`) break numeric
libraries that reserve large virtual ranges long before they use resident
memory. The sandbox already limits memory through cgroup v2
([ADR 0002](0002-sandboxed-transform-runner.md)).

The default engine stays DuckDB SQL ([ADR 0003](0003-engine-opt-in-bar.md)).
Python is opt-in.

## Decision

1. **One versioned runtime image** ships a pinned CPython, Polars, pandas,
   pyarrow, and DuckDB, installed from a hashed lockfile
   (`runtime/python/requirements.txt`). User code cannot `pip install` or open
   sockets at run time.
2. **Image identity on the receipt.** Python receipts record
   `runtime_image` as `python-lockfile:` plus the SHA-256 of the committed
   lockfile. SQL receipts omit the field. Receipt version stays 2.
3. **Entrypoint contract.** User source defines `transform(inputs)` where
   `inputs` maps staged names to frames (Polars by default). The function
   returns a dict of output file name to frame. The worker writes those
   frames under `outputs/` as CSV, Parquet, or Arrow based on the name.
   The runner loads user source as data (`importlib` from a scratch file); it
   does not import user code as a package.
4. **Interpreter roots, not `/usr`.** The parent resolves the interpreter
   (`KAKO_PYTHON`, then a local `runtime/python/.venv` if present, then
   `python3` on `PATH` that can import Polars) and grants Landlock read on
   that prefix, `base_prefix`, and `exec_prefix`. It does not add `/usr` or
   `/sys` as extra roots. kekkai's own system grants stay in kekkai.
5. **Memory is the cgroup cap.** Python jobs use kekkai `memory_max`. An
   out-of-memory kill is `Failure::Limit`. kako still never sets `RLIMIT_AS`.
6. **The interpreter process gets an allowlisted environment.** User
   transforms do not inherit the runner's environment. `KAKO_PYTHON` and
   PATH fallbacks must import Polars, pandas, pyarrow, and DuckDB. The
   receipt `runtime_image` is the committed lockfile digest; `engine_version`
   is the interpreter that actually ran.

## Consequences

- Reproducing a Python run needs the lockfile (or an image built from it)
  in addition to the job bytes and input digests.
- Hosts without a matching interpreter fail closed before the sandbox spawn.
- Per-transform package sets and guest isolation stay later Issues.

## Alternatives considered

- **Install packages per run.** Rejected: user code has no network.
- **Keep `RLIMIT_AS` with a large value.** Rejected: it does not bound
  resident memory and still breaks numeric libraries.
- **Import user code in-process (PyO3).** Rejected: user Python is data to
  the Rust runner; the sandboxed child execs the interpreter.
- **Grant `/usr` from kako.** Rejected: extra Landlock roots are the
  interpreter prefix only.
