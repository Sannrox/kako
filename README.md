# kako (加工)

A sandboxed data-transform runner. kako takes a job (SQL or Python plus input
datasets), runs it in an isolated, short-lived child process, and returns the
outputs together with a receipt describing exactly what ran.

> Status: early skeleton. The design is proposed in
> [ADR 0002](docs/decisions/0002-sandboxed-transform-runner.md); nothing runs
> yet.

## How it works

1. **Take a job** from a [job source](#job-sources).
2. **Stage** the inputs into a per-run scratch directory.
3. **Run** the code in a [kekkai](https://github.com/Sannrox/kekkai) sandbox:
   no network, CPU, memory and wall-clock limits, writes confined to the
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

## Engines

- **SQL via DuckDB** first.
- **Python** (Polars, pandas) later. Packages are baked into the runtime
  image; user code has no network access.

## Job sources

A job source is a small trait (`JobSource` in `crates/kako/src/lib.rs`): it
hands out jobs and receives results. The source decides what runs and whether
to retry; kako only executes.

- **Local / CLI** (first-class): run a job described by a file on disk, write
  outputs and the receipt to a directory.
- **Sekai** ([sekai-chisei](https://github.com/Sannrox/sekai-chisei)): claim
  transform runs from the Sekai control plane. One adapter among others.

## Receipts

Every run produces a receipt with:

- the digest of each input;
- the digest of the code;
- the engine and its version;
- the digest of each output;
- start time and duration.

A receipt is enough to check what produced an output and to rerun it.

## Non-goals

- Scheduling, triggers, or a dependency graph between transforms. That is the
  job source's or an orchestrator's job.
- Being a distributed engine. Single-node execution first.
- Interactive authoring or notebooks.
- Network access for user code.

## Development

```bash
cargo build
cargo test
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

The toolchain is pinned in `rust-toolchain.toml`.

## Security

kako runs untrusted code. Report sandbox escapes privately; see
[SECURITY.md](SECURITY.md).

## License

Apache-2.0. See [LICENSE](LICENSE).
