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

/// A resolved Python image: lockfile identity plus optional extra site-packages.
#[derive(Debug)]
pub(crate) struct PythonImage {
    pub id: String,
    pub site_packages: Option<PathBuf>,
}

/// Operator catalog of shipped Python images (`KAKO_PYTHON_IMAGES`).
pub(crate) struct ImageCatalog {
    pub root: Option<PathBuf>,
    pub shipped_id: String,
}

impl ImageCatalog {
    pub(crate) fn from_env() -> Self {
        Self {
            root: std::env::var_os("KAKO_PYTHON_IMAGES").map(PathBuf::from),
            shipped_id: runtime_image_id(),
        }
    }

    /// Select the image the job asked for, or the catalog default.
    pub(crate) fn select(&self, pin: Option<&str>) -> Result<PythonImage, String> {
        let wanted = match pin {
            None => self.default_id()?,
            Some(pin) => validate_runtime_image_id(pin)?,
        };
        self.lookup(&wanted)
    }

    fn default_id(&self) -> Result<String, String> {
        let Some(root) = &self.root else {
            return Ok(self.shipped_id.clone());
        };
        let current = root.join("current");
        if !current.is_file() {
            return Ok(self.shipped_id.clone());
        }
        let raw = fs::read_to_string(&current)
            .map_err(|error| format!("read python image current: {error}"))?;
        validate_runtime_image_id(raw.trim())
    }

    fn lookup(&self, id: &str) -> Result<PythonImage, String> {
        if let Some(root) = &self.root {
            let dir = root.join(image_hex(id)?);
            if dir.is_dir() {
                return load_catalog_entry(&dir, id);
            }
        }
        if id == self.shipped_id {
            return Ok(PythonImage {
                id: id.to_string(),
                site_packages: None,
            });
        }
        Err(format!(
            "python runtime image `{id}` is not in the image catalog"
        ))
    }
}

/// Accept `python-lockfile:sha256:<64 hex>` and canonicalize the hex.
pub(crate) fn validate_runtime_image_id(id: &str) -> Result<String, String> {
    let hex = image_hex(id)?;
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "python runtime_image must be python-lockfile:sha256:<64 hex>, got `{id}`"
        ));
    }
    Ok(format!(
        "python-lockfile:sha256:{}",
        hex.to_ascii_lowercase()
    ))
}

fn image_hex(id: &str) -> Result<&str, String> {
    id.strip_prefix("python-lockfile:sha256:").ok_or_else(|| {
        format!("python runtime_image must be python-lockfile:sha256:<hex>, got `{id}`")
    })
}

fn load_catalog_entry(dir: &Path, expected_id: &str) -> Result<PythonImage, String> {
    let lockfile = dir.join("requirements.txt");
    let bytes = fs::read(&lockfile)
        .map_err(|error| format!("read python image lockfile {}: {error}", lockfile.display()))?;
    let id = format!("python-lockfile:{}", digest::hash_bytes(&bytes).0);
    if id != expected_id {
        return Err(format!(
            "python image {} lockfile digest is `{id}`, expected `{expected_id}`",
            dir.display()
        ));
    }
    let site = dir.join("site-packages");
    let site_packages = if site.is_dir() {
        Some(site.canonicalize().unwrap_or(site))
    } else {
        None
    };
    Ok(PythonImage { id, site_packages })
}

/// Tell the worker which extra site-packages tree to put on PYTHONPATH.
pub(crate) fn write_python_site(scratch: &Path, site: &Path) -> Result<(), String> {
    let text = site
        .to_str()
        .ok_or_else(|| format!("python site-packages path is not utf-8: {}", site.display()))?;
    fs::write(scratch.join("python_site"), text)
        .map_err(|error| format!("write python_site: {error}"))
}

fn read_python_site(scratch: &Path) -> Option<PathBuf> {
    let raw = fs::read_to_string(scratch.join("python_site")).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(PathBuf::from(trimmed))
    }
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

fn apply_python_env(command: &mut Command, scratch: &Path, python_site: Option<&Path>) {
    command.env_clear();
    command.env("PYTHONNOUSERSITE", "1");
    command.env("PYTHONDONTWRITEBYTECODE", "1");
    command.env("PYTHONUTF8", "1");
    command.env("HOME", scratch);
    let tmp = scratch.join("tmp");
    command.env("TMPDIR", &tmp);
    command.env("TMP", &tmp);
    command.env("TEMP", &tmp);
    if let Some(site) = python_site {
        command.env("PYTHONPATH", site);
    }
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
    apply_python_env(&mut command, scratch, read_python_site(scratch).as_deref());
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

    fn image_id_for(lockfile: &[u8]) -> String {
        format!("python-lockfile:{}", digest::hash_bytes(lockfile).0)
    }

    fn write_catalog_image(root: &Path, lockfile: &[u8], version: &str) -> String {
        let id = image_id_for(lockfile);
        let hex = id.rsplit(':').next().expect("image id has hex");
        let dir = root.join(hex);
        fs::create_dir_all(dir.join("site-packages")).unwrap();
        fs::write(dir.join("requirements.txt"), lockfile).unwrap();
        fs::write(
            dir.join("site-packages/kako_image_marker.py"),
            format!("VERSION = {version:?}\n"),
        )
        .unwrap();
        id
    }

    fn run_marker_transform(site: &Path) -> String {
        let code = r#"
def transform(inputs):
    import kako_image_marker as marker
    import polars as pl
    return {"marker.csv": pl.DataFrame({"image": [marker.VERSION]})}
"#;
        let parent = tempfile::tempdir().unwrap();
        let scratch = Scratch::create(parent.path(), "python-pin").unwrap();
        write_manifest(
            scratch.path(),
            Engine::Python,
            code,
            &[],
            &["marker.csv".into()],
        )
        .unwrap();
        write_python_site(scratch.path(), site).unwrap();
        crate::engine::execute_in_scratch(scratch.path()).unwrap();
        fs::read_to_string(scratch.outputs_dir().join("marker.csv")).unwrap()
    }

    #[test]
    fn unpinned_image_is_the_shipped_lockfile() {
        let catalog = ImageCatalog {
            root: None,
            shipped_id: runtime_image_id(),
        };
        let image = catalog.select(None).unwrap();
        assert_eq!(image.id, runtime_image_id());
        assert!(image.site_packages.is_none());
    }

    #[test]
    fn unknown_image_pin_fails_closed() {
        let catalog = ImageCatalog {
            root: None,
            shipped_id: runtime_image_id(),
        };
        let pin = format!("python-lockfile:sha256:{}", "ab".repeat(32));
        let err = catalog.select(Some(&pin)).unwrap_err();
        assert!(err.contains("not in the image catalog"), "{err}");
    }

    #[test]
    fn catalog_entry_must_match_lockfile_digest() {
        let root = tempfile::tempdir().unwrap();
        let wanted = image_id_for(b"lock-a\n");
        let hex = wanted.rsplit(':').next().unwrap();
        let dir = root.path().join(hex);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("requirements.txt"), b"lock-b\n").unwrap();
        let catalog = ImageCatalog {
            root: Some(root.path().to_path_buf()),
            shipped_id: runtime_image_id(),
        };
        let err = catalog.select(Some(&wanted)).unwrap_err();
        assert!(err.contains("lockfile digest"), "{err}");
    }

    #[test]
    fn pinned_lockfile_survives_default_image_upgrade() {
        if skip_without_python().is_none() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let image_a = write_catalog_image(root.path(), b"lock-a\n", "A");
        let image_b = write_catalog_image(root.path(), b"lock-b\n", "B");
        fs::write(root.path().join("current"), &image_a).unwrap();
        let catalog = ImageCatalog {
            root: Some(root.path().to_path_buf()),
            shipped_id: runtime_image_id(),
        };

        let pinned = catalog.select(Some(&image_a)).unwrap();
        assert_eq!(pinned.id, image_a);
        let before = run_marker_transform(pinned.site_packages.as_ref().unwrap());

        fs::write(root.path().join("current"), &image_b).unwrap();
        let pinned_after = catalog.select(Some(&image_a)).unwrap();
        assert_eq!(pinned_after.id, image_a);
        let after = run_marker_transform(pinned_after.site_packages.as_ref().unwrap());
        assert_eq!(before, after, "pinned output changed after default upgrade");
        assert!(before.contains('A'), "{before}");

        let unpinned = catalog.select(None).unwrap();
        assert_eq!(unpinned.id, image_b);
        let upgraded = run_marker_transform(unpinned.site_packages.as_ref().unwrap());
        assert!(upgraded.contains('B'), "{upgraded}");
        assert_ne!(before, upgraded);
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
                eprintln!("skip: rlimit backend unavailable: {other:?}");
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
