# kako (加工)

A sandboxed data-transform runner. kako takes a job (SQL or Python plus input
datasets), runs it in an isolated, short-lived child process, and returns the
outputs together with a receipt describing exactly what ran.

> Status: SQL MVP plus opt-in Python. Local jobs run DuckDB (default) or
> Python in a [kekkai](https://github.com/Sannrox/kekkai) sandbox and write
> outputs plus a receipt. External job sources are not implemented yet.
> See [ADR 0002](docs/decisions/0002-sandboxed-transform-runner.md),
> [ADR 0005](docs/decisions/0005-python-runtime-image.md), and
> [ADR 0006](docs/decisions/0006-python-package-policy.md).

## How it works

1. **Take a job** from a [job source](#job-sources).
2. **Stage** the inputs into a per-run scratch directory.
3. **Run** the code in a [kekkai](https://github.com/Sannrox/kekkai) sandbox:
   no network, CPU and wall-clock limits, writes confined to the
   scratch directory.
4. **Hash** the outputs and build a receipt.
5. **Report** the outputs and receipt back to the job source.
6. **Clean up** the scratch directory and the process.

```text
  ┌──────────────┐  next_job   ┌──────────────────────────────┐
  │  job source  │────────────►│  kako runner                 │
  │ (local, CLI, │             │   scratch dir ─► sandboxed   │
  │  queue, ...) │◄────────────│   DuckDB / Python process    │
  └──────────────┘  complete / └──────────────────────────────┘
                    fail + receipt
```

## CLI

```bash
kako doctor
kako run job.json --output ./out
kako run job.json --tenant local --namespace default
kako run job.json --require-identity
```

`kako doctor` prints host capabilities. `kako run` uses kekkai `linux_native`
(Landlock filesystem allowlist and seccomp network deny). If that backend
cannot be applied, the run fails closed.

A job file is JSON:

```json
{
  "id": "join-agg",
  "sql": "COPY (\n  SELECT c.id, sum(o.amount) AS total\n  FROM customers c\n  JOIN orders o ON o.customer_id = c.id\n  GROUP BY c.id\n  ORDER BY c.id\n) TO 'outputs/totals.csv' (HEADER, DELIMITER ',');",
  "inputs": {
    "customers": "customers.csv",
    "orders": "orders.csv"
  },
  "outputs": ["totals.csv"],
  "timeout_ms": 60000,
  "tenant_id": "local",
  "namespace": "default"
}
```

Input paths are relative to the job file. Each input is staged as a DuckDB
view with that name. User SQL writes files under `outputs/`. kako copies those
files and `receipt.json` into `--output` (default `./out`).

Every job, receipt, and quota key carries `tenant_id` and `namespace`. The
local CLI fills `local` / `default` when the job file omits them, or the
values from `--tenant` / `--namespace`. `--require-identity` is the
multi-tenant mode: both fields must be present on the job file.

The job source owns queueing, quotas, and fair share
([ADR 0004](docs/decisions/0004-quota-authority.md)). kako enforces the
per-run `timeout_ms`, optional `cpu_time_secs`, and optional `memory_bytes`
it is given. A memory cap fails closed when the host cannot apply a
cgroup. Receipts record measured `cpu_usec` and `memory_peak_bytes` when
the sandbox reports them.

## Engines

- **SQL via DuckDB** first (default).
- **SQL via mizuchi** opt-in: set `"engine": "mizuchi"` on the job. Default
  jobs and tests stay on DuckDB. Mizuchi receipts add `lineage` (plan, reads,
  writes); DuckDB receipts stay the same.
- **Python** opt-in: set `"engine": "python"` and `"code"` with
  `def transform(inputs)` returning output frames. Packages (Polars, pandas,
  pyarrow, DuckDB) come from the hashed lockfile in `runtime/python/`; user
  code has no network. Jobs may pin `runtime_image` to that lockfile
  digest so a later default-image upgrade does not change output
  ([ADR 0006](docs/decisions/0006-python-package-policy.md)). Receipts
  record the image that ran. Operators keep prior images in
  `KAKO_PYTHON_IMAGES`. Set `KAKO_PYTHON` when the interpreter is not on
  `PATH`.

Adding or switching an engine is opt-in behind the bar in
[docs/engine-requirements.md](docs/engine-requirements.md)
([ADR 0003](docs/decisions/0003-engine-opt-in-bar.md)).

## Job sources

A job source is a small trait (`JobSource` in `crates/kako/src/lib.rs`): it
hands out jobs and receives results. The source decides what runs and whether
to retry; kako only executes.

- **Local / CLI** (first-class): run a job described by a file on disk, write
  outputs and the receipt to a directory. One-shot; it does not schedule.
- **Fair share**: `FairSource` yields jobs round-robin by quota key so one
  owner's flood does not occupy every slot. Other adapters may reuse it.
- **Sekai** ([sekai-chisei](https://github.com/Sannrox/sekai-chisei)): claim
  transform runs from the Sekai control plane. One adapter among others.

## Receipts

Every run produces a receipt with:

- `tenant_id` and `namespace`;
- measured `cpu_usec` and `memory_peak_bytes` when the sandbox reports them;
- the digest of each input;
- the digest of the code;
- the engine and its version;
- `runtime_image` for Python jobs (pinned lockfile digest);
- the digest of each output;
- start time and duration.

A receipt is enough to check what produced an output and to rerun it.

## Non-goals

- Scheduling, triggers, or a dependency graph between transforms. That is the
  job source's or an orchestrator's job.
- Being a distributed engine. Single-node execution first; larger
  datasets are split by the job source
  ([ADR 0007](docs/decisions/0007-single-node-execution.md)).
- Interactive authoring or notebooks.
- Network access for user code.

## Development

```bash
cargo build
cargo test
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
KAKO_CEILING_CUSTOMERS=2000,20000,100000 bash scripts/single-node-ceiling.sh
```

The toolchain is pinned in `rust-toolchain.toml`.

## Security

kako runs untrusted code. Report sandbox escapes privately; see
[SECURITY.md](SECURITY.md).

## License

Apache-2.0. See [LICENSE](LICENSE).
