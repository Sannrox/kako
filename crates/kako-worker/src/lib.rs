//! In-process engine dispatch for `kako --internal-execute`.

use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use kako_core::manifest::read_manifest;
use kako_core::{CANCEL_FILE, Engine, WriteMode};
use kako_engine_mizuchi::CancelToken;

#[cfg(test)]
mod ceiling;

/// Run the staged job inside this process. Prints nothing; returns engine version.
///
/// Does not watch the scratch cancel file: in-process callers (engine-bar,
/// tests) pass their own token when they need cancel.
pub fn execute_in_scratch(scratch: &Path) -> Result<String, String> {
    dispatch(scratch, None)
}

/// Worker child: a scratch cancel file cancels engines that expose a token.
pub fn execute_worker_child(scratch: &Path) -> Result<String, String> {
    dispatch(scratch, Some(arm_cooperative_cancel(scratch)))
}

fn dispatch(scratch: &Path, mizuchi_cancel: Option<CancelToken>) -> Result<String, String> {
    let manifest = read_manifest(scratch)?;
    let write_mode = manifest.write_mode()?;
    let engine = manifest.engine.as_deref().unwrap_or("duckdb_sql");
    if manifest.sample_rows.is_some() && !matches!(engine, "duckdb_sql" | "sql") {
        return Err(format!("preview supports duckdb_sql jobs, not `{engine}`"));
    }
    match engine {
        "duckdb_sql" | "sql" => {
            if write_mode != WriteMode::Overwrite {
                return Err("write_mode is only valid for mizuchi jobs".into());
            }
            let sample = manifest.sample_rows.map(|rows| kako_engine_duckdb::Sample {
                rows,
                capture: manifest.capture,
                outputs: &manifest.outputs,
            });
            kako_engine_duckdb::execute_sampled(&manifest.sql, &manifest.inputs, scratch, sample)
        }
        "mizuchi" => kako_engine_mizuchi::execute_cancel(
            &manifest.sql,
            &manifest.inputs,
            scratch,
            write_mode,
            mizuchi_cancel.unwrap_or_default(),
        ),
        "python" => {
            if write_mode != WriteMode::Overwrite {
                return Err("write_mode is only valid for mizuchi jobs".into());
            }
            kako_engine_python::execute(&manifest.sql, &manifest.inputs, &manifest.outputs, scratch)
        }
        other => Err(format!("unknown engine `{other}`")),
    }
}

fn arm_cooperative_cancel(scratch: &Path) -> CancelToken {
    let token = CancelToken::new();
    let watch = token.clone();
    let path = scratch.join(CANCEL_FILE);
    thread::spawn(move || watch_cancel_file(path, watch));
    token
}

fn watch_cancel_file(path: PathBuf, token: CancelToken) {
    let relative = PathBuf::from(CANCEL_FILE);
    loop {
        if token.is_cancelled() {
            return;
        }
        if path.exists() || relative.exists() {
            token.cancel();
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Version a SQL engine reports on receipts, read without running a job.
/// `None` for Python: its version depends on the runtime image, so read it
/// with `kako_engine_python::engine_version` for the selected image.
pub fn sql_engine_version(engine: Engine) -> Option<Result<String, String>> {
    match engine {
        Engine::DuckDbSql => Some(kako_engine_duckdb::version()),
        Engine::Mizuchi => Some(Ok(kako_engine_mizuchi::version())),
        Engine::Python => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kako_core::Input;
    use kako_core::manifest::write_manifest;
    use kako_core::stage::Scratch;
    use std::fs;

    #[test]
    fn worker_child_cancel_file_stops_mizuchi() {
        let parent = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        let input = src.path().join("in.csv");
        let mut csv = String::from("x\n");
        for i in 0..4_000 {
            csv.push_str(&format!("{i}\n"));
        }
        fs::write(&input, csv).unwrap();
        let scratch = Scratch::create(parent.path(), "worker-cancel").unwrap();
        let staged = scratch
            .stage_inputs(&[Input {
                name: "in".into(),
                path: input,
                markings: Vec::new(),
                column_markings: Default::default(),
                object_set: String::new(),
                digest: None,
            }])
            .unwrap();
        write_manifest(
            scratch.path(),
            Engine::Mizuchi,
            "COPY (SELECT count(*) AS n FROM in a CROSS JOIN in b) TO 'outputs/one.csv' (HEADER, DELIMITER ',');",
            &staged,
            &[],
        )
        .unwrap();
        let scratch_path = scratch.path().to_path_buf();
        let cancel_path = scratch_path.join(CANCEL_FILE);
        let handle = thread::spawn(move || execute_worker_child(&scratch_path));
        thread::sleep(Duration::from_millis(80));
        fs::write(&cancel_path, b"").unwrap();
        let error = handle.join().expect("join").unwrap_err();
        assert!(
            error.contains("limit:") && error.contains("cancelled"),
            "{error}"
        );
    }

    #[test]
    fn unknown_engine_is_an_error() {
        let parent = tempfile::tempdir().unwrap();
        let scratch = Scratch::create(parent.path(), "unknown").unwrap();
        fs::write(
            scratch.path().join("manifest.json"),
            r#"{"engine":"nope","sql":"SELECT 1","inputs":[]}"#,
        )
        .unwrap();
        let error = execute_in_scratch(scratch.path()).unwrap_err();
        assert!(error.contains("unknown engine"), "{error}");
    }

    #[test]
    fn duckdb_dispatch_writes_output() {
        let parent = tempfile::tempdir().unwrap();
        let src = tempfile::tempdir().unwrap();
        let input = src.path().join("in.csv");
        fs::write(&input, "x\n1\n").unwrap();
        let scratch = Scratch::create(parent.path(), "dispatch").unwrap();
        let staged = scratch
            .stage_inputs(&[Input {
                name: "in".into(),
                path: input,
                markings: Vec::new(),
                column_markings: Default::default(),
                object_set: String::new(),
                digest: None,
            }])
            .unwrap();
        write_manifest(
            scratch.path(),
            Engine::DuckDbSql,
            "COPY (SELECT 1 AS x) TO 'outputs/one.csv' (HEADER, DELIMITER ',');",
            &staged,
            &[],
        )
        .unwrap();
        let version = execute_in_scratch(scratch.path()).unwrap();
        assert_eq!(
            Some(version),
            sql_engine_version(Engine::DuckDbSql).map(Result::unwrap)
        );
        assert!(scratch.outputs_dir().join("one.csv").is_file());
    }

    #[test]
    fn preview_refuses_non_sql_engines() {
        let parent = tempfile::tempdir().unwrap();
        let scratch = Scratch::create(parent.path(), "preview-python").unwrap();
        fs::write(
            scratch.path().join("manifest.json"),
            r#"{"engine":"python","sql":"print(1)","inputs":[],"sample_rows":5}"#,
        )
        .unwrap();
        let error = execute_in_scratch(scratch.path()).unwrap_err();
        assert!(error.contains("preview supports duckdb_sql"), "{error}");
    }
}
