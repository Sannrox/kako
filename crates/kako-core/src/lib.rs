//! Domain types, receipts, staging, and worker manifests for kako.

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::{Duration, SystemTime};

#[cfg(test)]
mod crate_graph;
pub mod digest;
pub mod fair;
pub mod isolation;
pub mod manifest;
pub mod receipt;
pub mod stage;

pub use isolation::IsolationTier;

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
    /// Python (Polars, pandas) executed in a sandboxed interpreter.
    Python,
}

/// How the mizuchi adapter commits a job's output. Omitted jobs overwrite.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WriteMode {
    /// Replace the named output.
    #[default]
    Overwrite,
    /// Append onto an existing output, or create it when missing.
    Append,
}

impl WriteMode {
    /// Job file and lineage spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Overwrite => "overwrite",
            Self::Append => "append",
        }
    }
}

impl fmt::Display for WriteMode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for WriteMode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "overwrite" => Ok(Self::Overwrite),
            "append" => Ok(Self::Append),
            other => Err(format!(
                "unknown write_mode `{other}`: expected overwrite or append"
            )),
        }
    }
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
    /// How mizuchi should commit those outputs (`overwrite` or `append`).
    /// COPY SQL overwrites a new CSV and rejects `append`. Partitioned
    /// parquet writes honor `append` through mizuchi. DuckDB and Python
    /// stay overwrite.
    pub write_mode: WriteMode,
    /// Hard wall-clock timeout for the sandboxed child.
    pub wall_clock: Duration,
    /// Optional CPU ceiling for the sandboxed child (`RLIMIT_CPU`, whole seconds).
    pub cpu_time: Option<Duration>,
    /// Optional cgroup memory cap in bytes. Fail closed when it cannot be set.
    pub memory_bytes: Option<u64>,
    /// Tenant that owns this job. Required; the local source fills a default.
    pub tenant_id: String,
    /// Namespace inside the tenant. Required; the local source fills a default.
    pub namespace: String,
    /// Pin of the Python runtime image (`python-lockfile:sha256:…`). SQL
    /// jobs leave this unset; unpinned Python jobs use the catalog default.
    pub runtime_image: Option<String>,
    /// Minimum isolation the run must enforce. Omitted jobs are `linux_native`.
    pub isolation: IsolationTier,
}

/// Key for quotas, claims, and receipts: `(tenant_id, namespace)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
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
    /// Measured CPU time from the sandbox, when reported.
    pub cpu_usec: Option<u64>,
    /// Peak memory from the sandbox cgroup, when reported.
    pub memory_peak_bytes: Option<u64>,
    /// Engine-reported reads, writes, and plan. Present for mizuchi; omitted
    /// from DuckDB receipts so v1 JSON for the default engine stays the same.
    pub lineage: Option<Lineage>,
    /// Baked runtime identity for Python jobs (`python-lockfile:sha256:…`).
    /// Omitted from SQL receipts.
    pub runtime_image: Option<String>,
    /// Isolation tier that actually ran.
    pub isolation: IsolationTier,
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

    /// Return a claimed job without `complete` or `fail`.
    ///
    /// Drain uses this so the source stays the retry authority. Sources that
    /// cannot restore the claim must return an error. Do not write outputs.
    fn release(&mut self, job: &Job) -> Result<(), Self::Error>;
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
        fn release(&mut self, _: &Job) -> Result<(), Never> {
            Ok(())
        }
    }

    #[test]
    fn empty_source_yields_no_job() {
        assert_eq!(Empty.next_job().unwrap(), None);
    }

    #[test]
    fn empty_release_is_a_noop() {
        let job = Job {
            id: "idle".into(),
            engine: Engine::DuckDbSql,
            code: "SELECT 1".into(),
            inputs: Vec::new(),
            outputs: vec!["out.csv".into()],
            write_mode: WriteMode::Overwrite,
            wall_clock: Duration::from_secs(1),
            cpu_time: None,
            memory_bytes: None,
            tenant_id: "local".into(),
            namespace: "default".into(),
            runtime_image: None,
            isolation: IsolationTier::LinuxNative,
        };
        Empty.release(&job).unwrap();
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
