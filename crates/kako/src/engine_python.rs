//! Opt-in Python worker used by the sandboxed child.
//!
//! User source is written to the scratch tree and loaded as data by
//! `runtime/python/kako_run.py`. The runner never imports it.

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::Digest;
use crate::digest;
use crate::engine::StagedInput;

const WRAPPER: &str = include_str!("../../../runtime/python/kako_run.py");
const LOCKFILE: &[u8] = include_bytes!("../../../runtime/python/requirements.txt");

/// Receipt identity for the baked Python package set: lockfile SHA-256.
pub fn runtime_image_id() -> String {
    format!("python-lockfile:{}", lockfile_digest().0)
}

/// Digest of the committed hashed lockfile.
pub fn lockfile_digest() -> Digest {
    digest::hash_bytes(LOCKFILE)
}

/// Resolve the interpreter for a Python job.
///
/// Order: `KAKO_PYTHON`, then `runtime/python/.venv` next to the crate if that
/// path exists, then `python3` on `PATH`. The chosen binary must import
/// Polars, pandas, pyarrow, and DuckDB.
pub fn resolve_interpreter() -> Result<PathBuf, String> {
    resolve_interpreter_with(std::env::var_os("KAKO_PYTHON"), bundled_venv())
}

fn bundled_venv() -> Option<PathBuf> {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../runtime/python/.venv/bin/python");
    path.is_file().then_some(path)
}

fn resolve_interpreter_with(
    kako_python: Option<OsString>,
    bundled: Option<PathBuf>,
) -> Result<PathBuf, String> {
    if let Some(value) = kako_python {
        let path = PathBuf::from(value);
        if !path.is_file() {
            return Err(format!(
                "KAKO_PYTHON `{}` is not a python interpreter",
                path.display()
            ));
        }
        return require_runtime(&path);
    }
    if let Some(bundled) = bundled {
        return require_runtime(&bundled);
    }
    if let Some(path) = python3_with_runtime() {
        return Ok(path);
    }
    Err(
        "python engine needs an interpreter with polars, pandas, pyarrow, and duckdb (set KAKO_PYTHON, or install the runtime lockfile)"
            .into(),
    )
}

fn require_runtime(path: &Path) -> Result<PathBuf, String> {
    if interpreter_has_runtime(path) {
        Ok(path.to_path_buf())
    } else {
        Err(format!(
            "python interpreter `{}` does not provide polars, pandas, pyarrow, and duckdb",
            path.display()
        ))
    }
}

fn interpreter_has_runtime(path: &Path) -> bool {
    Command::new(path)
        .args(["-c", "import polars, pandas, pyarrow, duckdb"])
        .env("PYTHONNOUSERSITE", "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

fn python3_with_runtime() -> Option<PathBuf> {
    let output = Command::new("python3")
        .args([
            "-c",
            "import polars, pandas, pyarrow, duckdb, sys; print(sys.executable)",
        ])
        .env("PYTHONNOUSERSITE", "1")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let path = PathBuf::from(path);
    path.is_file().then_some(path)
}

fn apply_python_env(command: &mut Command, scratch: &Path) {
    command.env_clear();
    command.env("PYTHONNOUSERSITE", "1");
    command.env("PYTHONDONTWRITEBYTECODE", "1");
    command.env("PYTHONUTF8", "1");
    command.env("HOME", scratch);
    let tmp = scratch.join("tmp");
    command.env("TMPDIR", &tmp);
    command.env("TMP", &tmp);
    command.env("TEMP", &tmp);
}

/// Landlock extra roots for this interpreter. Skips `/`, `/usr`, and `/sys`.
pub fn python_readable_roots(interpreter: &Path) -> Result<Vec<PathBuf>, String> {
    let output = Command::new(interpreter)
        .args([
            "-c",
            "import json, sys; print(json.dumps([sys.executable, sys.prefix, sys.base_prefix, sys.exec_prefix, getattr(sys, 'base_exec_prefix', sys.exec_prefix)] + [p for p in sys.path if p]))",
        ])
        .env("PYTHONNOUSERSITE", "1")
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()
        .map_err(|error| format!("probe python prefixes: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "probe python prefixes: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let listed: Vec<String> = serde_json::from_slice(&output.stdout).map_err(|error| {
        format!(
            "probe python prefixes: parse {}: {error}",
            String::from_utf8_lossy(&output.stdout)
        )
    })?;
    let mut roots = Vec::new();
    push_python_root(&mut roots, interpreter);
    for item in listed {
        push_python_root(&mut roots, Path::new(&item));
    }
    roots.sort();
    roots.dedup();
    Ok(roots)
}

fn too_broad(path: &Path) -> bool {
    path == Path::new("/") || path == Path::new("/usr") || path == Path::new("/sys")
}

fn push_python_root(roots: &mut Vec<PathBuf>, path: &Path) {
    if path.as_os_str().is_empty() || too_broad(path) {
        return;
    }
    let canon = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    if too_broad(&canon) {
        return;
    }
    if !canon.exists() {
        return;
    }
    roots.push(canon);
}

/// Write user source plus the wrapper and exec the interpreter in scratch.
pub(crate) fn execute(
    code: &str,
    _inputs: &[StagedInput],
    _outputs: &[String],
    scratch: &Path,
) -> Result<String, String> {
    let interpreter = resolve_interpreter()?;
    fs::write(scratch.join("user.py"), code).map_err(|error| format!("write user.py: {error}"))?;
    fs::write(scratch.join("kako_run.py"), WRAPPER)
        .map_err(|error| format!("write kako_run.py: {error}"))?;
    let mut command = Command::new(&interpreter);
    command.arg(scratch.join("kako_run.py"));
    command.current_dir(scratch);
    apply_python_env(&mut command, scratch);
    let output = command
        .output()
        .map_err(|error| format!("spawn python: {error}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(format!(
            "python: interpreter exited {}: {stderr}{stdout}",
            output.status
        ));
    }
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if version.is_empty() {
        return Err("python: interpreter printed no engine version".into());
    }
    Ok(version)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::write_manifest;
    use crate::stage::Scratch;
    use crate::{Engine, Input};
    use std::ffi::OsString;

    fn engine_bar_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../scripts/engine-bar")
    }

    fn skip_without_python() -> Option<PathBuf> {
        match resolve_interpreter() {
            Ok(path) => Some(path),
            Err(error) => {
                eprintln!("skip: python runtime unavailable: {error}");
                None
            }
        }
    }

    fn assert_join_totals(csv: &str) {
        assert!(csv.contains("customer_id") || csv.contains("id"), "{csv}");
        assert!(csv.contains('a'), "{csv}");
        assert!(csv.contains('b'), "{csv}");
        assert!(
            csv.contains("25") || csv.contains("25.0"),
            "expected aggregate 25, got {csv}"
        );
        assert!(csv.contains('7'), "expected aggregate 7, got {csv}");
    }

    fn run_python_code(code: &str) -> (String, String) {
        let bar = engine_bar_dir();
        let parent = tempfile::tempdir().unwrap();
        let scratch = Scratch::create(parent.path(), "python-engine").unwrap();
        let staged = scratch
            .stage_inputs(&[
                Input {
                    name: "customers".into(),
                    path: bar.join("customers.csv"),
                },
                Input {
                    name: "orders".into(),
                    path: bar.join("orders.csv"),
                },
            ])
            .unwrap();
        write_manifest(
            scratch.path(),
            Engine::Python,
            code,
            &staged,
            &["totals.csv".into()],
        )
        .unwrap();
        let version = crate::engine::execute_in_scratch(scratch.path()).unwrap();
        let csv = fs::read_to_string(scratch.outputs_dir().join("totals.csv")).unwrap();
        (version, csv)
    }

    fn run_fixture(code: &str) -> String {
        let (version, csv) = run_python_code(code);
        assert!(version.starts_with("python "), "{version}");
        assert_join_totals(&csv);
        version
    }

    #[test]
    fn lockfile_digest_matches_committed_bytes() {
        let on_disk = fs::read(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../runtime/python/requirements.txt"),
        )
        .unwrap();
        assert_eq!(digest::hash_bytes(&on_disk), lockfile_digest());
        assert!(runtime_image_id().starts_with("python-lockfile:sha256:"));
    }

    #[test]
    fn dockerfile_installs_from_hashed_lockfile() {
        let docker = fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../runtime/python/Dockerfile"),
        )
        .unwrap();
        assert!(docker.contains("--require-hashes"), "{docker}");
        assert!(docker.contains("requirements.txt"), "{docker}");
        assert!(docker.contains("kako_run.py"), "{docker}");
    }

    #[test]
    fn missing_kako_python_is_an_error() {
        let err = resolve_interpreter_with(Some(OsString::from("/nonexistent/kako-python")), None)
            .unwrap_err();
        assert!(err.contains("KAKO_PYTHON"), "{err}");
    }

    #[test]
    fn kako_python_without_packages_is_an_error() {
        let candidates = ["/usr/bin/python3", "/bin/echo"];
        let Some(path) = candidates
            .iter()
            .map(PathBuf::from)
            .find(|path| path.is_file())
        else {
            eprintln!("skip: no host python/echo binary to reject");
            return;
        };
        let err = resolve_interpreter_with(Some(path.into_os_string()), None).unwrap_err();
        assert!(
            err.contains("does not provide")
                || err.contains("polars")
                || err.contains("not a python"),
            "{err}"
        );
    }

    #[test]
    fn python_transform_does_not_see_runner_env() {
        if skip_without_python().is_none() {
            return;
        }
        let code = r#"
def transform(inputs):
    import os
    leaked = [key for key in os.environ if key.startswith("KAKO_TEST_SECRET_")]
    import polars as pl
    return {"totals.csv": pl.DataFrame({"leaked": [",".join(leaked)]})}
"#;
        unsafe { std::env::set_var("KAKO_TEST_SECRET_ISSUE2", "should-not-leak") };
        let (version, csv) = run_python_code(code);
        unsafe { std::env::remove_var("KAKO_TEST_SECRET_ISSUE2") };
        assert!(version.starts_with("python "), "{version}");
        assert!(
            csv.contains("leaked") && !csv.contains("KAKO_TEST_SECRET_ISSUE2"),
            "{csv}"
        );
    }

    #[test]
    fn python_readable_roots_skip_usr_and_sys() {
        let Some(interpreter) = skip_without_python() else {
            return;
        };
        let roots = python_readable_roots(&interpreter).unwrap();
        for root in &roots {
            assert_ne!(root.as_path(), Path::new("/usr"), "{roots:?}");
            assert_ne!(root.as_path(), Path::new("/sys"), "{roots:?}");
            assert_ne!(root.as_path(), Path::new("/"), "{roots:?}");
        }
        let exe = interpreter
            .canonicalize()
            .unwrap_or_else(|_| interpreter.clone());
        assert!(
            roots
                .iter()
                .any(|root| root == &exe || exe.starts_with(root)),
            "expected interpreter {exe:?} granted via {roots:?}"
        );
    }

    #[test]
    fn polars_join_matches_fixture() {
        if skip_without_python().is_none() {
            return;
        }
        let code = fs::read_to_string(engine_bar_dir().join("join_polars.py")).unwrap();
        run_fixture(&code);
    }

    #[test]
    fn pandas_join_matches_fixture() {
        if skip_without_python().is_none() {
            return;
        }
        let code = fs::read_to_string(engine_bar_dir().join("join_pandas.py")).unwrap();
        run_fixture(&code);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn polars_join_is_killed_by_rlimit_as_at_a_cgroup_sized_cap() {
        let Some(interpreter) = skip_without_python() else {
            return;
        };
        let bar = engine_bar_dir();
        let parent = tempfile::tempdir().unwrap();
        let scratch = Scratch::create(parent.path(), "python-rlimit").unwrap();
        let staged = scratch
            .stage_inputs(&[
                Input {
                    name: "customers".into(),
                    path: bar.join("customers.csv"),
                },
                Input {
                    name: "orders".into(),
                    path: bar.join("orders.csv"),
                },
            ])
            .unwrap();
        let code = fs::read_to_string(bar.join("join_polars.py")).unwrap();
        write_manifest(
            scratch.path(),
            Engine::Python,
            &code,
            &staged,
            &["totals.csv".into()],
        )
        .unwrap();
        fs::write(scratch.path().join("user.py"), &code).unwrap();
        fs::write(scratch.path().join("kako_run.py"), WRAPPER).unwrap();
        let mut settings = kekkai::Settings {
            backend: kekkai::Backend::Rlimit,
            network: kekkai::Network::Deny,
            ..kekkai::Settings::default()
        };
        settings.limits.memory_mb = Some(256);
        match kekkai::health(&settings) {
            kekkai::Health::Ok(_) => {}
            other => {
                eprintln!("skip: rlimit backend unavailable: {other}");
                return;
            }
        }
        let sandbox = kekkai::Sandbox::prepare(&settings, scratch.path()).unwrap();
        let mut command = Command::new(&interpreter);
        command.arg(scratch.path().join("kako_run.py"));
        command.current_dir(scratch.path());
        command.env("PYTHONNOUSERSITE", "1");
        sandbox.apply(&mut command).unwrap();
        let status = command.status().unwrap();
        let _ = sandbox.finish();
        assert!(
            !status.success(),
            "RLIMIT_AS at 256MiB must fail a Polars join that cgroup memory.max of the same size allows"
        );
    }
}
