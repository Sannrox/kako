//! In-process engine dispatch for `kako --internal-execute`.

use std::path::Path;

use kako_core::WriteMode;
use kako_core::manifest::read_manifest;

#[cfg(test)]
mod ceiling;

/// Run the staged job inside this process. Prints nothing; returns engine version.
pub fn execute_in_scratch(scratch: &Path) -> Result<String, String> {
    let manifest = read_manifest(scratch)?;
    let write_mode = manifest.write_mode()?;
    match manifest.engine.as_deref().unwrap_or("duckdb_sql") {
        "duckdb_sql" | "sql" => {
            if write_mode != WriteMode::Overwrite {
                return Err("write_mode is only valid for mizuchi jobs".into());
            }
            kako_engine_duckdb::execute(&manifest.sql, &manifest.inputs, scratch)
        }
        "mizuchi" => {
            kako_engine_mizuchi::execute(&manifest.sql, &manifest.inputs, scratch, write_mode)
        }
        "python" => {
            if write_mode != WriteMode::Overwrite {
                return Err("write_mode is only valid for mizuchi jobs".into());
            }
            kako_engine_python::execute(&manifest.sql, &manifest.inputs, &manifest.outputs, scratch)
        }
        other => Err(format!("unknown engine `{other}`")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kako_core::Engine;
    use kako_core::Input;
    use kako_core::manifest::write_manifest;
    use kako_core::stage::Scratch;
    use std::fs;

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
        assert!(version.starts_with("duckdb "), "{version}");
        assert!(scratch.outputs_dir().join("one.csv").is_file());
    }
}
