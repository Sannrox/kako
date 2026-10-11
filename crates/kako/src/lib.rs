//! kako (加工): a sandboxed data-transform runner.
//!
//! kako takes a transform job (SQL or Python plus input datasets) from a
//! pluggable [`JobSource`], runs it in a sandboxed child process, and hands the
//! outputs and a [`Receipt`] back to the source.

use std::fmt;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

pub mod digest;
pub mod engine;
mod engine_mizuchi;
pub mod local;
pub mod receipt;
pub mod run;
pub mod sandbox;
pub mod stage;

/// A content digest, written as `<algorithm>:<hex>` (for example SHA-256).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Digest(pub String);

/// The engine that executes a job's code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    /// SQL executed by DuckDB. Default.
    DuckDbSql,
    /// SQL executed by the opt-in mizuchi engine.
    Mizuchi,
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
    /// Hard wall-clock timeout for the sandboxed child.
    pub wall_clock: Duration,
    /// Tenant that owns this job. Required; the local source fills a default.
    pub tenant_id: String,
    /// Namespace inside the tenant. Required; the local source fills a default.
    pub namespace: String,
}

/// Key for quotas, claims, and receipts: `(tenant_id, namespace)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct QuotaKey {
    pub tenant_id: String,
    pub namespace: String,
}

impl QuotaKey {
    pub fn new(tenant_id: impl Into<String>, namespace: impl Into<String>) -> Result<Self, String> {
        let tenant_id = tenant_id.into();
        let namespace = namespace.into();
        if tenant_id.is_empty() {
            return Err("tenant_id is required".into());
        }
        if namespace.is_empty() {
            return Err("namespace is required".into());
        }
        Ok(Self {
            tenant_id,
            namespace,
        })
    }

    pub fn from_job(job: &Job) -> Result<Self, String> {
        Self::new(&job.tenant_id, &job.namespace)
    }
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
    pub tenant_id: String,
    pub namespace: String,
    /// Engine-reported reads, writes, and plan. Present for mizuchi; omitted
    /// from DuckDB receipts so v1 JSON for the default engine stays the same.
    pub lineage: Option<Lineage>,
}

/// Stable record of what one run read and wrote, for receipts.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Lineage {
    /// Executed plan text from the engine.
    pub plan: String,
    pub reads: Vec<LineageRead>,
    pub writes: Vec<LineageWrite>,
}

/// One input scan after projection and partition prune.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LineageRead {
    pub dataset: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub partitions: Vec<String>,
    pub columns: Vec<String>,
}

/// One output write after the adapter (or engine) committed files.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LineageWrite {
    pub path: String,
    pub partitions: Vec<String>,
    pub mode: String,
    pub rows: u64,
    pub bytes: u64,
    pub commit_id: String,
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

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Job(message) => write!(f, "job: {message}"),
            Self::Limit(message) => write!(f, "limit: {message}"),
            Self::Runner(message) => write!(f, "runner: {message}"),
        }
    }
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

    #[test]
    fn quota_key_is_tenant_and_namespace() {
        let key = QuotaKey::new("t", "ns").unwrap();
        assert_eq!(key.tenant_id, "t");
        assert_eq!(key.namespace, "ns");
        assert!(QuotaKey::new("", "ns").unwrap_err().contains("tenant_id"));
        assert!(QuotaKey::new("t", "").unwrap_err().contains("namespace"));
    }
}
