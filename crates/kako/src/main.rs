//! kako (加工): sandboxed data-transform runner.

use std::path::{Path, PathBuf};
use std::process;

const USAGE: &str = "\
kako: sandboxed data-transform runner

USAGE:
    kako run <job.json> [--output <dir>] [--tenant <id>] [--namespace <ns>] [--require-identity]
    kako doctor
    kako [--version | --help]

`kako run` reads a job file (SQL or Python, named input files, expected
outputs), runs it in a kekkai sandbox, and writes outputs plus receipt.json
to the output directory (default: ./out). Jobs carry tenant_id and namespace
(default local/default). --require-identity refuses a job file that omits
either field. Python jobs set \"engine\": \"python\" and \"code\", and may pin \"runtime_image\".";

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("--version" | "-V") => println!("kako {}", env!("CARGO_PKG_VERSION")),
        Some("--help" | "-h") | None => println!("{USAGE}"),
        Some("--internal-execute") => {
            let scratch = args.next().unwrap_or_default();
            if scratch.is_empty() {
                eprintln!("kako: --internal-execute requires a scratch directory");
                process::exit(2);
            }
            match kako::engine::execute_in_scratch(Path::new(&scratch)) {
                Ok(version) => println!("{version}"),
                Err(error) => {
                    eprintln!("{error}");
                    process::exit(if error.starts_with("limit:") { 3 } else { 1 });
                }
            }
        }
        Some("--internal-probe") => match args.next().as_deref() {
            Some("network") => process::exit(kako::sandbox::probe_network_exit()),
            Some(other) => {
                eprintln!("kako: unknown probe `{other}`");
                process::exit(2);
            }
            None => {
                eprintln!("kako: --internal-probe requires a probe name");
                process::exit(2);
            }
        },
        Some("doctor") => print!("{}", kako::sandbox::doctor_report()),
        Some("run") => {
            if let Err(error) = cmd_run(args) {
                eprintln!("kako: {error}");
                process::exit(1);
            }
        }
        Some(other) => {
            eprintln!("kako: unknown argument `{other}`\n\n{USAGE}");
            process::exit(2);
        }
    }
}

fn cmd_run(args: impl Iterator<Item = String>) -> Result<(), String> {
    let mut job_file: Option<PathBuf> = None;
    let mut output = PathBuf::from("out");
    let mut identity = kako::local::LocalIdentity::default();
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--output" | "-o" => {
                let dir = args
                    .next()
                    .ok_or_else(|| "missing value for --output".to_string())?;
                output = PathBuf::from(dir);
            }
            "--tenant" => {
                identity.tenant_id = args
                    .next()
                    .ok_or_else(|| "missing value for --tenant".to_string())?;
            }
            "--namespace" => {
                identity.namespace = args
                    .next()
                    .ok_or_else(|| "missing value for --namespace".to_string())?;
            }
            "--require-identity" => identity.require_job_identity = true,
            s if s.starts_with('-') => return Err(format!("unknown flag `{s}`")),
            s => {
                if job_file.is_some() {
                    return Err("multiple job files".into());
                }
                job_file = Some(PathBuf::from(s));
            }
        }
    }
    let job_file = job_file.ok_or_else(|| "missing job file".to_string())?;
    let mut source = kako::local::LocalSource::from_job_file_with(&job_file, output, identity)
        .map_err(|error| error.to_string())?;
    let scratch_parent = std::env::temp_dir();
    let config = kako::run::RunnerConfig::for_current_exe(scratch_parent)
        .map_err(|error| error.to_string())?;
    let finishes =
        kako::run::run_source(&mut source, &config).map_err(|error| error.to_string())?;
    for finish in finishes {
        if let kako::run::JobFinish::Failed { job_id, failure } = finish {
            return Err(format!("{job_id}: {failure}"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn version_is_set() {
        assert!(!env!("CARGO_PKG_VERSION").is_empty());
    }
}
