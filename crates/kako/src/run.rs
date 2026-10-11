//! Take one job, stage, sandbox, hash, report, clean up.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};

use crate::digest::{hash_bytes, hash_file};
use crate::engine;
use crate::engine_python;
use crate::sandbox;
use crate::stage::Scratch;
use crate::{Engine, Failure, Job, JobSource, Output, QuotaKey, Receipt};

/// How the runner finds a worker binary and where it puts scratch trees.
#[derive(Debug, Clone)]
pub struct RunnerConfig {
    /// kako binary that implements `--internal-execute`.
    pub worker: PathBuf,
    /// Parent directory for per-run scratch trees.
    pub scratch_parent: PathBuf,
}

impl RunnerConfig {
    pub fn for_current_exe(scratch_parent: PathBuf) -> Result<Self, Failure> {
        let worker = std::env::current_exe()
            .map_err(|error| Failure::Runner(format!("current_exe: {error}")))?;
        Ok(Self {
            worker,
            scratch_parent,
        })
    }
}

/// Outcome of one job after the source has been notified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobFinish {
    Completed { job_id: String },
    Failed { job_id: String, failure: Failure },
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
    QuotaKey::from_job(job).map_err(Failure::Runner)?;
    let python = match job.engine {
        crate::Engine::DuckDbSql | crate::Engine::Mizuchi => None,
        crate::Engine::Python => {
            Some(engine_python::resolve_interpreter().map_err(Failure::Runner)?)
        }
    };
    let started_at = SystemTime::now();
    let started = Instant::now();
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
    engine::write_manifest(scratch.path(), job.engine, &job.code, &staged, &job.outputs)
        .map_err(Failure::Runner)?;

    let backend = sandbox::select_backend()?;
    let cpu_time_secs = Some(cpu_limit_secs(job)?);
    let mut command = Command::new(&config.worker);
    command.arg("--internal-execute").arg(scratch.path());
    let mut extra_readable = if let Some(ref interpreter) = python {
        command.env("KAKO_PYTHON", interpreter);
        command.env("PYTHONNOUSERSITE", "1");
        command.env("PYTHONDONTWRITEBYTECODE", "1");
        engine_python::python_readable_roots(interpreter).map_err(Failure::Runner)?
    } else {
        Vec::new()
    };
    extra_readable.push(config.worker.clone());
    let (status, stdout, stderr, usage) = sandbox::run_sandboxed(
        command,
        scratch.path(),
        backend,
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
        if !path.is_file() {
            return Err(Failure::Job(format!(
                "expected output `{name}` was not written"
            )));
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
        runtime_image: python.map(|_| engine_python::runtime_image_id()),
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
fn resource_limit_signal(status: std::process::ExitStatus) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        matches!(status.signal(), Some(libc::SIGXCPU | libc::SIGKILL))
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
            }],
            outputs: vec!["out.csv".into()],
            wall_clock: Duration::from_secs(5),
            cpu_time: None,
            memory_bytes: None,
            tenant_id: "local".into(),
            namespace: "default".into(),
        };
        let mut source = Fake {
            jobs: vec![job],
            fails: Vec::new(),
        };
        let config = RunnerConfig {
            worker: PathBuf::from("/bin/false"),
            scratch_parent: parent.path().to_path_buf(),
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
        };
        let mut source = Fake {
            jobs: vec![job],
            fails: Vec::new(),
        };
        let config = RunnerConfig {
            worker: PathBuf::from("/bin/false"),
            scratch_parent: parent.path().to_path_buf(),
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
            }],
            outputs: vec!["out.csv".into()],
            wall_clock: Duration::from_secs(5),
            cpu_time: None,
            memory_bytes: None,
            tenant_id: tenant.into(),
            namespace: "default".into(),
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
}
