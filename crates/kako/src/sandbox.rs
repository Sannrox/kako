//! kekkai backend selection, doctor, and sandboxed spawn.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use kekkai::{Backend, Health, Limits, Network, Sandbox, Settings};

use crate::Failure;

/// Backend used for `kako run`. Only `linux_native` confines writes and
/// denies network; anything weaker is a runner failure.
pub fn select_backend() -> Result<Backend, Failure> {
    let linux = settings(Backend::LinuxNative, None, extra_readable_roots(None));
    match kekkai::health(&linux) {
        Health::Ok(_) => Ok(Backend::LinuxNative),
        Health::Warning(message) | Health::Error(message) => Err(Failure::Runner(format!(
            "sandbox backend linux_native cannot be applied: {message}"
        ))),
    }
}

/// Text report of host capabilities and the backend `kako run` would use.
pub fn doctor_report() -> String {
    let caps = kekkai::capabilities();
    let selected = match select_backend() {
        Ok(backend) => backend.to_string(),
        Err(Failure::Runner(message)) => format!("none ({message})"),
        Err(other) => format!("none ({other})"),
    };
    let linux = kekkai::health(&settings(
        Backend::LinuxNative,
        None,
        extra_readable_roots(None),
    ));
    let rlimit = kekkai::health(&settings(Backend::Rlimit, None, Vec::new()));
    format!(
        "kako doctor\n\
         backend_selected: {selected}\n\
         rlimit: {rlimit_avail}\n\
         landlock_abi: {abi}\n\
         seccomp: {seccomp}\n\
         cgroup: {cgroup}\n\
         linux_native_health: {linux}\n\
         rlimit_health: {rlimit}\n",
        rlimit_avail = if caps.rlimit {
            "available"
        } else {
            "unavailable"
        },
        abi = caps
            .landlock_abi
            .map(|n| n.to_string())
            .unwrap_or_else(|| "none".into()),
        seccomp = if caps.seccomp {
            "available"
        } else {
            "unavailable"
        },
        cgroup = if caps.cgroup {
            "available"
        } else {
            "unavailable"
        },
        linux = health_line(&linux),
        rlimit = health_line(&rlimit),
    )
}

fn health_line(health: &Health) -> String {
    match health {
        Health::Ok(message) => format!("ok {message}"),
        Health::Warning(message) => format!("warning {message}"),
        Health::Error(message) => format!("error {message}"),
    }
}

fn settings(
    backend: Backend,
    cpu_time_secs: Option<u64>,
    readable_roots: Vec<PathBuf>,
) -> Settings {
    Settings {
        backend,
        limits: Limits {
            cpu_time_secs,
            // RLIMIT_AS; unset because it breaks numeric engines (ADR 0002).
            memory_mb: None,
            user_processes: None,
            file_size_mb: None,
            open_files: None,
        },
        readable_roots,
        writable_roots: Vec::new(),
        network: Network::Deny,
        cgroup: kekkai::CgroupLimits::default(),
        cgroup_root: None,
    }
}

fn extra_readable_roots(worker: Option<&Path>) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    let mut add = |path: &Path| {
        if let Ok(canon) = fs_canonicalize(path) {
            if let Some(parent) = canon.parent() {
                roots.push(parent.to_path_buf());
            }
            roots.push(canon);
        } else {
            roots.push(path.to_path_buf());
        }
    };
    if let Ok(exe) = std::env::current_exe() {
        add(&exe);
    }
    if let Some(worker) = worker {
        add(worker);
    }
    roots.sort();
    roots.dedup();
    roots
}

fn fs_canonicalize(path: &Path) -> io::Result<PathBuf> {
    path.canonicalize()
}

/// Spawn `command` under kekkai, wait up to `wall_clock`, kill the group on timeout.
///
/// `worker` is granted read/execute under `linux_native` so the child binary
/// can be loaded even when it is not under `/usr`.
pub fn run_sandboxed(
    mut command: Command,
    scratch: &Path,
    backend: Backend,
    wall_clock: Duration,
    cpu_time_secs: Option<u64>,
    worker: &Path,
) -> Result<(ExitStatus, Vec<u8>, Vec<u8>), Failure> {
    let deadline = Instant::now()
        .checked_add(wall_clock)
        .ok_or_else(|| Failure::Runner("wall-clock timeout overflows the runner clock".into()))?;
    let readable = if matches!(backend, Backend::LinuxNative) {
        extra_readable_roots(Some(worker))
    } else {
        Vec::new()
    };
    let prepared = settings(backend, cpu_time_secs, readable);
    let sandbox = Sandbox::prepare(&prepared, scratch)
        .map_err(|error| Failure::Runner(format!("sandbox cannot be applied: {error}")))?;
    sandbox
        .apply(&mut command)
        .map_err(|error| Failure::Runner(format!("sandbox cannot be applied: {error}")))?;
    command.current_dir(scratch);
    command.env("HOME", scratch);
    command.env("TMPDIR", scratch);
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());

    let mut child = command
        .spawn()
        .map_err(|error| Failure::Runner(format!("spawn worker: {error}")))?;
    let pid = child.id();
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let stdout_thread = thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut pipe) = stdout_pipe.take() {
            let _ = io::Read::read_to_end(&mut pipe, &mut buf);
        }
        buf
    });
    let stderr_thread = thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut pipe) = stderr_pipe.take() {
            let _ = io::Read::read_to_end(&mut pipe, &mut buf);
        }
        buf
    });
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                sandbox.kill_process_group(Some(pid));
                let _ = child.kill();
                let _ = child.wait();
                let _ = sandbox.finish();
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                return Err(Failure::Limit(format!(
                    "wall-clock timeout after {}ms",
                    wall_clock.as_millis()
                )));
            }
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(error) => {
                sandbox.kill_process_group(Some(pid));
                let _ = child.kill();
                let _ = sandbox.finish();
                let _ = stdout_thread.join();
                let _ = stderr_thread.join();
                return Err(Failure::Runner(format!("wait for worker: {error}")));
            }
        }
    };
    let _ = sandbox.finish();
    let stdout = stdout_thread.join().unwrap_or_default();
    let stderr = stderr_thread.join().unwrap_or_default();
    Ok((status, stdout, stderr))
}

/// Exit code for `--internal-probe network`: 0 allowed, 2 denied, 1 other error.
pub fn probe_network_exit() -> i32 {
    #[cfg(unix)]
    {
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM, 0) };
        if fd < 0 {
            let err = io::Error::last_os_error();
            match err.raw_os_error() {
                Some(code) if code == libc::EPERM || code == libc::EACCES => return 2,
                _ => {
                    eprintln!("socket: {err}");
                    return 1;
                }
            }
        }
        unsafe {
            libc::close(fd);
        }
        0
    }
    #[cfg(not(unix))]
    {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doctor_names_a_backend() {
        let report = doctor_report();
        assert!(report.contains("backend_selected:"), "{report}");
        assert!(report.contains("linux_native_health:"), "{report}");
        match select_backend() {
            Ok(backend) => {
                assert_eq!(backend, Backend::LinuxNative);
                assert!(
                    report.contains("backend_selected: linux_native"),
                    "{report}"
                );
            }
            Err(Failure::Runner(message)) => {
                assert!(message.contains("linux_native"), "{message}");
                assert!(report.contains("backend_selected: none"), "{report}");
            }
            Err(other) => panic!("unexpected doctor failure: {other}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn wall_clock_timeout_kills_a_child() {
        let scratch = tempfile::tempdir().unwrap();
        let mut command = Command::new("sleep");
        command.arg("30");
        let error = run_sandboxed(
            command,
            scratch.path(),
            Backend::Rlimit,
            Duration::from_millis(400),
            None,
            Path::new("/bin/sleep"),
        )
        .unwrap_err();
        assert!(
            matches!(error, Failure::Limit(ref message) if message.contains("timeout")),
            "{error}"
        );
    }

    #[test]
    fn overflow_wall_clock_is_a_runner_error() {
        let scratch = tempfile::tempdir().unwrap();
        let command = Command::new("true");
        let error = run_sandboxed(
            command,
            scratch.path(),
            Backend::Rlimit,
            Duration::MAX,
            None,
            Path::new("/bin/true"),
        )
        .unwrap_err();
        assert!(
            matches!(error, Failure::Runner(ref message) if message.contains("overflows")),
            "{error}"
        );
    }
}
