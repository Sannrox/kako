//! kekkai backend selection, doctor, and sandboxed spawn.

use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use kekkai::{Backend, CgroupLimits, ChildUsage, Health, Limits, Network, Sandbox, Settings};

use crate::Failure;

/// Backend used for `kako run`. Only `linux_native` confines writes and
/// denies network; anything weaker is a runner failure.
pub fn select_backend() -> Result<Backend, Failure> {
    let linux = settings(Backend::LinuxNative, None, None, extra_readable_roots(None));
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
        None,
        extra_readable_roots(None),
    ));
    let rlimit = kekkai::health(&settings(Backend::Rlimit, None, None, Vec::new()));
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
    memory_bytes: Option<u64>,
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
        cgroup: CgroupLimits {
            memory_max: memory_bytes,
            ..CgroupLimits::default()
        },
        cgroup_root: None,
    }
}

fn extra_readable_roots(worker: Option<&Path>) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        push_executable(&mut roots, &exe);
    }
    if let Some(worker) = worker {
        push_executable(&mut roots, worker);
    }
    // The engine child reads `/proc/self/cgroup`. That path is the child's
    // pid, which the parent cannot name when it builds the ruleset, so the
    // `/proc` directory is granted as a whole. Do not grant `/sys`.
    // User SQL cannot follow this grant: the engine allow-lists only the
    // scratch directory and then disables DuckDB external access.
    if Path::new("/proc").is_dir() {
        push_existing(&mut roots, Path::new("/proc"));
    }
    for path in [
        Path::new("/sys/devices/system/cpu"),
        Path::new("/sys/fs/cgroup"),
    ] {
        if path.exists() {
            push_existing(&mut roots, path);
        }
    }
    roots.sort();
    roots.dedup();
    roots
}

fn push_existing(roots: &mut Vec<PathBuf>, path: &Path) {
    if let Ok(canon) = fs_canonicalize(path) {
        roots.push(canon);
    } else {
        roots.push(path.to_path_buf());
    }
}

/// Landlock needs the directory that holds the worker binary. Do not apply
/// that parent rule to `/proc/*`: the parent is `/proc`, which includes
/// `/proc/self/root`.
fn push_executable(roots: &mut Vec<PathBuf>, path: &Path) {
    if let Ok(canon) = fs_canonicalize(path) {
        if let Some(parent) = canon.parent() {
            roots.push(parent.to_path_buf());
        }
        roots.push(canon);
    } else {
        roots.push(path.to_path_buf());
    }
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
    memory_bytes: Option<u64>,
    worker: &Path,
) -> Result<(ExitStatus, Vec<u8>, Vec<u8>, ChildUsage), Failure> {
    let deadline = Instant::now()
        .checked_add(wall_clock)
        .ok_or_else(|| Failure::Runner("wall-clock timeout overflows the runner clock".into()))?;
    let readable = if matches!(backend, Backend::LinuxNative) {
        extra_readable_roots(Some(worker))
    } else {
        Vec::new()
    };
    let prepared = settings(backend, cpu_time_secs, memory_bytes, readable);
    if memory_bytes.is_some()
        && let Health::Error(message) = kekkai::health(&prepared)
    {
        return Err(Failure::Runner(format!(
            "memory limit cannot be set: {message}"
        )));
    }
    let sandbox = Sandbox::prepare(&prepared, scratch)
        .map_err(|error| Failure::Runner(format!("sandbox cannot be applied: {error}")))?;
    sandbox
        .apply(&mut command)
        .map_err(|error| Failure::Runner(format!("sandbox cannot be applied: {error}")))?;
    command.current_dir(scratch);
    let tmp = scratch.join("tmp");
    command.env("HOME", scratch);
    command.env("TMPDIR", &tmp);
    command.env("TMP", &tmp);
    command.env("TEMP", &tmp);
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
    let usage = sandbox.finish();
    let stdout = stdout_thread.join().unwrap_or_default();
    let stderr = stderr_thread.join().unwrap_or_default();
    Ok((status, stdout, stderr, usage))
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
    fn extra_readable_roots_do_not_grant_sys() {
        let roots = extra_readable_roots(None);
        for root in &roots {
            assert_ne!(root.as_path(), Path::new("/sys"), "{roots:?}");
        }
    }

    #[test]
    fn extra_readable_roots_include_proc_when_present() {
        let proc = Path::new("/proc");
        if !proc.is_dir() {
            return;
        }
        let roots = extra_readable_roots(None);
        let granted = roots.iter().any(|root| {
            root == proc
                || fs_canonicalize(proc)
                    .map(|canon| root == &canon)
                    .unwrap_or(false)
        });
        assert!(granted, "expected {proc:?} in {roots:?}");
    }

    #[test]
    fn extra_readable_roots_include_cgroup_when_present() {
        let cgroup = Path::new("/sys/fs/cgroup");
        if !cgroup.exists() {
            return;
        }
        let roots = extra_readable_roots(None);
        let granted = roots.iter().any(|root| {
            root == cgroup
                || fs_canonicalize(cgroup)
                    .map(|canon| root == &canon)
                    .unwrap_or(false)
        });
        assert!(granted, "expected {cgroup:?} in {roots:?}");
    }

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
            None,
            Path::new("/bin/true"),
        )
        .unwrap_err();
        assert!(
            matches!(error, Failure::Runner(ref message) if message.contains("overflows")),
            "{error}"
        );
    }

    #[test]
    fn memory_limit_fails_closed_when_cgroup_unavailable() {
        let scratch = tempfile::tempdir().unwrap();
        let prepared = settings(Backend::Rlimit, None, Some(1024 * 1024), Vec::new());
        if matches!(kekkai::health(&prepared), Health::Ok(_)) {
            return;
        }
        let command = Command::new("true");
        let error = run_sandboxed(
            command,
            scratch.path(),
            Backend::Rlimit,
            Duration::from_secs(1),
            None,
            Some(1024 * 1024),
            Path::new("/bin/true"),
        )
        .unwrap_err();
        assert!(
            matches!(error, Failure::Runner(ref message) if message.contains("memory limit")),
            "{error}"
        );
    }
}
