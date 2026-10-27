# kako (加工)

A sandboxed data-transform runner. kako takes a job (SQL or Python plus input
datasets), runs it in an isolated, short-lived child process, and returns the
outputs together with a receipt describing exactly what ran.

> Status: SQL MVP plus opt-in Python, and streaming jobs v1. Local jobs run
> DuckDB (default), opt-in mizuchi, or Python in a
> [kekkai](https://github.com/Sannrox/kekkai) sandbox and write outputs plus
> a receipt. Streaming jobs (`kako.streaming-job/v1`) read a partitioned
> topic, run a mizuchi plan, and apply objects with checkpoints tied to
> receipts ([streaming-jobs.md](docs/streaming-jobs.md),
> [ADR 0017](docs/decisions/0017-streaming-checkpoints.md)). `kako serve`
> runs a pool process with health, drain, and rollback onto the previous
> GitHub Release. The optional Sekai adapter claims isolated-process work
> over the published runtime-claim client.
> See [ADR 0002](docs/decisions/0002-sandboxed-transform-runner.md),
> [ADR 0005](docs/decisions/0005-python-runtime-image.md),
> [ADR 0006](docs/decisions/0006-python-package-policy.md),
> [ADR 0008](docs/decisions/0008-release-artifacts.md),
> [ADR 0009](docs/decisions/0009-isolation-tier.md),
> [ADR 0010](docs/decisions/0010-crate-graph.md), and
> [ADR 0011](docs/decisions/0011-runner-pool-drain.md).

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
kako validate job.json
kako preview job.json --rows 100
kako replay replay.json --output ./out
kako replay replay.json --check
kako serve --listen <addr>
kako serve --listen <addr> job.json --output ./out
kako serve --listen <addr> --hand-back
```

`kako doctor` prints host capabilities. `kako run` uses kekkai `linux_native`
(Landlock filesystem allowlist and seccomp network deny) unless the job
sets a stronger `isolation` floor. `userspace` requires Linux, `linux_native`
health, and the `kekkai-guest` helper on `PATH` or `KEKKAI_GUEST_HELPER`.
If the required tier cannot be applied, the run fails closed. There is no
silent downgrade.

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
  "namespace": "default",
  "isolation": "linux_native"
}
```

`isolation` is a minimum tier (`linux_native`, `userspace`, or `kvm`).
Omitted means `linux_native`. The receipt records the tier that ran.

Optional `input_markings`, `input_column_markings`, `output_markings`, and
`declassify` are opaque labels from the job source. The runner refuses an
under-marked output unless `declassify` lists the dropped labels. See
[ADR 0014](docs/decisions/0014-job-markings.md).

When a job claim carries a wrapped tenant key, the runner unwraps it in
process and seals published outputs as `KAKOSEAL`. Key bytes never enter
the scratch tree, the worker manifest, or the sandbox environment. A
revoked key fails the job. See
[ADR 0015](docs/decisions/0015-job-encryption.md).

Input paths are relative to the job file. Each input is staged as a DuckDB
view with that name. User SQL writes files under `outputs/`. kako copies those
files and `receipt.json` into `--output` (default `./out`).

Every job, receipt, and quota key carries `tenant_id` and `namespace`. The
local CLI fills `local` / `default` when the job file omits them, or the
values from `--tenant` / `--namespace`. `--require-identity` is the
multi-tenant mode: both fields must be present on the job file.

`kako validate` parses a `duckdb_sql` job file and runs the SQL in the
sandbox over empty inputs that keep the column types a full run infers.
Unknown columns, bad join keys and type errors fail with the
engine's message and `LINE` location before any run.

`kako preview` runs a `duckdb_sql` job with each input loaded as a table of
its first `--rows` rows (default 100, at most 10000) and prints JSON: each
declared output's columns and rows as text, plus a receipt under
`"committing": false`. Outputs are read as Parquet when they carry the
Parquet magic bytes, otherwise as headered delimited text.

In both commands SQL reads inputs only through their named tables; a direct
scan of a staged file under `inputs/` is refused rather than read past the
sample. Neither command writes an output directory or calls a job source,
and the scratch tree is removed. Both refuse other engines and encrypted
jobs, whose sealed outputs must not leave as plaintext rows.

`kako replay` re-runs a pipeline's steps exactly as an earlier run did,
into an isolated branch ([ADR 0016](docs/decisions/0016-replay-jobs.md)):

```json
{
  "id": "replay-7",
  "branch": "replay/7",
  "steps": [
    {
      "id": "totals",
      "job": "totals.json",
      "pins": {
        "inputs": {"orders": "sha256:…"},
        "code": "sha256:…",
        "engine_version": "duckdb 1.4.5",
        "outputs": {"totals.csv": "sha256:…"}
      }
    }
  ]
}
```

Pins come from the original receipts: each input digest, the code digest
(the transform version), and the engine version. A Python step's job must
pin `runtime_image`. `outputs` is optional and holds the original output
digests. Before anything runs, every step is checked. A changed input or
code, or an engine or runtime image this runner does not have, makes the
step not reproducible. The replay then runs nothing and reports every such
step. It never substitutes another version. Otherwise the steps run in
order. Each step writes its outputs and receipt to
`<output>/<branch>/<step>/`, with `/` in the branch written as `%2F`
(`replay%2F7/totals/`). The run is re-checked against the pins, and one
replay at a time holds a branch.
`replay.json` in the branch directory says `"complete": false` while the
replay runs, so an interrupted rerun never reads as an earlier success.
When complete, it gives each step's status:
`reproduced`, `diverged` (an output digest differs from its pin), `ran`
(no output pins), `not_reproducible`, `failed`, or `skipped`. The branch
may not be `main` or `master`, and no part of its directory may be a
link. `--check` only reports. Steps do not feed
each other: the job source stages every step's inputs at their pinned
versions. Computing diffs is the job source's job.

`kako serve` claims from the job source, serves `GET /health`, `GET /ready`,
and `GET /version`, and drains on SIGTERM: readiness goes false immediately,
health stays true while an in-flight job still runs, then the process exits.
Ready means the process will claim work. A local job file is the one-shot
source (`kako run` uses the same file); after that job finishes the process
goes not-ready. Omit the file and it stays not-ready. `--hand-back` returns an in-flight job with `JobSource::release`
instead of finishing it. kako does not retry; the source does
([ADR 0011](docs/decisions/0011-runner-pool-drain.md)). Live Sekai claiming
stays behind the `sekai` feature and a later plane-proof issue.

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
  writes). Optional `write_mode` is `overwrite` (default) or `append`. COPY
  SQL writes a new CSV and rejects `append`; partitioned parquet writes pass
  the mode to mizuchi. Lineage records the mode that ran. DuckDB receipts
  stay the same.
- **Python** opt-in: set `"engine": "python"` and `"code"` with
  `def transform(inputs)` returning output frames. Packages (Polars, pandas,
  pyarrow, DuckDB, ONNX Runtime) come from the hashed lockfile in `runtime/python/`; user
  code has no network. Jobs may pin `runtime_image` to that lockfile
  digest so a later default-image upgrade does not change output
  ([ADR 0006](docs/decisions/0006-python-package-policy.md)). Receipts
  record the image that ran. Operators keep prior images in
  `KAKO_PYTHON_IMAGES`. Set `KAKO_PYTHON` when the interpreter is not on
  `PATH`. CI sets `KAKO_REQUIRE_PYTHON=1` so Python engine tests fail
  instead of skip, delegates a cgroup subtree for memory jobs, and
  builds the runtime image from the lockfile.

- **Scoring** from a model registry: a job with a `scoring` section names a
  signed batch descriptor, the registry keys it trusts and the `.onnx`
  artifact. Kako verifies the signatures and the artifact digest, then
  scores the one input with ONNX Runtime in the Python sandbox. The receipt
  records the model version ([scoring](docs/scoring.md)).

Adding or switching an engine is opt-in behind the bar in
[docs/engine-requirements.md](docs/engine-requirements.md)
([ADR 0003](docs/decisions/0003-engine-opt-in-bar.md)).

## Job sources

A job source is a small trait (`JobSource` in `kako-core`, re-exported as
`kako::JobSource`): it
hands out jobs and receives results. The source decides what runs and whether
to retry; kako only executes.

- **Local / CLI** (first-class): run a job described by a file on disk, write
  outputs and the receipt to a directory. One-shot; it does not schedule.
- **Fair share**: `FairSource` yields jobs round-robin by quota key so one
  owner's flood does not occupy every slot. Other adapters may reuse it.
- **Sekai** ([sekai-chisei](https://github.com/Sannrox/sekai-chisei)): optional
  `kako-source-sekai` adapter (`sekai` feature on `kako`) claims isolated-process
  transform runs from the control plane. Fence loss aborts write-back. Drain
  can hand a claim back without writing. Live proof against a plane is a later
  issue; this crate's default tests use a fake claim client. The same crate
  runs streaming jobs under the claim fence
  ([streaming-jobs.md](docs/streaming-jobs.md)).

## Receipts

Every run produces a receipt with:

- `tenant_id` and `namespace`;
- measured `cpu_usec` and `memory_peak_bytes` when the sandbox reports them;
- the digest of each input;
- the digest of the code;
- the engine and its version;
- `runtime_image` for Python jobs (pinned lockfile digest);
- `model` for scoring jobs: the registry model version, artifact digest
  and descriptor that scored the input;
- the digest of each output;
- start time and duration.

A receipt is enough to check what produced an output and to rerun it.
`kako_core::governed` maps it onto the Sekai isolated-process JobSpec and
run receipt
([ADR 0012](docs/decisions/0012-governed-receipt-mapping.md),
[governed-receipt.md](docs/governed-receipt.md)).

## Non-goals

- Scheduling, triggers, or a dependency graph between transforms. That is the
  job source's or an orchestrator's job.
- Being a distributed engine. Single-node execution first; larger
  datasets are split by the job source
  ([ADR 0007](docs/decisions/0007-single-node-execution.md)).
- Interactive authoring or notebooks.
- Network access for user code.

## Releases

A `v*` tag runs `.github/workflows/release.yml` and publishes a GitHub
Release: the Linux runner binary, SQL and Python runtime images, a Python
catalog archive that unpacks into `KAKO_PYTHON_IMAGES`, `SHA256SUMS`, and
artifact attestations. The tag version must match `Cargo.toml`, and
`CHANGELOG.md` must have a `## [<version>]` section, which becomes the
Release body. Release prep drafts it with
`bash scripts/changelog-draft.sh <version>` and commits it; pull requests do
not edit `CHANGELOG.md`. Crates stay unpublished while they depend on git pins
([ADR 0008](docs/decisions/0008-release-artifacts.md)).

```bash
bash scripts/release-dry-run.sh --out dist
bash scripts/release-dry-run.sh --out dist --with-images --with-site-packages
```

`--with-images` needs a container runtime. `--with-site-packages` installs
the hashed lockfile into the catalog tree. `workflow_dispatch` and pull
requests package artifacts without attesting or creating a GitHub Release.
Attestations and the Release are tag-only (`v*`).

## Runner pool

A pool is N long-running kako processes, each claiming independently from a
job source through `kako::pool::serve_source`. This repository does not pick
a supervisor; a process manager, container orchestrator, or
[Tenkai](https://github.com/Sannrox/tenkai) can watch `/health` and `/ready`.
`kako serve` with a local job file claims that one-shot source, then goes
not-ready. Without a job file it stays not-ready and only serves probes
and drain. Live Sekai claiming is a later issue.

Drain and replace one process at a time:

1. Send SIGTERM. Readiness goes false; the process stops claiming.
2. Wait until the process exits (health stays true while the current job
   still runs). With `--hand-back`, that job returns to the source without
   outputs.
3. Start a replacement process with the same flags.

To roll back, drain as above and start the previous `v*` GitHub Release
binary instead of the newer one. The job source remains the retry authority.

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
