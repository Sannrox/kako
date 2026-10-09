//! kako (加工): a sandboxed data-transform runner.
//!
//! kako takes a transform job (SQL or Python plus input datasets) from a
//! pluggable [`JobSource`], runs it in a sandboxed child process, and hands the
//! outputs and a [`Receipt`] back to the source.
//!
//! This is a skeleton: it defines the core vocabulary only. See
//! `docs/decisions/0002-sandboxed-transform-runner.md`.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

/// A content digest, written as `<algorithm>:<hex>` (for example SHA-256).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Digest(pub String);

/// The engine that executes a job's code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    /// SQL executed by DuckDB.
    DuckDbSql,
    /// Python (Polars, pandas) executed in a sandboxed interpreter. Planned.
    Python,
}

/// One named input dataset, already available as a local file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Input {
    pub name: String,
    pub path: PathBuf,
}

/// A unit of work: code to run against named inputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Job {
    /// Identifier assigned by the job source.
    pub id: String,
    pub engine: Engine,
    /// SQL text or Python source.
    pub code: String,
    pub inputs: Vec<Input>,
    /// Names of the outputs the job is expected to produce.
    pub outputs: Vec<String>,
}

/// One produced output file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Output {
    pub name: String,
    pub path: PathBuf,
    pub digest: Digest,
}

/// Evidence of what ran: enough to reproduce or audit a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Receipt {
    pub job_id: String,
    /// Digest of each input, by input name.
    pub inputs: Vec<(String, Digest)>,
    pub code: Digest,
    /// Engine name and version, for example `duckdb 1.4.0`.
    pub engine_version: String,
    pub outputs: Vec<(String, Digest)>,
    pub started_at: SystemTime,
    pub duration: Duration,
}

/// Why a job did not produce outputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Failure {
    /// The job's code failed (non-zero exit, SQL error).
    Job(String),
    /// A sandbox or resource limit stopped the job.
    Limit(String),
    /// The runner itself could not run the job.
    Runner(String),
}

/// Where jobs come from and where results go.
///
/// Adapters implement this for a local directory or CLI, a queue, or a
/// control plane. The source decides what runs; the runner only executes.
pub trait JobSource {
    type Error: std::error::Error;

    /// Take the next job, or `None` when there is nothing to do.
    fn next_job(&mut self) -> Result<Option<Job>, Self::Error>;

    /// Report a successful run with its outputs and receipt.
    fn complete(
        &mut self,
        job: &Job,
        outputs: &[Output],
        receipt: &Receipt,
    ) -> Result<(), Self::Error>;

    /// Report a failed run.
    fn fail(&mut self, job: &Job, failure: &Failure) -> Result<(), Self::Error>;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Empty;

    #[derive(Debug)]
    struct Never;
    impl std::fmt::Display for Never {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("never")
        }
    }
    impl std::error::Error for Never {}

    impl JobSource for Empty {
        type Error = Never;
        fn next_job(&mut self) -> Result<Option<Job>, Never> {
            Ok(None)
        }
        fn complete(&mut self, _: &Job, _: &[Output], _: &Receipt) -> Result<(), Never> {
            Ok(())
        }
        fn fail(&mut self, _: &Job, _: &Failure) -> Result<(), Never> {
            Ok(())
        }
    }

    #[test]
    fn empty_source_yields_no_job() {
        assert_eq!(Empty.next_job().unwrap(), None);
    }
}
