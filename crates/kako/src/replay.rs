//! Replay: re-run a pipeline's steps exactly as pinned, into a branch.
//!
//! A replay file names an output branch and ordered steps. Each step is a
//! local job file plus pins: the input digests, the code digest (transform
//! version), the engine version and, for Python, the job's `runtime_image`.
//! Optional output pins are the original run's digests.
//!
//! [`check`] reports every step that cannot be reproduced on this runner
//! without running anything. [`run`] refuses unless every step is
//! reproducible, then runs the steps in order into `<output>/<branch>/<step>/`,
//! re-checks the pins against each step's receipt, and writes `replay.json`
//! next to the step directories. A pin is never substituted: a missing
//! engine, runtime image or input version is reported, and the job source
//! decides what to do next.

use std::collections::{BTreeMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::digest::{hash_bytes, hash_reader};
use crate::local::{LocalError, load_job, write_completion};
use crate::run::{RunnerConfig, execute_job};
use crate::{Job, Receipt};

/// `replay.json` format version.
const REPORT_VERSION: u32 = 1;

/// Branch names a replay may never write to.
const PROTECTED_BRANCHES: &[&str] = &["main", "master"];

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReplayFile {
    id: String,
    branch: String,
    steps: Vec<StepFile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StepFile {
    id: String,
    /// Job file, relative to the replay file.
    job: String,
    pins: Pins,
}

/// What a step must reproduce.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pins {
    /// Digest of every input, by input name.
    pub inputs: BTreeMap<String, String>,
    /// Digest of the step's code: the transform version.
    pub code: String,
    /// Engine name and version as the original receipt spelled it.
    pub engine_version: String,
    /// Original output digests. A replay with outputs pinned reports whether
    /// it reproduced them.
    #[serde(default)]
    pub outputs: BTreeMap<String, String>,
}

/// A loaded replay: branch and steps with their jobs. Only [`load`] builds
/// one, so the branch and step ids are always validated.
#[derive(Debug, Clone)]
pub struct Replay {
    id: String,
    branch: String,
    steps: Vec<Step>,
}

impl Replay {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn branch(&self) -> &str {
        &self.branch
    }
}

#[derive(Debug, Clone)]
struct Step {
    id: String,
    job: Job,
    pins: Pins,
}

/// Parse a replay file. Job paths resolve against the replay file's directory.
pub fn load(path: &Path) -> Result<Replay, LocalError> {
    let raw = fs::read_to_string(path)
        .map_err(|error| LocalError(format!("read {}: {error}", path.display())))?;
    let file: ReplayFile = serde_json::from_str(&raw)
        .map_err(|error| LocalError(format!("parse {}: {error}", path.display())))?;
    if file.id.is_empty() {
        return Err(LocalError("replay id is required".into()));
    }
    validate_branch(&file.branch)?;
    if file.steps.is_empty() {
        return Err(LocalError("replay must name at least one step".into()));
    }
    let base = path.parent().unwrap_or_else(|| Path::new("."));
    let mut seen = HashSet::new();
    let mut steps = Vec::with_capacity(file.steps.len());
    for step in file.steps {
        if step.id.is_empty()
            || !step
                .id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
        {
            return Err(LocalError(format!(
                "step id `{}` must be alphanumeric, `_` or `-`",
                step.id
            )));
        }
        // Step ids name directories; reject ids that alias on a
        // case-insensitive filesystem.
        if !seen.insert(step.id.to_ascii_lowercase()) {
            return Err(LocalError(format!("duplicate step `{}`", step.id)));
        }
        if !relative_path(&step.job) {
            return Err(LocalError(format!(
                "step `{}` job must be relative to the replay file",
                step.id
            )));
        }
        let job = load_job(&base.join(&step.job))?;
        steps.push(Step {
            id: step.id,
            job,
            pins: step.pins,
        });
    }
    Ok(Replay {
        id: file.id,
        branch: file.branch,
        steps,
    })
}

/// Output branch: relative path segments, never a protected branch.
fn validate_branch(branch: &str) -> Result<(), LocalError> {
    let segments_ok = relative_path(branch)
        && branch.split('/').all(|segment| {
            !segment.is_empty()
                && !segment.starts_with('.')
                && segment
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        });
    if !segments_ok {
        return Err(LocalError(format!(
            "branch `{branch}` must be `/`-separated segments of alphanumerics, `_`, `-` or `.`"
        )));
    }
    // Case-insensitive: on such a filesystem `Main` is the `main` directory.
    if PROTECTED_BRANCHES
        .iter()
        .any(|protected| protected.eq_ignore_ascii_case(branch))
    {
        return Err(LocalError(format!(
            "replays never write to `{branch}`; name an isolated branch"
        )));
    }
    Ok(())
}

fn relative_path(path: &str) -> bool {
    let path = Path::new(path);
    !path.as_os_str().is_empty() && path.components().all(|c| matches!(c, Component::Normal(_)))
}

/// Why each step cannot be reproduced on this runner, by step id. Empty when
/// every step can. Runs nothing.
pub fn check(replay: &Replay) -> Vec<(String, Vec<String>)> {
    replay
        .steps
        .iter()
        .filter_map(|step| {
            let problems = step_problems(step);
            (!problems.is_empty()).then(|| (step.id.clone(), problems))
        })
        .collect()
}

fn step_problems(step: &Step) -> Vec<String> {
    let Step { job, pins, .. } = step;
    let mut problems = Vec::new();
    let code = hash_bytes(job.code.as_bytes()).0;
    if code != pins.code {
        problems.push(format!(
            "code is `{code}`, pinned transform version is `{}`",
            pins.code
        ));
    }
    for input in &job.inputs {
        match pins.inputs.get(&input.name) {
            None => problems.push(format!("input `{}` is not pinned", input.name)),
            Some(pinned) => match hash_input(&input.path) {
                Ok(actual) if &actual.0 == pinned => {}
                Ok(actual) => problems.push(format!(
                    "input `{}` is `{}`, pinned version is `{pinned}`",
                    input.name, actual.0
                )),
                Err(error) => problems.push(format!(
                    "input `{}` pinned version `{pinned}` is unavailable: {error}",
                    input.name
                )),
            },
        }
    }
    for name in pins.inputs.keys() {
        if !job.inputs.iter().any(|input| &input.name == name) {
            problems.push(format!("pinned input `{name}` is not a job input"));
        }
    }
    for name in pins.outputs.keys() {
        if !job.outputs.contains(name) {
            problems.push(format!("pinned output `{name}` is not a job output"));
        }
    }
    match kako_worker::sql_engine_version(job.engine) {
        Some(Ok(version)) if version == pins.engine_version => {}
        Some(Ok(version)) => problems.push(format!(
            "pinned engine `{}` is not available; this runner has `{version}`",
            pins.engine_version
        )),
        Some(Err(error)) => problems.push(format!("engine version: {error}")),
        // Python's version depends on the interpreter and the image's
        // packages, so the image pin must resolve first.
        None => match job.runtime_image.as_deref() {
            None => problems
                .push("python step must pin `runtime_image`; the default image can change".into()),
            Some(pin) => match kako_engine_python::ImageCatalog::from_env().select(Some(pin)) {
                Err(error) => problems.push(error),
                Ok(image) => match kako_engine_python::engine_version(&image) {
                    Ok(version) if version == pins.engine_version => {}
                    Ok(version) => problems.push(format!(
                        "pinned engine `{}` is not available; this runner has `{version}`",
                        pins.engine_version
                    )),
                    Err(error) => problems.push(format!("python runtime is unavailable: {error}")),
                },
            },
        },
    }
    problems
}

/// Digest of an input before staging, refusing what staging refuses: links
/// and anything but a regular file. Opened without following links and
/// without blocking, so a FIFO cannot stall the check.
fn hash_input(path: &Path) -> Result<crate::Digest, String> {
    let meta = fs::symlink_metadata(path).map_err(|error| error.to_string())?;
    if !meta.is_file() {
        return Err("not a regular file".into());
    }
    let mut open = fs::OpenOptions::new();
    open.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = open.open(path).map_err(|error| error.to_string())?;
    if !file
        .metadata()
        .map_err(|error| error.to_string())?
        .is_file()
    {
        return Err("not a regular file".into());
    }
    hash_reader(file).map_err(|error| error.to_string())
}

/// How one step of a replay ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    /// Ran with every pin honored and every pinned output digest matched.
    Reproduced,
    /// Ran with every pin honored; at least one output digest differs.
    Diverged,
    /// Ran with every pin honored and no output pins to compare.
    Ran,
    /// A pin could not be honored. Nothing from the step was written.
    NotReproducible,
    /// The job failed.
    Failed,
    /// Not run because an earlier step did not finish.
    Skipped,
}

#[derive(Debug, Clone, Serialize)]
pub struct OutputCheck {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pinned: Option<String>,
    pub actual: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct StepReport {
    pub id: String,
    pub job_id: String,
    pub status: StepStatus,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub engine_version: Option<String>,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub outputs: BTreeMap<String, OutputCheck>,
    /// Step receipt, relative to the branch directory.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receipt: Option<String>,
}

/// `replay.json`: what the replay ran and whether it reproduced the pins.
#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub replay_version: u32,
    pub id: String,
    pub branch: String,
    /// False while the replay is still running, or after it was interrupted.
    /// Only a complete report describes the step directories.
    pub complete: bool,
    /// Every step ran with its pins honored.
    pub reproducible: bool,
    pub steps: Vec<StepReport>,
}

impl Report {
    fn new(replay: &Replay) -> Self {
        Self {
            replay_version: REPORT_VERSION,
            id: replay.id.clone(),
            branch: replay.branch.clone(),
            complete: false,
            reproducible: false,
            steps: Vec::new(),
        }
    }

    fn step(step: &Step, status: StepStatus, problems: Vec<String>) -> StepReport {
        StepReport {
            id: step.id.clone(),
            job_id: step.job.id.clone(),
            status,
            problems,
            engine_version: None,
            outputs: BTreeMap::new(),
            receipt: None,
        }
    }
}

/// Run every step into the branch directory under `output`, after [`check`]
/// passes for all of them. Writes and returns the report. Re-running a replay
/// rewrites the same branch directory, so a crashed replay restarts from the
/// top. `output` is the operator's root and is trusted as `kako run` trusts
/// `--output`; only the branch directory and below are checked for links.
pub fn run(replay: &Replay, output: &Path, config: &RunnerConfig) -> Result<Report, LocalError> {
    let branch_dir = branch_dir(output, &replay.branch)?;
    // One replay per branch at a time. The OS drops the lock if the process
    // dies, so a crashed replay never blocks the rerun.
    let lock = branch_dir.join(".replay.lock");
    let mut open = fs::OpenOptions::new();
    open.create(true).truncate(false).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open.custom_flags(libc::O_NOFOLLOW);
    }
    let lock = match open.open(&lock) {
        Ok(file) if file.metadata().is_ok_and(|meta| meta.is_file()) => file,
        Ok(_) => return Err(LocalError(format!("{} is not a file", lock.display()))),
        Err(error) => return Err(LocalError(format!("open {}: {error}", lock.display()))),
    };
    lock.try_lock().map_err(|error| {
        LocalError(format!(
            "branch `{}` is busy with another replay: {error}",
            replay.branch
        ))
    })?;
    let mut report = Report::new(replay);
    // Mark the branch incomplete before any step directory changes, so an
    // interrupted rerun never leaves an earlier success describing it.
    write_report(&branch_dir, &report)?;
    // Every step of this attempt starts empty, so a skipped or refused step
    // never keeps an earlier attempt's receipt.
    for step in &replay.steps {
        clear_step(&branch_dir.join(&step.id))?;
    }
    report.complete = true;
    let refused: BTreeMap<_, _> = check(replay).into_iter().collect();
    if !refused.is_empty() {
        report.steps = replay
            .steps
            .iter()
            .map(|step| match refused.get(&step.id) {
                Some(problems) => Report::step(step, StepStatus::NotReproducible, problems.clone()),
                None => Report::step(step, StepStatus::Skipped, Vec::new()),
            })
            .collect();
        write_report(&branch_dir, &report)?;
        return Ok(report);
    }
    let mut stopped = false;
    for step in &replay.steps {
        if stopped {
            report
                .steps
                .push(Report::step(step, StepStatus::Skipped, Vec::new()));
            continue;
        }
        let entry = run_step(step, &branch_dir, config)?;
        stopped = !matches!(
            entry.status,
            StepStatus::Reproduced | StepStatus::Diverged | StepStatus::Ran
        );
        report.steps.push(entry);
    }
    report.reproducible = !stopped;
    write_report(&branch_dir, &report)?;
    Ok(report)
}

fn run_step(
    step: &Step,
    branch_dir: &Path,
    config: &RunnerConfig,
) -> Result<StepReport, LocalError> {
    let step_dir = branch_dir.join(&step.id);
    let completed = match execute_job(&step.job, config) {
        Ok(completed) => completed,
        Err(failure) => {
            return Ok(Report::step(
                step,
                StepStatus::Failed,
                vec![failure.to_string()],
            ));
        }
    };
    let problems = receipt_problems(&completed.receipt, &step.pins);
    let mut entry = Report::step(step, StepStatus::NotReproducible, problems);
    entry.engine_version = Some(completed.receipt.engine_version.clone());
    if !entry.problems.is_empty() {
        return Ok(entry);
    }
    write_completion(&step_dir, &completed.outputs, &completed.receipt)?;
    entry.receipt = Some(format!("{}/receipt.json", step.id));
    entry.outputs = completed
        .receipt
        .outputs
        .iter()
        .map(|(name, digest)| {
            let check = OutputCheck {
                pinned: step.pins.outputs.get(name).cloned(),
                actual: digest.0.clone(),
            };
            (name.clone(), check)
        })
        .collect();
    entry.status = if step.pins.outputs.is_empty() {
        StepStatus::Ran
    } else if entry.outputs.values().all(|check| {
        check
            .pinned
            .as_ref()
            .is_none_or(|pinned| pinned == &check.actual)
    }) {
        StepStatus::Reproduced
    } else {
        StepStatus::Diverged
    };
    Ok(entry)
}

/// Pins the run itself must confirm: what was staged and which engine ran.
fn receipt_problems(receipt: &Receipt, pins: &Pins) -> Vec<String> {
    let mut problems = Vec::new();
    if receipt.engine_version != pins.engine_version {
        problems.push(format!(
            "ran on `{}`, pinned engine is `{}`",
            receipt.engine_version, pins.engine_version
        ));
    }
    for (name, digest) in &receipt.inputs {
        if pins.inputs.get(name) != Some(&digest.0) {
            problems.push(format!(
                "input `{name}` changed to `{}` before it was staged",
                digest.0
            ));
        }
    }
    problems
}

/// One directory per branch directly under the output root, `/` written as
/// `%2F`. `%` is not allowed in branch names, so names never collide, and a
/// branch never nests inside another branch's step directories.
fn branch_dir_name(branch: &str) -> String {
    branch.replace('/', "%2F")
}

/// Create the branch directory, refusing a symlink or file in its place, so
/// replay writes and deletes stay inside the output root.
fn branch_dir(output: &Path, branch: &str) -> Result<PathBuf, LocalError> {
    fs::create_dir_all(output)
        .map_err(|error| LocalError(format!("create {}: {error}", output.display())))?;
    let dir = output.join(branch_dir_name(branch));
    match fs::symlink_metadata(&dir) {
        Ok(meta) if meta.is_dir() => {}
        Ok(_) => {
            return Err(LocalError(format!(
                "{} must be a directory, not a link or file",
                dir.display()
            )));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::create_dir(&dir)
            .map_err(|error| LocalError(format!("create {}: {error}", dir.display())))?,
        Err(error) => return Err(LocalError(format!("{}: {error}", dir.display()))),
    }
    Ok(dir)
}

fn clear_step(step_dir: &Path) -> Result<(), LocalError> {
    match fs::symlink_metadata(step_dir) {
        Ok(meta) if meta.is_dir() => fs::remove_dir_all(step_dir)
            .map_err(|error| LocalError(format!("clear {}: {error}", step_dir.display()))),
        Ok(_) => Err(LocalError(format!(
            "{} is not a directory",
            step_dir.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(LocalError(format!("{}: {error}", step_dir.display()))),
    }
}

fn write_report(branch_dir: &Path, report: &Report) -> Result<(), LocalError> {
    let mut json = serde_json::to_string_pretty(report)
        .map_err(|error| LocalError(format!("serialize replay report: {error}")))?;
    json.push('\n');
    let path = branch_dir.join("replay.json");
    let tmp = branch_dir.join(".replay.json.kako-tmp");
    for file in [&path, &tmp] {
        if fs::symlink_metadata(file).is_ok_and(|meta| meta.file_type().is_symlink()) {
            return Err(LocalError(format!(
                "refusing to write through symlink {}",
                file.display()
            )));
        }
    }
    fs::write(&tmp, json)
        .map_err(|error| LocalError(format!("write {}: {error}", tmp.display())))?;
    fs::rename(&tmp, &path)
        .map_err(|error| LocalError(format!("rename {}: {error}", path.display())))
}

/// Path of the report [`run`] writes.
pub fn report_path(replay: &Replay, output: &Path) -> PathBuf {
    output
        .join(branch_dir_name(&replay.branch))
        .join("replay.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Engine;
    use crate::digest::hash_file;

    const SQL: &str =
        "COPY (SELECT x FROM t ORDER BY x) TO 'outputs/out.csv' (HEADER, DELIMITER ',');";

    fn write_job(dir: &Path, name: &str, body: serde_json::Value) {
        fs::write(dir.join(name), body.to_string()).unwrap();
    }

    fn write_replay(dir: &Path, branch: &str, steps: serde_json::Value) -> PathBuf {
        let path = dir.join("replay.json");
        let body = serde_json::json!({"id": "r1", "branch": branch, "steps": steps});
        fs::write(&path, body.to_string()).unwrap();
        path
    }

    fn sql_fixture(dir: &Path) -> serde_json::Value {
        fs::write(dir.join("t.csv"), "x\n2\n1\n").unwrap();
        write_job(
            dir,
            "job.json",
            serde_json::json!({"id": "step", "sql": SQL, "inputs": {"t": "t.csv"}, "outputs": ["out.csv"]}),
        );
        serde_json::json!({
            "code": hash_bytes(SQL.as_bytes()).0,
            "engine_version": kako_worker::sql_engine_version(Engine::DuckDbSql).unwrap().unwrap(),
            "inputs": {"t": hash_file(&dir.join("t.csv")).unwrap().0},
        })
    }

    #[test]
    fn honored_pins_leave_nothing_to_report() {
        let dir = tempfile::tempdir().unwrap();
        let pins = sql_fixture(dir.path());
        let path = write_replay(
            dir.path(),
            "replay/r1",
            serde_json::json!([{"id": "a", "job": "job.json", "pins": pins}]),
        );
        assert_eq!(check(&load(&path).unwrap()), Vec::new());
    }

    #[test]
    fn every_unhonored_pin_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        sql_fixture(dir.path());
        let pins = serde_json::json!({
            "code": "sha256:00",
            "engine_version": "duckdb 0.0.1",
            "inputs": {"t": "sha256:11", "gone": "sha256:22"},
            "outputs": {"other.csv": "sha256:33"},
        });
        let path = write_replay(
            dir.path(),
            "replay/r1",
            serde_json::json!([{"id": "a", "job": "job.json", "pins": pins}]),
        );
        let refused = check(&load(&path).unwrap());
        let [(step, problems)] = refused.as_slice() else {
            panic!("{refused:?}");
        };
        assert_eq!(step, "a");
        let text = problems.join("\n");
        for expected in [
            "pinned transform version is `sha256:00`",
            "input `t` is `sha256:",
            "pinned input `gone` is not a job input",
            "pinned output `other.csv` is not a job output",
            "pinned engine `duckdb 0.0.1` is not available",
        ] {
            assert!(text.contains(expected), "{expected}: {text}");
        }
    }

    #[test]
    fn a_missing_pinned_image_is_reported_and_nothing_runs() {
        let dir = tempfile::tempdir().unwrap();
        let sql_pins = sql_fixture(dir.path());
        let missing = format!("python-lockfile:sha256:{}", "0".repeat(64));
        let code = "import polars\n";
        write_job(
            dir.path(),
            "py.json",
            serde_json::json!({
                "id": "py", "engine": "python", "code": code, "inputs": {},
                "outputs": ["p.csv"], "runtime_image": missing,
            }),
        );
        let path = write_replay(
            dir.path(),
            "replay/r1",
            serde_json::json!([
                {"id": "first", "job": "job.json", "pins": sql_pins},
                {"id": "py", "job": "py.json", "pins": {
                    "code": hash_bytes(code.as_bytes()).0,
                    "engine_version": "python 3.12.0",
                    "inputs": {},
                }},
            ]),
        );
        let replay = load(&path).unwrap();
        let out = dir.path().join("out");
        // An earlier attempt's step output must not survive this one.
        fs::create_dir_all(out.join("replay%2Fr1/first")).unwrap();
        fs::write(out.join("replay%2Fr1/first/receipt.json"), "{}").unwrap();
        let config = RunnerConfig::for_current_exe(dir.path().join("scratch")).unwrap();
        let report = run(&replay, &out, &config).unwrap();
        assert!(!report.reproducible);
        assert_eq!(report.steps[0].status, StepStatus::Skipped);
        assert_eq!(report.steps[1].status, StepStatus::NotReproducible);
        assert!(
            report.steps[1].problems[0]
                .contains(&format!("`{missing}` is not in the image catalog")),
            "{:?}",
            report.steps[1].problems
        );
        let written: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(report_path(&replay, &out)).unwrap()).unwrap();
        assert_eq!(written["steps"][1]["status"], "not_reproducible");
        assert_eq!(written["complete"], true);
        assert!(!out.join("replay%2Fr1/first").exists());
        assert!(!out.join("replay%2Fr1/py").exists());
        assert!(!dir.path().join("scratch").exists());
    }

    #[test]
    fn unpinned_python_images_are_not_reproducible() {
        let dir = tempfile::tempdir().unwrap();
        write_job(
            dir.path(),
            "py.json",
            serde_json::json!({"id": "py", "engine": "python", "code": "x = 1\n", "inputs": {}, "outputs": ["p.csv"]}),
        );
        let path = write_replay(
            dir.path(),
            "replay/r1",
            serde_json::json!([{"id": "py", "job": "py.json", "pins": {
                "code": hash_bytes(b"x = 1\n").0, "engine_version": "python 3.12.0", "inputs": {},
            }}]),
        );
        let refused = check(&load(&path).unwrap());
        assert!(
            refused[0].1[0].contains("must pin `runtime_image`"),
            "{refused:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_linked_branch_directory_is_refused_before_anything_changes() {
        let dir = tempfile::tempdir().unwrap();
        let pins = sql_fixture(dir.path());
        let path = write_replay(
            dir.path(),
            "replay/r1",
            serde_json::json!([{"id": "a", "job": "job.json", "pins": pins}]),
        );
        let elsewhere = dir.path().join("published");
        fs::create_dir_all(elsewhere.join("a")).unwrap();
        fs::write(elsewhere.join("a/keep.csv"), "x\n").unwrap();
        let out = dir.path().join("out");
        fs::create_dir_all(&out).unwrap();
        std::os::unix::fs::symlink(&elsewhere, out.join("replay%2Fr1")).unwrap();
        let config = RunnerConfig::for_current_exe(dir.path().join("scratch")).unwrap();
        let error = run(&load(&path).unwrap(), &out, &config).unwrap_err();
        assert!(error.0.contains("not a link"), "{error}");
        assert!(elsewhere.join("a/keep.csv").is_file());
        assert!(!elsewhere.join("replay.json").exists());
    }

    #[cfg(unix)]
    #[test]
    fn linked_inputs_are_refused_before_they_are_read() {
        let dir = tempfile::tempdir().unwrap();
        let pins = sql_fixture(dir.path());
        fs::rename(dir.path().join("t.csv"), dir.path().join("real.csv")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("real.csv"), dir.path().join("t.csv")).unwrap();
        let path = write_replay(
            dir.path(),
            "replay/r1",
            serde_json::json!([{"id": "a", "job": "job.json", "pins": pins}]),
        );
        let refused = check(&load(&path).unwrap());
        assert!(
            refused[0].1[0].contains("not a regular file"),
            "{refused:?}"
        );
    }

    #[test]
    fn a_branch_runs_one_replay_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let pins = sql_fixture(dir.path());
        let path = write_replay(
            dir.path(),
            "replay/r1",
            serde_json::json!([{"id": "a", "job": "job.json", "pins": pins}]),
        );
        let out = dir.path().join("out");
        fs::create_dir_all(out.join("replay%2Fr1")).unwrap();
        let held = fs::File::create(out.join("replay%2Fr1/.replay.lock")).unwrap();
        held.lock().unwrap();
        let config = RunnerConfig::for_current_exe(dir.path().join("scratch")).unwrap();
        let error = run(&load(&path).unwrap(), &out, &config).unwrap_err();
        assert!(error.0.contains("busy with another replay"), "{error}");
        assert!(!out.join("replay%2Fr1/replay.json").exists());
    }

    #[test]
    fn step_ids_may_not_alias_by_case() {
        let dir = tempfile::tempdir().unwrap();
        let pins = sql_fixture(dir.path());
        let path = write_replay(
            dir.path(),
            "replay/r1",
            serde_json::json!([
                {"id": "a", "job": "job.json", "pins": pins},
                {"id": "A", "job": "job.json", "pins": pins},
            ]),
        );
        assert!(load(&path).unwrap_err().0.contains("duplicate step `A`"));
    }

    #[test]
    fn replays_only_write_to_isolated_branches() {
        let dir = tempfile::tempdir().unwrap();
        let pins = sql_fixture(dir.path());
        let steps = serde_json::json!([{"id": "a", "job": "job.json", "pins": pins}]);
        for branch in [
            "main", "master", "Main", "MASTER", "main/", "", "../up", "/abs", "a/../b", ".hidden",
            "a b", "a//b", "a%2Fb",
        ] {
            let path = write_replay(dir.path(), branch, steps.clone());
            assert!(load(&path).is_err(), "{branch}");
        }
        for branch in ["replay/2026-10-22", "r1", "main/replay"] {
            let path = write_replay(dir.path(), branch, steps.clone());
            assert_eq!(load(&path).unwrap().branch, branch);
        }
    }
}
