//! kako (加工): sandboxed data-transform runner.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process;

const USAGE: &str = "\
kako: sandboxed data-transform runner

USAGE:
    kako run <job.json> [--output <dir>] [--tenant <id>] [--namespace <ns>] [--require-identity]
    kako validate <job.json>
    kako preview <job.json> [--rows <n>]
    kako serve --listen <addr> [--hand-back] [--idle-ms <n>] [<job.json>] [--output <dir>] [--tenant <id>] [--namespace <ns>] [--require-identity]
    kako doctor
    kako [--version | --help]

`kako run` reads a job file (SQL or Python, named input files, expected
outputs), runs it in a kekkai sandbox, and writes outputs plus receipt.json
to the output directory (default: ./out). Jobs carry tenant_id and namespace
(default local/default). --require-identity refuses a job file that omits
either field. Python jobs set \"engine\": \"python\" and \"code\", and may pin \"runtime_image\".
`kako validate` checks a SQL job file and runs the query in the
sandbox over empty inputs typed as a full run types them, so unknown columns, join
keys and type errors are reported with locations. `kako preview` runs a SQL
job on at most --rows rows per input (default 100) and prints each output's
columns and rows with a receipt marked non-committing. Both read inputs only
through their named tables and write no output directory.
`kako serve` exposes /health, /ready, and /version, claims an optional local
job file, and drains on SIGTERM. Without a job file the process stays
not-ready.";

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
            match kako_worker::execute_in_scratch(Path::new(&scratch)) {
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
        Some("validate") => {
            if let Err(error) = cmd_validate(args) {
                eprintln!("kako: {error}");
                process::exit(1);
            }
        }
        Some("preview") => {
            if let Err(error) = cmd_preview(args) {
                eprintln!("kako: {error}");
                process::exit(1);
            }
        }
        Some("serve") => {
            if let Err(error) = cmd_serve(args) {
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

fn cmd_validate(mut args: impl Iterator<Item = String>) -> Result<(), String> {
    let job_file = args.next().ok_or_else(|| "missing job file".to_string())?;
    if let Some(extra) = args.next() {
        return Err(format!("unexpected argument `{extra}`"));
    }
    let job = kako::local::load_job(Path::new(&job_file)).map_err(|error| error.to_string())?;
    let config = kako::run::RunnerConfig::for_current_exe(std::env::temp_dir())
        .map_err(|error| error.to_string())?;
    kako::run::validate_job(&job, &config).map_err(|error| error.to_string())?;
    println!("{job_file}: valid");
    Ok(())
}

fn cmd_preview(args: impl Iterator<Item = String>) -> Result<(), String> {
    let mut job_file: Option<PathBuf> = None;
    let mut rows = 100u64;
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--rows" => {
                rows = args
                    .next()
                    .ok_or_else(|| "missing value for --rows".to_string())?
                    .parse()
                    .map_err(|error| format!("--rows: {error}"))?;
            }
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
    let job = kako::local::load_job(&job_file).map_err(|error| error.to_string())?;
    let config = kako::run::RunnerConfig::for_current_exe(std::env::temp_dir())
        .map_err(|error| error.to_string())?;
    let preview = kako::run::preview_job(&job, &config, rows).map_err(|error| error.to_string())?;
    println!("{}", preview.to_json()?);
    Ok(())
}

fn cmd_serve(args: impl Iterator<Item = String>) -> Result<(), String> {
    let mut listen: Option<std::net::SocketAddr> = None;
    let mut hand_back = false;
    let mut idle_ms = 200u64;
    let mut job_file: Option<PathBuf> = None;
    let mut output = PathBuf::from("out");
    let mut output_set = false;
    let mut identity = kako::local::LocalIdentity::default();
    let mut args = args.peekable();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--listen" => {
                let value = args
                    .next()
                    .ok_or_else(|| "missing value for --listen".to_string())?;
                listen = Some(
                    value
                        .parse()
                        .map_err(|error| format!("invalid --listen `{value}`: {error}"))?,
                );
            }
            "--hand-back" => hand_back = true,
            "--idle-ms" => {
                let value = args
                    .next()
                    .ok_or_else(|| "missing value for --idle-ms".to_string())?;
                idle_ms = value
                    .parse()
                    .map_err(|error| format!("invalid --idle-ms `{value}`: {error}"))?;
            }
            "--output" | "-o" => {
                let dir = args
                    .next()
                    .ok_or_else(|| "missing value for --output".to_string())?;
                output = PathBuf::from(dir);
                output_set = true;
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
    let listen = listen.ok_or_else(|| "missing --listen".to_string())?;
    if job_file.is_none() && (output_set || identity != kako::local::LocalIdentity::default()) {
        return Err("job identity and --output require a job file".into());
    }
    let probes = std::sync::Arc::new(kako::pool::ProbeState::new(env!("CARGO_PKG_VERSION")));
    if job_file.is_none() {
        probes.mark_not_ready();
    }
    let server = kako::pool::ProbeServer::bind(listen, probes.clone())
        .map_err(|error| format!("bind {listen}: {error}"))?;
    println!("listening on {}", server.addr());
    let _ = std::io::stdout().flush();
    let drain = kako::pool::DrainHandle::new(probes.clone());
    kako::pool::install_process_drain_signals();
    let scratch_parent = std::env::temp_dir();
    let runner = kako::run::RunnerConfig::for_current_exe(scratch_parent)
        .map_err(|error| error.to_string())?;
    let config = kako::pool::ServeConfig {
        runner,
        idle: std::time::Duration::from_millis(idle_ms),
        drain_mode: if hand_back {
            kako::pool::DrainMode::HandBack
        } else {
            kako::pool::DrainMode::FinishCurrent
        },
        not_ready_when_empty: job_file.is_some(),
    };
    match job_file {
        None => {
            let mut source = kako::pool::IdleSource;
            kako::pool::serve_source(&mut source, &config, &probes, &drain)
                .map_err(|error| error.to_string())?;
        }
        Some(job_file) => {
            let mut source =
                kako::local::LocalSource::from_job_file_with(&job_file, output, identity)
                    .map_err(|error| error.to_string())?;
            kako::pool::serve_source(&mut source, &config, &probes, &drain)
                .map_err(|error| error.to_string())?;
        }
    }
    server.shutdown();
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn version_is_set() {
        assert!(!env!("CARGO_PKG_VERSION").is_empty());
    }
}
