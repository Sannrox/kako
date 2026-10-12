//! kekkai backend selection, doctor, and sandboxed spawn.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use kekkai::{
    Backend, CaptureKeep, CaptureLimits, CgroupLimits, ChildUsage, Health, Limits, Network,
    Sandbox, Settings,
};

/// Resource usage kako records on a receipt. Translated from kekkai so
/// callers do not name sandbox-crate types.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxUsage {
    pub cpu_usec: Option<u64>,
    pub memory_peak_bytes: Option<u64>,
    pub oom_killed: bool,
}

fn sandbox_usage(usage: ChildUsage) -> SandboxUsage {
    SandboxUsage {
        cpu_usec: usage.cpu_usec,
        memory_peak_bytes: usage.memory_peak_bytes,
        oom_killed: usage.oom_killed,
    }
}

use kako_core::Failure;
use kako_core::isolation::{self, IsolationTier};

/// Stdout/stderr cap for guest capture. The worker prints an engine version
/// line; errors stay bounded so a noisy child cannot fill the parent.
const GUEST_CAPTURE: CaptureLimits = CaptureLimits {
    stdout_max: Some(4 * 1024 * 1024),
    stderr_max: Some(4 * 1024 * 1024),
    keep: CaptureKeep::Head,
};

/// SIGTERM grace before SIGKILL on a guest wall-clock timeout.
const GUEST_TERM_GRACE: Duration = Duration::from_secs(1);

/// Isolation used for a job that omitted `isolation` (the `linux_native` floor).
pub fn select_backend() -> Result<IsolationTier, Failure> {
    select_backend_for(IsolationTier::LinuxNative)
}

/// Map a job's isolation floor onto an applied tier. Never returns a weaker
/// tier than `required`.
///
/// `linux_native` and `userspace` are health-checked. `kvm` stays a runner
/// failure until kekkai ships that adapter. A missing guest helper never
/// becomes `linux_native`.
pub fn select_backend_for(required: IsolationTier) -> Result<IsolationTier, Failure> {
    let linux = settings(Backend::LinuxNative, None, None, extra_readable_roots(&[]));
    let linux_health = kekkai::health(&linux);
    let userspace = settings(Backend::Userspace, None, None, Vec::new());
    let userspace_health = kekkai::health(&userspace);
    let mut available = Vec::new();
    if matches!(linux_health, Health::Ok(_)) {
        available.push(IsolationTier::LinuxNative);
    }
    if matches!(userspace_health, Health::Ok(_)) {
        available.push(IsolationTier::Userspace);
    }
    match isolation::select_tier(required, &available) {
        Ok(IsolationTier::LinuxNative) => Ok(IsolationTier::LinuxNative),
        Ok(IsolationTier::Userspace) => Ok(IsolationTier::Userspace),
        Ok(IsolationTier::Kvm) => Err(Failure::Runner(
            "sandbox backend kvm cannot be applied".into(),
        )),
        Err(failure) => {
            if required == IsolationTier::LinuxNative
                && let Health::Warning(message) | Health::Error(message) = linux_health
            {
                return Err(Failure::Runner(format!(
                    "sandbox backend linux_native cannot be applied: {message}"
                )));
            }
            if required == IsolationTier::Userspace
                && let Health::Warning(message) | Health::Error(message) = userspace_health
            {
                return Err(Failure::Runner(format!(
                    "sandbox backend userspace cannot be applied: {message}"
                )));
            }
            Err(failure)
        }
    }
}

fn backend_for(isolation: IsolationTier) -> Result<Backend, Failure> {
    match isolation {
        IsolationTier::LinuxNative => Ok(Backend::LinuxNative),
        IsolationTier::Userspace => Ok(Backend::Userspace),
        IsolationTier::Kvm => Err(Failure::Runner(
            "sandbox backend kvm cannot be applied".into(),
        )),
    }
}

/// Copy the worker into scratch for `userspace`; otherwise return `worker`.
pub fn stage_worker(
    worker: &Path,
    scratch: &Path,
    isolation: IsolationTier,
) -> Result<PathBuf, Failure> {
    if isolation == IsolationTier::Userspace {
        stage_guest_worker(worker, scratch)
    } else {
        Ok(worker.to_path_buf())
    }
}

/// Whether the host worker binary must be Landlock-readable (`linux_native`).
pub fn host_worker_readable(isolation: IsolationTier) -> bool {
    isolation != IsolationTier::Userspace
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
        extra_readable_roots(&[]),
    ));
    let rlimit = kekkai::health(&settings(Backend::Rlimit, None, None, Vec::new()));
    let userspace = kekkai::health(&settings(Backend::Userspace, None, None, Vec::new()));
    format!(
        "kako doctor\n\
         backend_selected: {selected}\n\
         rlimit: {rlimit_avail}\n\
         landlock_abi: {abi}\n\
         seccomp: {seccomp}\n\
         cgroup: {cgroup}\n\
         linux_native_health: {linux}\n\
         userspace_health: {userspace}\n\
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
        userspace = health_line(&userspace),
        rlimit = health_line(&rlimit),
    )
}

fn health_line(health: &Health) -> String {
    match health {
        Health::Ok(message) => format!("ok {message}"),
        Health::Warning(message) => format!("warning {message}"),
        Health::Error(message) => format!("error {message}"),
        _ => "unrecognized".into(),
    }
}

/// Delegated cgroup v2 directory from `KAKO_CGROUP_ROOT`. CI creates one
/// because the hosted runner's own cgroup is not writable.
fn cgroup_root() -> Option<PathBuf> {
    match std::env::var_os("KAKO_CGROUP_ROOT") {
        Some(value) if !value.is_empty() => Some(PathBuf::from(value)),
        _ => None,
    }
}

fn settings(
    backend: Backend,
    cpu_time_secs: Option<u64>,
    memory_bytes: Option<u64>,
    readable_roots: Vec<PathBuf>,
) -> Settings {
    let builder = Settings::builder()
        .backend(backend)
        .limits(Limits {
            cpu_time_secs,
            // RLIMIT_AS; unset because it breaks numeric engines (ADR 0002).
            memory_mb: None,
            user_processes: None,
            file_size_mb: None,
            open_files: None,
        })
        .readable_roots(readable_roots)
        .network(Network::Deny);
    if matches!(backend, Backend::Userspace) {
        // Guest spawn does not apply cgroup limits (kekkai Invalid).
        builder.build()
    } else {
        builder
            .cgroup(CgroupLimits {
                memory_max: memory_bytes,
                ..CgroupLimits::default()
            })
            .cgroup_root(cgroup_root())
            .build()
    }
}

fn extra_readable_roots(extra: &[PathBuf]) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        push_executable(&mut roots, &exe);
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
    for path in extra {
        if too_broad(path) {
            continue;
        }
        if path.is_file() {
            push_executable(&mut roots, path);
        } else {
            push_existing(&mut roots, path);
        }
    }
    roots.sort();
    roots.dedup();
    roots
}

fn too_broad(path: &Path) -> bool {
    path == Path::new("/") || path == Path::new("/usr") || path == Path::new("/sys")
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

/// Copy the worker into `scratch` so a guest payload lives on the workspace
/// mount. Never hard-link: scratch is writable.
pub(crate) fn stage_guest_worker(worker: &Path, scratch: &Path) -> Result<PathBuf, Failure> {
    let dest = scratch.join("kako-worker");
    fs::copy(worker, &dest)
        .map_err(|error| Failure::Runner(format!("copy guest worker: {error}")))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&dest)
            .map_err(|error| Failure::Runner(format!("guest worker metadata: {error}")))?
            .permissions();
        perms.set_mode(perms.mode() | 0o111);
        fs::set_permissions(&dest, perms)
            .map_err(|error| Failure::Runner(format!("guest worker permissions: {error}")))?;
    }
    Ok(dest)
}

/// Spawn `command` under kekkai, wait up to `wall_clock`, kill the group on timeout.
///
/// `extra_readable` grants the worker binary and interpreter prefixes under
/// `linux_native`. `/usr` and `/sys` are skipped. `userspace` takes those
/// extra paths as kekkai `--readable` only (Python interpreter prefixes);
/// the workspace is the per-run scratch directory. `apply()` is invalid for
/// `userspace`; spawn goes through the guest helper.
pub fn run_sandboxed(
    command: Command,
    scratch: &Path,
    isolation: IsolationTier,
    wall_clock: Duration,
    cpu_time_secs: Option<u64>,
    memory_bytes: Option<u64>,
    extra_readable: &[PathBuf],
) -> Result<(ExitStatus, Vec<u8>, Vec<u8>, SandboxUsage), Failure> {
    let backend = backend_for(isolation)?;
    run_sandboxed_on(
        command,
        scratch,
        backend,
        wall_clock,
        cpu_time_secs,
        memory_bytes,
        extra_readable,
    )
}

fn run_sandboxed_on(
    mut command: Command,
    scratch: &Path,
    backend: Backend,
    wall_clock: Duration,
    cpu_time_secs: Option<u64>,
    memory_bytes: Option<u64>,
    extra_readable: &[PathBuf],
) -> Result<(ExitStatus, Vec<u8>, Vec<u8>, SandboxUsage), Failure> {
    Instant::now()
        .checked_add(wall_clock)
        .ok_or_else(|| Failure::Runner("wall-clock timeout overflows the runner clock".into()))?;
    if matches!(backend, Backend::Userspace) && memory_bytes.is_some() {
        return Err(Failure::Runner(
            "memory limit cannot be set: cgroup limits are not applied by backend userspace".into(),
        ));
    }
    let readable = match backend {
        Backend::LinuxNative => extra_readable_roots(extra_readable),
        Backend::Userspace => guest_readable_roots(extra_readable),
        _ => Vec::new(),
    };
    let prepared = settings(backend, cpu_time_secs, memory_bytes, readable);
    if memory_bytes.is_some()
        && let Health::Error(message) = kekkai::health(&prepared)
    {
        return Err(Failure::Runner(format!(
            "memory limit cannot be set: {message}"
        )));
    }
    let sandbox = Sandbox::prepare(&prepared, scratch).map_err(|error| {
        if matches!(backend, Backend::Userspace) {
            Failure::Runner(format!(
                "sandbox backend userspace cannot be applied: {error}"
            ))
        } else {
            Failure::Runner(format!("sandbox cannot be applied: {error}"))
        }
    })?;
    command.current_dir(scratch);
    let tmp = scratch.join("tmp");
    command.env("HOME", scratch);
    command.env("TMPDIR", &tmp);
    command.env("TMP", &tmp);
    command.env("TEMP", &tmp);
    if matches!(backend, Backend::Userspace) {
        return run_userspace(sandbox, command, wall_clock);
    }
    run_in_process(sandbox, command, wall_clock)
}

fn guest_readable_roots(extra: &[PathBuf]) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for path in extra {
        if too_broad(path) {
            continue;
        }
        if path.is_file() {
            push_executable(&mut roots, path);
        } else {
            push_existing(&mut roots, path);
        }
    }
    roots.sort();
    roots.dedup();
    roots
}

fn run_userspace(
    sandbox: Sandbox,
    mut command: Command,
    wall_clock: Duration,
) -> Result<(ExitStatus, Vec<u8>, Vec<u8>, SandboxUsage), Failure> {
    let (status, usage, stdout, stderr) = sandbox
        .spawn_with_deadline_captured(&mut command, wall_clock, GUEST_TERM_GRACE, GUEST_CAPTURE)
        .map_err(|error| {
            Failure::Runner(format!(
                "sandbox backend userspace cannot be applied: {error}"
            ))
        })?;
    if usage.timed_out {
        return Err(Failure::Limit(format!(
            "wall-clock timeout after {}ms",
            wall_clock.as_millis()
        )));
    }
    Ok((status, stdout.bytes, stderr.bytes, sandbox_usage(usage)))
}

fn run_in_process(
    sandbox: Sandbox,
    mut command: Command,
    wall_clock: Duration,
) -> Result<(ExitStatus, Vec<u8>, Vec<u8>, SandboxUsage), Failure> {
    let deadline = Instant::now()
        .checked_add(wall_clock)
        .ok_or_else(|| Failure::Runner("wall-clock timeout overflows the runner clock".into()))?;
    sandbox
        .apply(&mut command)
        .map_err(|error| Failure::Runner(format!("sandbox cannot be applied: {error}")))?;
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
    Ok((status, stdout, stderr, sandbox_usage(usage)))
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
    use std::fs;

    #[test]
    fn guest_backend_type_is_nameable() {
        let settings = Settings::builder().backend(Backend::Userspace).build();
        assert_eq!(settings.backend, Backend::Userspace);
        assert_eq!(settings.backend.to_string(), "userspace");
    }

    #[test]
    fn extra_readable_roots_do_not_grant_sys() {
        let roots = extra_readable_roots(&[PathBuf::from("/usr"), PathBuf::from("/sys")]);
        for root in &roots {
            assert_ne!(root.as_path(), Path::new("/sys"), "{roots:?}");
            assert_ne!(root.as_path(), Path::new("/usr"), "{roots:?}");
        }
    }

    #[test]
    fn extra_readable_roots_include_proc_when_present() {
        let proc = Path::new("/proc");
        if !proc.is_dir() {
            return;
        }
        let roots = extra_readable_roots(&[]);
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
        let roots = extra_readable_roots(&[]);
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
        assert!(report.contains("userspace_health:"), "{report}");
        match select_backend() {
            Ok(tier) => {
                assert_eq!(tier, IsolationTier::LinuxNative);
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

    fn userspace_health_ok() -> bool {
        matches!(
            kekkai::health(&settings(Backend::Userspace, None, None, Vec::new())),
            Health::Ok(_)
        )
    }

    #[test]
    fn userspace_floor_does_not_select_linux_native() {
        match select_backend_for(IsolationTier::Userspace) {
            Ok(IsolationTier::Userspace) => {}
            Err(Failure::Runner(message)) => {
                assert!(message.contains("userspace"), "{message}");
            }
            other => panic!("unexpected backend {other:?}"),
        }
    }

    #[test]
    fn userspace_run_fails_closed_without_helper() {
        if userspace_health_ok() {
            return;
        }
        let scratch = tempfile::tempdir().unwrap();
        let command = Command::new("true");
        let error = run_sandboxed(
            command,
            scratch.path(),
            IsolationTier::Userspace,
            Duration::from_secs(1),
            None,
            None,
            &[],
        )
        .unwrap_err();
        assert!(
            matches!(error, Failure::Runner(ref message) if message.contains("userspace")),
            "{error}"
        );
    }

    #[test]
    fn userspace_memory_cap_fails_closed() {
        let scratch = tempfile::tempdir().unwrap();
        let command = Command::new("true");
        let error = run_sandboxed(
            command,
            scratch.path(),
            IsolationTier::Userspace,
            Duration::from_secs(1),
            None,
            Some(1024 * 1024),
            &[],
        )
        .unwrap_err();
        assert!(
            matches!(error, Failure::Runner(ref message) if message.contains("memory limit")),
            "{error}"
        );
    }

    #[test]
    fn stage_guest_worker_copies_into_scratch() {
        let scratch = tempfile::tempdir().unwrap();
        let src = scratch.path().join("src-bin");
        fs::write(&src, b"#!/bin/sh\n").unwrap();
        let dest = stage_guest_worker(&src, scratch.path()).unwrap();
        assert_eq!(dest, scratch.path().join("kako-worker"));
        assert_eq!(fs::read(&dest).unwrap(), b"#!/bin/sh\n");
        assert_ne!(dest, src);
    }

    /// Requires Linux, `linux_native` health, and `kekkai-guest` on PATH or
    /// `KEKKAI_GUEST_HELPER`. Offline `cargo test` skips this.
    #[test]
    #[ignore = "needs kekkai-guest helper; set KEKKAI_GUEST_HELPER"]
    fn guest_payload_cannot_read_a_host_secret() {
        let scratch = tempfile::tempdir().unwrap();
        let secret_dir = tempfile::tempdir().unwrap();
        let secret = secret_dir.path().join("credentials");
        fs::write(&secret, "guest-escape-token").unwrap();
        let mut command = Command::new("cat");
        command.arg(&secret);
        let (status, stdout, stderr, _) = run_sandboxed(
            command,
            scratch.path(),
            IsolationTier::Userspace,
            Duration::from_secs(5),
            None,
            None,
            &[],
        )
        .expect("guest helper must spawn");
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        );
        assert!(
            !combined.contains("guest-escape-token"),
            "guest read host secret: {combined}"
        );
        assert!(!status.success(), "cat of host secret must fail");
    }

    /// Requires Linux, `linux_native` health, and `kekkai-guest` on PATH or
    /// `KEKKAI_GUEST_HELPER`. Offline `cargo test` skips this.
    #[test]
    #[ignore = "needs kekkai-guest helper; set KEKKAI_GUEST_HELPER"]
    fn guest_payload_cannot_read_another_run_directory() {
        let scratch = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let leaked = other.path().join("other-run.txt");
        fs::write(&leaked, "other-run-bytes").unwrap();
        let mut command = Command::new("cat");
        command.arg(&leaked);
        let (status, stdout, stderr, _) = run_sandboxed(
            command,
            scratch.path(),
            IsolationTier::Userspace,
            Duration::from_secs(5),
            None,
            None,
            &[],
        )
        .expect("guest helper must spawn");
        let combined = format!(
            "{}{}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        );
        assert!(
            !combined.contains("other-run-bytes"),
            "guest read another run: {combined}"
        );
        assert!(!status.success(), "cat of another run must fail");
    }

    #[cfg(unix)]
    #[test]
    fn wall_clock_timeout_kills_a_child() {
        let scratch = tempfile::tempdir().unwrap();
        let mut command = Command::new("sleep");
        command.arg("30");
        let error = run_sandboxed_on(
            command,
            scratch.path(),
            Backend::Rlimit,
            Duration::from_millis(400),
            None,
            None,
            &[],
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
        let error = run_sandboxed_on(
            command,
            scratch.path(),
            Backend::Rlimit,
            Duration::MAX,
            None,
            None,
            &[],
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
        let error = run_sandboxed_on(
            command,
            scratch.path(),
            Backend::Rlimit,
            Duration::from_secs(1),
            None,
            Some(1024 * 1024),
            &[],
        )
        .unwrap_err();
        assert!(
            matches!(error, Failure::Runner(ref message) if message.contains("memory limit")),
            "{error}"
        );
    }
}
