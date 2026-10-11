//! Take one job, stage, sandbox, hash, report, clean up.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime};

use crate::digest::{hash_bytes, hash_file};
use crate::engine;
use crate::sandbox;
use crate::stage::Scratch;
use crate::{Engine, Failure, Job, JobSource, Output, Receipt};

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
    match job.engine {
        crate::Engine::DuckDbSql | crate::Engine::Mizuchi => {}
        crate::Engine::Python => {
            return Err(Failure::Runner("python engine is not implemented".into()));
        }
    }
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
    engine::write_manifest(scratch.path(), job.engine, &job.code, &staged)
        .map_err(Failure::Runner)?;

    let backend = sandbox::select_backend()?;
    // CPU ceiling sits above the wall clock so the hard timeout wins.
    let cpu_time_secs = Some(job.wall_clock.as_secs().saturating_mul(2).max(2));
    let mut command = Command::new(&config.worker);
    command.arg("--internal-execute").arg(scratch.path());
    let (status, stdout, stderr) = sandbox::run_sandboxed(
        command,
        scratch.path(),
        backend,
        job.wall_clock,
        cpu_time_secs,
        &config.worker,
    )?;
    if !status.success() {
        let err = String::from_utf8_lossy(&stderr);
        let out = String::from_utf8_lossy(&stdout);
        let body = format!("worker exited {status}: {err}{out}");
        if status.code() == Some(3) || err.contains("limit:") || out.contains("limit:") {
            return Err(Failure::Limit(body));
        }
        return Err(Failure::Job(body));
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
        lineage,
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
    fn python_engine_is_a_runner_failure() {
        let parent = tempfile::tempdir().unwrap();
        let job = Job {
            id: "py".into(),
            engine: Engine::Python,
            code: "print(1)".into(),
            inputs: vec![],
            outputs: vec!["out.csv".into()],
            wall_clock: Duration::from_secs(5),
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
                } if msg.contains("python")
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
}
