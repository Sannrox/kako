//! Take one job, stage, sandbox, hash, report, clean up.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use crate::crypto::{KeyProvider, require_regular_output, seal_output, unwrap_job_dek};
use crate::digest::{hash_bytes, hash_file};
use crate::sandbox;
use crate::stage::Scratch;
use crate::{Engine, Failure, Job, JobSource, Output, QuotaKey, Receipt};

/// How the runner finds a worker binary and where it puts scratch trees.
#[derive(Clone)]
pub struct RunnerConfig {
    /// kako binary that implements `--internal-execute`.
    pub worker: PathBuf,
    /// Parent directory for per-run scratch trees.
    pub scratch_parent: PathBuf,
    /// Provider that unwraps a [`crate::JobKeyClaim`]. Held in the runner
    /// process; never copied into the scratch tree or sandbox environment.
    pub keys: Option<Arc<dyn KeyProvider + Send + Sync>>,
}

impl std::fmt::Debug for RunnerConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunnerConfig")
            .field("worker", &self.worker)
            .field("scratch_parent", &self.scratch_parent)
            .field("keys", &self.keys.as_ref().map(|_| "set"))
            .finish()
    }
}

impl RunnerConfig {
    pub fn for_current_exe(scratch_parent: PathBuf) -> Result<Self, Failure> {
        let worker = std::env::current_exe()
            .map_err(|error| Failure::Runner(format!("current_exe: {error}")))?;
        Ok(Self {
            worker,
            scratch_parent,
            keys: None,
        })
    }
}

/// Outcome of one job after the source has been notified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobFinish {
    Completed {
        job_id: String,
    },
    Failed {
        job_id: String,
        failure: Failure,
    },
    /// Drain handed the job back; the source was not told complete or fail.
    Released {
        job_id: String,
    },
}

/// Pull jobs until the source is empty. Scratch trees are removed after
/// `complete` or `fail` returns.
pub fn run_source<S: JobSource>(
    source: &mut S,
    config: &RunnerConfig,
) -> Result<Vec<JobFinish>, S::Error> {
    let mut finishes = Vec::new();
    while let Some(job) = source.next_job()? {
        match execute_job(&job, config) {
            Ok(completed) => {
                source.complete(&job, &completed.outputs, &completed.receipt)?;
                finishes.push(JobFinish::Completed {
                    job_id: job.id.clone(),
                });
            }
            Err(failure) => {
                source.fail(&job, &failure)?;
                finishes.push(JobFinish::Failed {
                    job_id: job.id.clone(),
                    failure,
                });
            }
        }
    }
    Ok(finishes)
}

/// Successful execute that still owns the scratch tree until the source copies outputs.
pub struct CompletedRun {
    pub outputs: Vec<Output>,
    pub receipt: Receipt,
    _scratch: Scratch,
}

/// Stage, sandbox, hash. The scratch tree lives in the returned value so
/// `JobSource::complete` can copy outputs before cleanup.
pub fn execute_job(job: &Job, config: &RunnerConfig) -> Result<CompletedRun, Failure> {
    execute(job, config, None)
}

/// Largest `sample_rows` a preview accepts.
pub const MAX_PREVIEW_ROWS: u64 = 10_000;

/// Non-committing preview of a job: the same sandboxed run over inputs capped
/// at `sample_rows`, returning each output's columns and rows. Nothing is
/// handed to a job source and the scratch tree is removed on return.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Preview {
    pub sample_rows: u64,
    pub receipt: Receipt,
    pub outputs: Vec<PreviewOutput>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PreviewOutput {
    pub name: String,
    pub columns: Vec<PreviewColumn>,
    /// Values rendered as text; `None` is SQL NULL.
    pub rows: Vec<Vec<Option<String>>>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PreviewColumn {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: String,
}

impl Preview {
    /// Preview JSON: the receipt is marked `"committing": false`.
    pub fn to_json(&self) -> Result<String, String> {
        let receipt: serde_json::Value =
            serde_json::from_str(&crate::receipt::to_json(&self.receipt)?)
                .map_err(|error| format!("receipt json: {error}"))?;
        let body = serde_json::json!({
            "committing": false,
            "sample_rows": self.sample_rows,
            "receipt": receipt,
            "outputs": self.outputs,
        });
        serde_json::to_string_pretty(&body).map_err(|error| format!("preview json: {error}"))
    }
}

/// Run `job` as a preview. Only `duckdb_sql` jobs preview; encrypted jobs are
/// refused because sampled plaintext rows would bypass output sealing.
pub fn preview_job(job: &Job, config: &RunnerConfig, sample_rows: u64) -> Result<Preview, Failure> {
    if sample_rows > MAX_PREVIEW_ROWS {
        return Err(Failure::Runner(format!(
            "sample_rows {sample_rows} exceeds {MAX_PREVIEW_ROWS}"
        )));
    }
    let run = sampled_run(job, config, sample_rows, true)?;
    let raw = fs::read_to_string(run._scratch.path().join(crate::manifest::PREVIEW_FILE))
        .map_err(|error| Failure::Runner(format!("read preview: {error}")))?;
    #[derive(serde::Deserialize)]
    struct File {
        outputs: Vec<PreviewOutput>,
    }
    let file: File = serde_json::from_str(&raw)
        .map_err(|error| Failure::Runner(format!("parse preview: {error}")))?;
    Ok(Preview {
        sample_rows,
        receipt: run.receipt,
        outputs: file.outputs,
    })
}

/// Check a job before a run: the same sandboxed dry run as [`preview_job`]
/// over empty inputs typed as a full run types them, so unknown columns, bad join
/// keys and type errors surface as binder errors with line locations. Writes
/// nothing outside scratch.
pub fn validate_job(job: &Job, config: &RunnerConfig) -> Result<(), Failure> {
    sampled_run(job, config, 0, false).map(|_| ())
}

/// Sandboxed run over each input's first `rows` rows. Staged input files
/// cannot be read directly, so SQL sees only the sample.
fn sampled_run(
    job: &Job,
    config: &RunnerConfig,
    rows: u64,
    capture: bool,
) -> Result<CompletedRun, Failure> {
    if job.engine != Engine::DuckDbSql {
        return Err(Failure::Runner(format!(
            "preview and validate support duckdb_sql jobs, not `{}`",
            job.engine.as_str()
        )));
    }
    if job.encryption.is_some() {
        return Err(Failure::Runner(
            "preview refuses encrypted jobs: sampled rows would bypass output sealing".into(),
        ));
    }
    execute(job, config, Some((rows, capture)))
}

fn execute(
    job: &Job,
    config: &RunnerConfig,
    sample: Option<(u64, bool)>,
) -> Result<CompletedRun, Failure> {
    QuotaKey::from_job(job).map_err(Failure::Runner)?;
    if !matches!(job.engine, crate::Engine::Python) && job.runtime_image.is_some() {
        return Err(Failure::Runner(
            "runtime_image is only valid for python jobs".into(),
        ));
    }
    let python_image = match job.engine {
        crate::Engine::DuckDbSql | crate::Engine::Mizuchi => None,
        crate::Engine::Python => Some(
            kako_engine_python::ImageCatalog::from_env()
                .select(job.runtime_image.as_deref())
                .map_err(Failure::Runner)?,
        ),
    };
    let python = match job.engine {
        crate::Engine::DuckDbSql | crate::Engine::Mizuchi => None,
        crate::Engine::Python => {
            Some(kako_engine_python::resolve_interpreter().map_err(Failure::Runner)?)
        }
    };
    let started_at = SystemTime::now();
    let started = Instant::now();
    let job_dek = match job.encryption.as_ref() {
        Some(claim) => {
            let provider = config
                .keys
                .as_ref()
                .ok_or_else(|| Failure::Runner("encrypted job requires a key provider".into()))?;
            Some((claim, unwrap_job_dek(provider, job, claim)?))
        }
        None => None,
    };
    let scratch = Scratch::create(&config.scratch_parent, &job.id).map_err(Failure::Runner)?;
    let staged = scratch.stage_inputs(&job.inputs).map_err(Failure::Runner)?;
    let mut input_digests = Vec::new();
    for (name, relative) in &staged {
        let staged_path = scratch.path().join(relative);
        input_digests.push((
            name.clone(),
            hash_file(&staged_path).map_err(Failure::Runner)?,
        ));
    }
    let code_digest = hash_bytes(job.code.as_bytes());
    if job.engine != crate::Engine::Mizuchi {
        kako_core::marking::refuse_under_marked(job, &kako_core::marking::input_union(job))
            .map_err(Failure::Runner)?;
    }
    match sample {
        Some((rows, capture)) => {
            kako_core::manifest::write_preview_manifest(scratch.path(), job, &staged, rows, capture)
        }
        None => kako_core::manifest::write_manifest_from_job(scratch.path(), job, &staged),
    }
    .map_err(Failure::Runner)?;
    if let Some(site) = python_image
        .as_ref()
        .and_then(|image| image.site_packages.as_ref())
    {
        kako_engine_python::write_python_site(scratch.path(), site).map_err(Failure::Runner)?;
    }

    let isolation = sandbox::select_backend_for(job.isolation)?;
    let cpu_time_secs = Some(cpu_limit_secs(job)?);
    let worker = sandbox::stage_worker(&config.worker, scratch.path(), isolation)?;
    let mut command = Command::new(&worker);
    command.arg("--internal-execute").arg(scratch.path());
    let mut extra_readable = if let Some(ref interpreter) = python {
        command.env("KAKO_PYTHON", interpreter);
        command.env("PYTHONNOUSERSITE", "1");
        command.env("PYTHONDONTWRITEBYTECODE", "1");
        kako_engine_python::python_readable_roots(interpreter).map_err(Failure::Runner)?
    } else {
        Vec::new()
    };
    if let Some(site) = python_image
        .as_ref()
        .and_then(|image| image.site_packages.as_ref())
    {
        extra_readable.push(site.clone());
    }
    if sandbox::host_worker_readable(isolation) {
        extra_readable.push(config.worker.clone());
    }
    let (status, stdout, stderr, usage) = sandbox::run_sandboxed(
        command,
        scratch.path(),
        isolation,
        job.wall_clock,
        cpu_time_secs,
        job.memory_bytes,
        &extra_readable,
    )?;
    if let Some(failure) = classify_worker(status, &stdout, &stderr, usage.oom_killed) {
        return Err(failure);
    }
    let engine_version = String::from_utf8_lossy(&stdout).trim().to_string();
    if engine_version.is_empty() {
        return Err(Failure::Runner("worker printed no engine version".into()));
    }

    let mut outputs = Vec::new();
    let mut output_digests = Vec::new();
    for name in &job.outputs {
        let path = scratch.outputs_dir().join(name);
        require_regular_output(&path, name)?;
        if let Some((claim, dek)) = job_dek.as_ref() {
            seal_output(&path, dek, claim, &job.id)?;
        }
        let digest = hash_file(&path).map_err(Failure::Runner)?;
        output_digests.push((name.clone(), digest.clone()));
        outputs.push(Output {
            name: name.clone(),
            path,
            digest,
        });
    }
    let lineage = load_lineage(job.engine, scratch.path()).map_err(Failure::Runner)?;
    let required = if job.engine == crate::Engine::Mizuchi {
        kako_core::marking::sql_union(job, lineage.as_ref()).map_err(Failure::Runner)?
    } else {
        kako_core::marking::input_union(job)
    };
    kako_core::marking::refuse_under_marked(job, &required).map_err(Failure::Runner)?;
    let receipt = Receipt {
        job_id: job.id.clone(),
        inputs: input_digests,
        code: code_digest,
        engine_version,
        outputs: output_digests,
        started_at,
        duration: started.elapsed(),
        tenant_id: job.tenant_id.clone(),
        namespace: job.namespace.clone(),
        cpu_usec: usage.cpu_usec,
        memory_peak_bytes: usage.memory_peak_bytes,
        lineage,
        runtime_image: python_image.map(|image| image.id),
        isolation,
        credential_scope: None,
        input_markings: kako_core::marking::input_receipt_map(job),
        output_markings: kako_core::marking::output_receipt_map(job),
        declassify: job.declassify.clone(),
        key_ids: job
            .encryption
            .as_ref()
            .map(|claim| vec![claim.dek_id.clone(), claim.kek_id.clone()])
            .unwrap_or_default(),
        key_state: job.encryption.as_ref().map(|_| "ready".to_string()),
    };
    Ok(CompletedRun {
        outputs,
        receipt,
        _scratch: scratch,
    })
}

fn load_lineage(
    engine: Engine,
    scratch: &std::path::Path,
) -> Result<Option<crate::Lineage>, String> {
    if engine != Engine::Mizuchi {
        return Ok(None);
    }
    let raw = fs::read_to_string(scratch.join("lineage.json"))
        .map_err(|error| format!("read lineage.json: {error}"))?;
    serde_json::from_str(&raw)
        .map(Some)
        .map_err(|error| format!("parse lineage.json: {error}"))
}

/// Default wall-clock timeout when a job file omits `timeout_ms`.
pub const DEFAULT_WALL_CLOCK: Duration = Duration::from_secs(60);

/// Integer seconds for `RLIMIT_CPU`. The sandbox API is whole seconds.
fn cpu_limit_secs(job: &Job) -> Result<u64, Failure> {
    let cpu = match job.cpu_time {
        Some(cpu) if cpu.subsec_nanos() != 0 => {
            return Err(Failure::Runner(
                "cpu_time must be a whole number of seconds".into(),
            ));
        }
        Some(cpu) => cpu,
        None => job.wall_clock.saturating_mul(2),
    };
    Ok(cpu.as_secs().max(1))
}

fn classify_worker(
    status: std::process::ExitStatus,
    stdout: &[u8],
    stderr: &[u8],
    oom_killed: bool,
) -> Option<Failure> {
    let err = String::from_utf8_lossy(stderr);
    let out = String::from_utf8_lossy(stdout);
    if oom_killed {
        return Some(Failure::Limit(format!(
            "memory: cgroup oom: worker exited {status}: {err}{out}"
        )));
    }
    if resource_limit_signal(status) {
        return Some(Failure::Limit(format!(
            "limit: worker exited {status}: {err}{out}"
        )));
    }
    if status.success() {
        return None;
    }
    let body = format!("worker exited {status}: {err}{out}");
    if status.code() == Some(3) || err.contains("limit:") || out.contains("limit:") {
        Some(Failure::Limit(body))
    } else {
        Some(Failure::Job(body))
    }
}

/// kekkai sets `RLIMIT_CPU` soft and hard to the same value, so a CPU cap
/// on Linux is SIGKILL. SIGXCPU is the soft-limit signal when they differ.
/// The guest helper rewrites payload signals as `128 + sig` exit codes.
fn resource_limit_signal(status: std::process::ExitStatus) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if matches!(status.signal(), Some(libc::SIGXCPU | libc::SIGKILL)) {
            return true;
        }
        matches!(
            status.code(),
            Some(code)
                if code == 128 + libc::SIGKILL || code == 128 + libc::SIGXCPU
        )
    }
    #[cfg(not(unix))]
    {
        let _ = status;
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Engine, Input};
    use std::fs;

    struct Fake {
        jobs: Vec<Job>,
        fails: Vec<Failure>,
    }

    #[derive(Debug)]
    struct FakeErr(String);
    impl std::fmt::Display for FakeErr {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.0)
        }
    }
    impl std::error::Error for FakeErr {}

    impl JobSource for Fake {
        type Error = FakeErr;
        fn next_job(&mut self) -> Result<Option<Job>, Self::Error> {
            if self.jobs.is_empty() {
                Ok(None)
            } else {
                Ok(Some(self.jobs.remove(0)))
            }
        }
        fn complete(
            &mut self,
            _: &Job,
            _: &[crate::Output],
            _: &Receipt,
        ) -> Result<(), Self::Error> {
            Ok(())
        }
        fn fail(&mut self, _: &Job, failure: &Failure) -> Result<(), Self::Error> {
            self.fails.push(failure.clone());
            Ok(())
        }
        fn release(&mut self, job: &Job) -> Result<(), Self::Error> {
            self.jobs.insert(0, job.clone());
            Ok(())
        }
    }

    #[test]
    fn missing_input_fails_and_notifies_the_source() {
        let parent = tempfile::tempdir().unwrap();
        let job = Job {
            id: "missing".into(),
            engine: Engine::DuckDbSql,
            code: "SELECT 1".into(),
            inputs: vec![Input {
                name: "gone".into(),
                path: parent.path().join("nope.csv"),
                markings: Vec::new(),
                column_markings: Default::default(),
            }],
            outputs: vec!["out.csv".into()],
            wall_clock: Duration::from_secs(5),
            cpu_time: None,
            memory_bytes: None,
            tenant_id: "local".into(),
            namespace: "default".into(),
            runtime_image: None,
            write_mode: crate::WriteMode::Overwrite,
            isolation: crate::IsolationTier::LinuxNative,
            output_markings: Default::default(),
            declassify: None,
            encryption: None,
        };
        let mut source = Fake {
            jobs: vec![job],
            fails: Vec::new(),
        };
        let config = RunnerConfig {
            worker: PathBuf::from("/bin/false"),
            scratch_parent: parent.path().to_path_buf(),
            keys: None,
        };
        let finishes = run_source(&mut source, &config).unwrap();
        assert!(
            matches!(&finishes[0], JobFinish::Failed { failure: Failure::Runner(msg), .. } if msg.contains("gone")),
            "{finishes:?}"
        );
        assert_eq!(source.fails.len(), 1);
        let leftovers: Vec<_> = fs::read_dir(parent.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("kako-run-"))
            .collect();
        assert!(leftovers.is_empty(), "scratch leftover: {leftovers:?}");
    }

    #[test]
    fn under_marked_output_is_refused_before_the_worker() {
        let parent = tempfile::tempdir().unwrap();
        let input = parent.path().join("orders.csv");
        fs::write(&input, "id,ssn\n1,x\n").unwrap();
        let job = Job {
            id: "marks".into(),
            engine: Engine::DuckDbSql,
            code: "SELECT 1".into(),
            inputs: vec![Input {
                name: "orders".into(),
                path: input,
                markings: vec!["pii".into()],
                column_markings: Default::default(),
            }],
            outputs: vec!["out.csv".into()],
            wall_clock: Duration::from_secs(5),
            cpu_time: None,
            memory_bytes: None,
            tenant_id: "local".into(),
            namespace: "default".into(),
            runtime_image: None,
            write_mode: crate::WriteMode::Overwrite,
            isolation: crate::IsolationTier::LinuxNative,
            output_markings: Default::default(),
            declassify: None,
            encryption: None,
        };
        let mut source = Fake {
            jobs: vec![job],
            fails: Vec::new(),
        };
        let config = RunnerConfig {
            worker: PathBuf::from("/bin/false"),
            scratch_parent: parent.path().to_path_buf(),
            keys: None,
        };
        let finishes = run_source(&mut source, &config).unwrap();
        assert!(
            matches!(
                &finishes[0],
                JobFinish::Failed {
                    failure: Failure::Runner(msg),
                    ..
                } if msg.contains("pii") && msg.contains("out.csv")
            ),
            "{finishes:?}"
        );
    }

    #[test]
    fn declassified_job_is_not_refused_before_the_worker() {
        let parent = tempfile::tempdir().unwrap();
        let input = parent.path().join("orders.csv");
        fs::write(&input, "id\n1\n").unwrap();
        let mut output_markings = std::collections::BTreeMap::new();
        output_markings.insert("out.csv".into(), vec!["restricted".into()]);
        let job = Job {
            id: "dec".into(),
            engine: Engine::DuckDbSql,
            code: "SELECT 1".into(),
            inputs: vec![Input {
                name: "orders".into(),
                path: input,
                markings: vec!["pii".into(), "restricted".into()],
                column_markings: Default::default(),
            }],
            outputs: vec!["out.csv".into()],
            wall_clock: Duration::from_secs(5),
            cpu_time: None,
            memory_bytes: None,
            tenant_id: "local".into(),
            namespace: "default".into(),
            runtime_image: None,
            write_mode: crate::WriteMode::Overwrite,
            isolation: crate::IsolationTier::LinuxNative,
            output_markings,
            declassify: Some(crate::DeclassifyDecision {
                id: "dec-1".into(),
                from: vec!["pii".into()],
                reason: "public aggregate".into(),
            }),
            encryption: None,
        };
        let mut source = Fake {
            jobs: vec![job],
            fails: Vec::new(),
        };
        let config = RunnerConfig {
            worker: PathBuf::from("/bin/false"),
            scratch_parent: parent.path().to_path_buf(),
            keys: None,
        };
        let finishes = run_source(&mut source, &config).unwrap();
        assert!(
            matches!(
                &finishes[0],
                JobFinish::Failed {
                    failure: Failure::Job(msg) | Failure::Runner(msg),
                    ..
                } if !msg.contains("missing marking")
            ),
            "declassify must pass the pre-sandbox refuse so the worker runs: {finishes:?}"
        );
    }

    #[test]
    fn userspace_floor_fails_closed_before_scratch() {
        if sandbox::select_backend_for(crate::IsolationTier::Userspace).is_ok() {
            return;
        }
        let parent = tempfile::tempdir().unwrap();
        let job = Job {
            id: "guest".into(),
            engine: Engine::DuckDbSql,
            code: "SELECT 1".into(),
            inputs: vec![],
            outputs: vec!["out.csv".into()],
            wall_clock: Duration::from_secs(5),
            cpu_time: None,
            memory_bytes: None,
            tenant_id: "local".into(),
            namespace: "default".into(),
            runtime_image: None,
            write_mode: crate::WriteMode::Overwrite,
            isolation: crate::IsolationTier::Userspace,
            output_markings: Default::default(),
            declassify: None,
            encryption: None,
        };
        match execute_job(
            &job,
            &RunnerConfig {
                worker: PathBuf::from("/bin/false"),
                scratch_parent: parent.path().to_path_buf(),
                keys: None,
            },
        ) {
            Err(Failure::Runner(message)) => {
                assert!(message.contains("userspace"), "{message}");
            }
            Err(other) => panic!("expected runner failure, got {other}"),
            Ok(_) => panic!("userspace floor must fail closed when guest health fails"),
        }
        let leftovers: Vec<_> = fs::read_dir(parent.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("kako-run-"))
            .collect();
        assert!(leftovers.is_empty(), "scratch leftover: {leftovers:?}");
    }

    #[test]
    fn unknown_python_image_pin_fails_before_sandbox() {
        let parent = tempfile::tempdir().unwrap();
        let pin = format!("python-lockfile:sha256:{}", "ab".repeat(32));
        let job = Job {
            id: "pin".into(),
            engine: Engine::Python,
            code: "def transform(inputs):\n    return {}\n".into(),
            inputs: vec![],
            outputs: vec!["out.csv".into()],
            wall_clock: Duration::from_secs(5),
            cpu_time: None,
            memory_bytes: None,
            tenant_id: "local".into(),
            namespace: "default".into(),
            runtime_image: Some(pin),
            write_mode: crate::WriteMode::Overwrite,
            isolation: crate::IsolationTier::LinuxNative,
            output_markings: Default::default(),
            declassify: None,
            encryption: None,
        };
        match execute_job(
            &job,
            &RunnerConfig {
                worker: PathBuf::from("/bin/false"),
                scratch_parent: parent.path().to_path_buf(),
                keys: None,
            },
        ) {
            Err(Failure::Runner(message)) => {
                assert!(message.contains("not in the image catalog"), "{message}");
            }
            Err(other) => panic!("expected runner failure, got {other}"),
            Ok(_) => panic!("unknown python image pin must fail closed"),
        }
    }

    #[test]
    fn missing_tenant_is_a_runner_failure() {
        let parent = tempfile::tempdir().unwrap();
        let job = Job {
            id: "no-tenant".into(),
            engine: Engine::DuckDbSql,
            code: "SELECT 1".into(),
            inputs: vec![],
            outputs: vec!["out.csv".into()],
            wall_clock: Duration::from_secs(5),
            cpu_time: None,
            memory_bytes: None,
            tenant_id: String::new(),
            namespace: "default".into(),
            runtime_image: None,
            write_mode: crate::WriteMode::Overwrite,
            isolation: crate::IsolationTier::LinuxNative,
            output_markings: Default::default(),
            declassify: None,
            encryption: None,
        };
        let mut source = Fake {
            jobs: vec![job],
            fails: Vec::new(),
        };
        let config = RunnerConfig {
            worker: PathBuf::from("/bin/false"),
            scratch_parent: parent.path().to_path_buf(),
            keys: None,
        };
        let finishes = run_source(&mut source, &config).unwrap();
        assert!(
            matches!(
                &finishes[0],
                JobFinish::Failed {
                    failure: Failure::Runner(msg),
                    ..
                } if msg.contains("tenant_id")
            ),
            "{finishes:?}"
        );
    }

    #[test]
    fn duckdb_jobs_ignore_planted_lineage_file() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(
            dir.path().join("lineage.json"),
            r#"{"plan":"planted","reads":[],"writes":[]}"#,
        )
        .unwrap();
        assert_eq!(load_lineage(Engine::DuckDbSql, dir.path()).unwrap(), None);
        let lineage = load_lineage(Engine::Mizuchi, dir.path()).unwrap().unwrap();
        assert_eq!(lineage.plan, "planted");
    }

    fn sample_job(id: &str, tenant: &str) -> Job {
        Job {
            id: id.into(),
            engine: Engine::DuckDbSql,
            code: "SELECT 1".into(),
            inputs: vec![],
            outputs: vec!["out.csv".into()],
            wall_clock: Duration::from_secs(5),
            cpu_time: None,
            memory_bytes: None,
            tenant_id: tenant.into(),
            namespace: "default".into(),
            runtime_image: None,
            write_mode: crate::WriteMode::Overwrite,
            isolation: crate::IsolationTier::LinuxNative,
            output_markings: Default::default(),
            declassify: None,
            encryption: None,
        }
    }

    /// Job the run loop will start; missing input fails inside `execute_job`
    /// before the sandbox so the load test measures scheduling, not worker time.
    fn queued_job(id: &str, tenant: &str) -> Job {
        Job {
            id: id.into(),
            engine: Engine::DuckDbSql,
            code: "SELECT 1".into(),
            inputs: vec![Input {
                name: "gone".into(),
                path: PathBuf::from("/nonexistent/kako-fair-missing.csv"),
                markings: Vec::new(),
                column_markings: Default::default(),
            }],
            outputs: vec!["out.csv".into()],
            wall_clock: Duration::from_secs(5),
            cpu_time: None,
            memory_bytes: None,
            tenant_id: tenant.into(),
            namespace: "default".into(),
            runtime_image: None,
            write_mode: crate::WriteMode::Overwrite,
            isolation: crate::IsolationTier::LinuxNative,
            output_markings: Default::default(),
            declassify: None,
            encryption: None,
        }
    }

    /// Records when `run_source` pulls each job, which is when that run starts.
    struct RecordingSource {
        inner: crate::fair::FairSource,
        started: Vec<String>,
        started_at: Vec<(String, Instant)>,
    }

    impl JobSource for RecordingSource {
        type Error = std::convert::Infallible;
        fn next_job(&mut self) -> Result<Option<Job>, Self::Error> {
            let job = self.inner.next_job()?;
            if let Some(ref job) = job {
                self.started.push(job.tenant_id.clone());
                self.started_at.push((job.id.clone(), Instant::now()));
            }
            Ok(job)
        }
        fn complete(
            &mut self,
            job: &Job,
            outputs: &[crate::Output],
            receipt: &Receipt,
        ) -> Result<(), Self::Error> {
            self.inner.complete(job, outputs, receipt)
        }
        fn fail(&mut self, job: &Job, failure: &Failure) -> Result<(), Self::Error> {
            self.inner.fail(job, failure)
        }
        fn release(&mut self, job: &Job) -> Result<(), Self::Error> {
            self.inner.release(job)
        }
    }

    #[test]
    fn cpu_limit_secs_rejects_fractional_seconds() {
        let mut job = sample_job("c", "t");
        job.cpu_time = Some(Duration::from_millis(1500));
        let err = cpu_limit_secs(&job).unwrap_err();
        assert!(
            matches!(err, Failure::Runner(ref message) if message.contains("whole number")),
            "{err}"
        );
        job.cpu_time = Some(Duration::from_secs(3));
        assert_eq!(cpu_limit_secs(&job).unwrap(), 3);
        job.cpu_time = None;
        job.wall_clock = Duration::from_millis(500);
        assert_eq!(cpu_limit_secs(&job).unwrap(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn cpu_rlimit_signals_are_limit_failures() {
        for sig in ["XCPU", "KILL"] {
            let status = Command::new("sh")
                .args(["-c", &format!("kill -s {sig} $$")])
                .status()
                .unwrap();
            let failure = classify_worker(status, b"", b"", false).unwrap();
            assert!(
                matches!(failure, Failure::Limit(ref message) if message.contains("limit:")),
                "{sig}: {failure}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn guest_helper_encoded_cpu_signals_are_limit_failures() {
        for sig in [libc::SIGKILL, libc::SIGXCPU] {
            let code = 128 + sig;
            let status = Command::new("sh")
                .args(["-c", &format!("exit {code}")])
                .status()
                .unwrap();
            let failure = classify_worker(status, b"", b"", false).unwrap();
            assert!(
                matches!(failure, Failure::Limit(ref message) if message.contains("limit:")),
                "exit {code}: {failure}"
            );
        }
    }

    #[test]
    fn cgroup_oom_is_a_limit_failure() {
        let status = Command::new("true").status().unwrap();
        let failure = classify_worker(status, b"", b"", true).unwrap();
        assert!(
            matches!(failure, Failure::Limit(ref message) if message.contains("oom")),
            "{failure}"
        );
    }

    #[test]
    fn fair_source_starts_a_second_owner_before_the_flood_drains() {
        let flood = 20;
        let mut jobs = Vec::new();
        for i in 0..flood {
            jobs.push(queued_job(&format!("a{i}"), "alpha"));
        }
        jobs.push(queued_job("b0", "beta"));
        let mut source = RecordingSource {
            inner: crate::fair::FairSource::new(jobs).unwrap(),
            started: Vec::new(),
            started_at: Vec::new(),
        };
        let parent = tempfile::tempdir().unwrap();
        let config = RunnerConfig {
            worker: PathBuf::from("/bin/false"),
            scratch_parent: parent.path().to_path_buf(),
            keys: None,
        };
        let finishes = run_source(&mut source, &config).unwrap();
        assert_eq!(finishes.len(), flood + 1);
        assert!(
            finishes.iter().all(|finish| matches!(
                finish,
                JobFinish::Failed { failure: Failure::Runner(msg), .. } if msg.contains("gone")
            )),
            "{finishes:?}"
        );

        let started_ids: Vec<&str> = source
            .started_at
            .iter()
            .map(|(id, _)| id.as_str())
            .collect();
        let beta_slot = started_ids
            .iter()
            .position(|id| *id == "b0")
            .expect("beta run started");
        let last_alpha = format!("a{}", flood - 1);
        let last_alpha_slot = started_ids
            .iter()
            .position(|id| *id == last_alpha)
            .expect("flood still starts");
        let beta_at = source.started_at[beta_slot].1;
        let last_alpha_at = source.started_at[last_alpha_slot].1;
        // Sequential `run_source` starts one job per slot. FIFO would place
        // beta at slot `flood`; fair share starts it within the first few.
        assert!(
            beta_slot < 3,
            "beta started at slot {beta_slot}, started {:?}",
            source.started
        );
        assert!(
            beta_slot < last_alpha_slot,
            "beta must start before the flood drains; started {:?}",
            source.started
        );
        assert!(
            beta_at < last_alpha_at,
            "beta delay must end before the flood's last start; started {:?}",
            source.started
        );
    }

    /// With N owners and one flood, every other owner's first job starts
    /// within N−1 slots of the first start. Sequential `run_source` records
    /// one start per slot; missing-input jobs fail before the sandbox.
    #[test]
    fn fair_source_load_test_bounds_non_flood_start_delay() {
        const TRIALS: usize = 25;
        const FLOOD: usize = 40;
        let flood_owner = "alpha";
        let others = ["beta", "gamma"];
        let max_delay = others.len();
        let mut worst_delay = 0usize;

        for trial in 0..TRIALS {
            let mut jobs = Vec::new();
            for i in 0..FLOOD {
                jobs.push(queued_job(&format!("t{trial}-a{i}"), flood_owner));
            }
            for other in others {
                jobs.push(queued_job(&format!("t{trial}-{other}"), other));
            }
            let mut source = RecordingSource {
                inner: crate::fair::FairSource::new(jobs).unwrap(),
                started: Vec::new(),
                started_at: Vec::new(),
            };
            let parent = tempfile::tempdir().unwrap();
            let config = RunnerConfig {
                worker: PathBuf::from("/bin/false"),
                scratch_parent: parent.path().to_path_buf(),
                keys: None,
            };
            let finishes = run_source(&mut source, &config).unwrap();
            assert_eq!(finishes.len(), FLOOD + others.len());
            assert!(
                finishes.iter().all(|finish| matches!(
                    finish,
                    JobFinish::Failed { failure: Failure::Runner(msg), .. } if msg.contains("gone")
                )),
                "trial {trial}: {finishes:?}"
            );

            let started = &source.started;
            let last_flood_slot = started
                .iter()
                .rposition(|tenant| tenant == flood_owner)
                .expect("flood starts");
            for other in others {
                let slot = started
                    .iter()
                    .position(|tenant| tenant == other)
                    .unwrap_or_else(|| panic!("{other} run started; started {started:?}"));
                assert!(
                    slot <= max_delay,
                    "trial {trial}: {other} started at slot {slot} (bound {max_delay}); started {started:?}"
                );
                assert!(
                    slot < last_flood_slot,
                    "trial {trial}: {other} must start before the flood drains; started {started:?}"
                );
                worst_delay = worst_delay.max(slot);
            }
        }
        assert!(
            worst_delay <= max_delay,
            "worst non-flood start delay {worst_delay} exceeds bound {max_delay}"
        );
    }

    fn keyed_job(id: &str, tenant: &str, claim: crate::JobKeyClaim) -> Job {
        Job {
            id: id.into(),
            engine: Engine::DuckDbSql,
            code: "SELECT 1".into(),
            inputs: vec![],
            outputs: vec!["out.csv".into()],
            wall_clock: Duration::from_secs(5),
            cpu_time: None,
            memory_bytes: None,
            tenant_id: tenant.into(),
            namespace: "default".into(),
            runtime_image: None,
            write_mode: crate::WriteMode::Overwrite,
            isolation: crate::IsolationTier::LinuxNative,
            output_markings: Default::default(),
            declassify: None,
            encryption: Some(claim),
        }
    }

    #[test]
    fn revoked_key_fails_before_sandbox() {
        let provider = Arc::new(
            crate::crypto::MemoryKeyProvider::new(crate::crypto::RuntimeProfile::Development)
                .unwrap(),
        );
        let tenant = crate::crypto::TenantId::new("acme").unwrap();
        let dek = crate::crypto::Dek::generate();
        let wrapped = provider
            .wrap(&tenant, crate::crypto::KeyPurpose::Store, &dek)
            .unwrap();
        provider.revoke_dek(&wrapped.dek_id).unwrap();
        let claim = crate::JobKeyClaim {
            wrapped_dek: serde_json::to_string(&wrapped).unwrap(),
            dek_id: wrapped.dek_id.as_str().to_string(),
            kek_id: provider.describe().kek_id.as_str().to_string(),
        };
        let parent = tempfile::tempdir().unwrap();
        let err = match execute_job(
            &keyed_job("revoked", "acme", claim),
            &RunnerConfig {
                worker: PathBuf::from("/bin/false"),
                scratch_parent: parent.path().to_path_buf(),
                keys: Some(provider),
            },
        ) {
            Err(err) => err,
            Ok(_) => panic!("expected revoked key to fail"),
        };
        assert!(
            matches!(err, Failure::KeyRevoked { ref dek_id, .. } if dek_id == wrapped.dek_id.as_str()),
            "{err}"
        );
        let leftovers: Vec<_> = fs::read_dir(parent.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with("kako-run-"))
            .collect();
        assert!(leftovers.is_empty(), "scratch leftover: {leftovers:?}");
    }

    #[test]
    fn encrypted_job_without_provider_fails_closed() {
        let claim = crate::JobKeyClaim {
            wrapped_dek: "{}".into(),
            dek_id: "dek".into(),
            kek_id: "kek".into(),
        };
        let parent = tempfile::tempdir().unwrap();
        let err = match execute_job(
            &keyed_job("no-provider", "acme", claim),
            &RunnerConfig {
                worker: PathBuf::from("/bin/false"),
                scratch_parent: parent.path().to_path_buf(),
                keys: None,
            },
        ) {
            Err(err) => err,
            Ok(_) => panic!("expected encrypted job without provider to fail"),
        };
        assert!(
            matches!(err, Failure::Runner(ref msg) if msg.contains("key provider")),
            "{err}"
        );
    }
}
