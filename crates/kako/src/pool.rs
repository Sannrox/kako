//! Long-running runner pool: probes, drain, and an idle serve loop.
//!
//! `run_source` stops when the source is empty. A pool waits, exposes
//! `/health`, `/ready`, and `/version`, and on drain stops claiming work,
//! then finishes the current job or hands it back.

use std::convert::Infallible;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::run::{CompletedRun, JobFinish, RunnerConfig, execute_job};
use crate::{Failure, Job, JobSource, Output, Receipt};

static PROCESS_DRAIN: AtomicBool = AtomicBool::new(false);

/// How drain treats a job already claimed from the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DrainMode {
    /// Execute the in-flight job and `complete` or `fail` as usual.
    FinishCurrent,
    /// Return the in-flight job with `JobSource::release` and do not write.
    HandBack,
}

/// Run-loop settings for [`serve_source`].
#[derive(Debug, Clone)]
pub struct ServeConfig {
    pub runner: RunnerConfig,
    /// Sleep when `next_job` is `None`, then poll drain again.
    pub idle: Duration,
    pub drain_mode: DrainMode,
    /// One-shot sources (a local job file) go not-ready when empty.
    /// Repeating sources stay ready while they might still claim.
    pub not_ready_when_empty: bool,
}

/// Health, readiness, and version for the probe listener.
#[derive(Debug)]
pub struct ProbeState {
    ready: AtomicBool,
    healthy: AtomicBool,
    version: String,
}

impl ProbeState {
    pub fn new(version: impl Into<String>) -> Self {
        Self {
            ready: AtomicBool::new(true),
            healthy: AtomicBool::new(true),
            version: version.into(),
        }
    }

    pub fn is_ready(&self) -> bool {
        self.ready.load(Ordering::SeqCst)
    }

    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::SeqCst)
    }

    pub fn version(&self) -> &str {
        &self.version
    }

    fn mark_unhealthy(&self) {
        self.healthy.store(false, Ordering::SeqCst);
    }

    /// Ready means this process will claim work. Stay false with no source.
    pub fn mark_not_ready(&self) {
        self.ready.store(false, Ordering::SeqCst);
    }
}

/// Signal that the serve loop should stop claiming work.
#[derive(Clone)]
pub struct DrainHandle {
    draining: Arc<AtomicBool>,
    probes: Arc<ProbeState>,
}

impl DrainHandle {
    pub fn new(probes: Arc<ProbeState>) -> Self {
        Self {
            draining: Arc::new(AtomicBool::new(false)),
            probes,
        }
    }

    /// Readiness goes false immediately; health stays true until the loop exits.
    pub fn request(&self) {
        self.probes.ready.store(false, Ordering::SeqCst);
        self.draining.store(true, Ordering::SeqCst);
    }

    pub fn is_draining(&self) -> bool {
        self.draining.load(Ordering::SeqCst)
    }
}

/// HTTP listener for `/health`, `/ready`, and `/version`.
pub struct ProbeServer {
    addr: SocketAddr,
    shutdown: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl ProbeServer {
    pub fn bind(addr: SocketAddr, state: Arc<ProbeState>) -> std::io::Result<Self> {
        let listener = TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        let addr = listener.local_addr()?;
        let shutdown = Arc::new(AtomicBool::new(false));
        let flag = shutdown.clone();
        let thread = thread::Builder::new()
            .name("kako-probes".into())
            .spawn(move || probe_loop(listener, state, flag))?;
        Ok(Self {
            addr,
            shutdown,
            thread: Some(thread),
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn shutdown(mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for ProbeServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

const MAX_PROBE_CONN: usize = 32;
const PROBE_DEADLINE: Duration = Duration::from_secs(2);
const PROBE_READ_SLICE: Duration = Duration::from_millis(100);

fn probe_loop(listener: TcpListener, state: Arc<ProbeState>, shutdown: Arc<AtomicBool>) {
    let inflight = Arc::new(AtomicUsize::new(0));
    while !shutdown.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((stream, _)) => {
                let prev = inflight.fetch_add(1, Ordering::SeqCst);
                if prev >= MAX_PROBE_CONN {
                    inflight.fetch_sub(1, Ordering::SeqCst);
                    drop(stream);
                    continue;
                }
                let inflight_for_handler = inflight.clone();
                let state = state.clone();
                let spawned = thread::Builder::new()
                    .name("kako-probe".into())
                    .spawn(move || {
                        handle_probe(stream, &state);
                        inflight_for_handler.fetch_sub(1, Ordering::SeqCst);
                    });
                if spawned.is_err() {
                    inflight.fetch_sub(1, Ordering::SeqCst);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(10));
            }
            Err(_) => break,
        }
    }
}

const MAX_PROBE_HEAD: usize = 8192;

fn handle_probe(mut stream: TcpStream, state: &ProbeState) {
    let _ = stream.set_nonblocking(false);
    let _ = stream.set_nodelay(true);
    let _ = stream.set_read_timeout(Some(PROBE_READ_SLICE));
    let Some(first) = read_probe_request_line(&mut stream, Instant::now() + PROBE_DEADLINE) else {
        return;
    };
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("").split('?').next().unwrap_or("");
    if method != "GET" {
        write_response(
            &mut stream,
            405,
            "Method Not Allowed",
            "method not allowed\n",
        );
        return;
    }
    match path {
        "/health" => {
            if state.is_healthy() {
                write_response(&mut stream, 200, "OK", "ok\n");
            } else {
                write_response(&mut stream, 503, "Service Unavailable", "unhealthy\n");
            }
        }
        "/ready" => {
            if state.is_ready() {
                write_response(&mut stream, 200, "OK", "ok\n");
            } else {
                write_response(&mut stream, 503, "Service Unavailable", "not ready\n");
            }
        }
        "/version" => write_response(&mut stream, 200, "OK", &format!("{}\n", state.version())),
        _ => write_response(&mut stream, 404, "Not Found", "not found\n"),
    }
}

fn read_probe_request_line(stream: &mut TcpStream, deadline: Instant) -> Option<String> {
    let mut buf = [0u8; MAX_PROBE_HEAD];
    let mut filled = 0usize;
    loop {
        if Instant::now() >= deadline {
            return None;
        }
        if filled == buf.len() {
            return None;
        }
        match stream.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => {
                filled += n;
                let view = &buf[..filled];
                if view.windows(4).any(|window| window == b"\r\n\r\n")
                    || view.windows(2).any(|window| window == b"\n\n")
                {
                    break;
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(_) => return None,
        }
    }
    let text = std::str::from_utf8(&buf[..filled]).ok()?;
    Some(text.lines().next()?.to_string())
}

fn write_response(stream: &mut TcpStream, status: u16, reason: &str, body: &str) {
    let _ = write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.flush();
}

/// Job source that never yields work. `kako serve` uses it when no local
/// job file is given, stays not-ready, and only serves probes and drain.
#[derive(Debug, Default)]
pub struct IdleSource;

impl JobSource for IdleSource {
    type Error = Infallible;

    fn next_job(&mut self) -> Result<Option<Job>, Self::Error> {
        Ok(None)
    }

    fn complete(&mut self, _: &Job, _: &[Output], _: &Receipt) -> Result<(), Self::Error> {
        Ok(())
    }

    fn fail(&mut self, _: &Job, _: &Failure) -> Result<(), Self::Error> {
        Ok(())
    }

    fn release(&mut self, _: &Job) -> Result<(), Self::Error> {
        Ok(())
    }
}

/// SIGTERM/SIGINT start drain. Not async-signal-safe work: the serve loop
/// polls [`poll_process_drain`].
#[cfg(unix)]
pub fn install_process_drain_signals() {
    extern "C" fn handle(_: libc::c_int) {
        PROCESS_DRAIN.store(true, Ordering::SeqCst);
    }
    unsafe {
        libc::signal(libc::SIGTERM, handle as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, handle as *const () as libc::sighandler_t);
    }
}

#[cfg(not(unix))]
pub fn install_process_drain_signals() {}

pub fn poll_process_drain(drain: &DrainHandle) {
    if PROCESS_DRAIN.swap(false, Ordering::SeqCst) {
        drain.request();
    }
}

enum Executed {
    Success(Box<CompletedRun>),
    Failed(Failure),
}

/// Pull jobs while ready, idle when the source is empty, then drain.
pub fn serve_source<S: JobSource>(
    source: &mut S,
    config: &ServeConfig,
    probes: &ProbeState,
    drain: &DrainHandle,
) -> Result<Vec<JobFinish>, S::Error> {
    serve_source_with(
        source,
        config.idle,
        config.drain_mode,
        config.not_ready_when_empty,
        probes,
        drain,
        |job| match execute_job(job, &config.runner) {
            Ok(completed) => Executed::Success(Box::new(completed)),
            Err(failure) => Executed::Failed(failure),
        },
    )
}

fn serve_source_with<S, E>(
    source: &mut S,
    idle: Duration,
    drain_mode: DrainMode,
    not_ready_when_empty: bool,
    probes: &ProbeState,
    drain: &DrainHandle,
    mut execute: E,
) -> Result<Vec<JobFinish>, S::Error>
where
    S: JobSource,
    E: FnMut(&Job) -> Executed,
{
    let mut finishes = Vec::new();
    loop {
        poll_process_drain(drain);
        if drain.is_draining() {
            probes.mark_unhealthy();
            break;
        }
        match source.next_job()? {
            None => {
                if not_ready_when_empty {
                    probes.mark_not_ready();
                }
                idle_wait(idle, drain)
            }
            Some(job) => {
                poll_process_drain(drain);
                if drain.is_draining() && drain_mode == DrainMode::HandBack {
                    source.release(&job)?;
                    finishes.push(JobFinish::Released {
                        job_id: job.id.clone(),
                    });
                    continue;
                }
                let executed = execute(&job);
                poll_process_drain(drain);
                if drain.is_draining() && drain_mode == DrainMode::HandBack {
                    source.release(&job)?;
                    finishes.push(JobFinish::Released {
                        job_id: job.id.clone(),
                    });
                    continue;
                }
                match executed {
                    Executed::Success(completed) => {
                        source.complete(&job, &completed.outputs, &completed.receipt)?;
                        finishes.push(JobFinish::Completed {
                            job_id: job.id.clone(),
                        });
                    }
                    Executed::Failed(failure) => {
                        source.fail(&job, &failure)?;
                        finishes.push(JobFinish::Failed {
                            job_id: job.id.clone(),
                            failure,
                        });
                    }
                }
            }
        }
    }
    Ok(finishes)
}

fn idle_wait(idle: Duration, drain: &DrainHandle) {
    let deadline = Instant::now() + idle;
    while Instant::now() < deadline {
        poll_process_drain(drain);
        if drain.is_draining() {
            return;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        thread::sleep(remaining.min(Duration::from_millis(10)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Engine, Input, IsolationTier, WriteMode};
    use std::collections::VecDeque;
    use std::io::Read;
    use std::net::{IpAddr, Ipv4Addr};
    use std::sync::mpsc;
    use std::time::SystemTime;

    #[derive(Debug)]
    struct SourceErr(String);
    impl std::fmt::Display for SourceErr {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str(&self.0)
        }
    }
    impl std::error::Error for SourceErr {}

    struct HandBackSource {
        queued: VecDeque<Job>,
        completes: Vec<String>,
        fails: Vec<String>,
        releases: Vec<String>,
    }

    impl HandBackSource {
        fn new(jobs: impl IntoIterator<Item = Job>) -> Self {
            Self {
                queued: jobs.into_iter().collect(),
                completes: Vec::new(),
                fails: Vec::new(),
                releases: Vec::new(),
            }
        }
    }

    impl JobSource for HandBackSource {
        type Error = SourceErr;
        fn next_job(&mut self) -> Result<Option<Job>, Self::Error> {
            Ok(self.queued.pop_front())
        }
        fn complete(&mut self, job: &Job, _: &[Output], _: &Receipt) -> Result<(), Self::Error> {
            self.completes.push(job.id.clone());
            Ok(())
        }
        fn fail(&mut self, job: &Job, _: &Failure) -> Result<(), Self::Error> {
            self.fails.push(job.id.clone());
            Ok(())
        }
        fn release(&mut self, job: &Job) -> Result<(), Self::Error> {
            self.releases.push(job.id.clone());
            self.queued.push_front(job.clone());
            Ok(())
        }
    }

    fn sample(id: &str) -> Job {
        Job {
            id: id.into(),
            engine: Engine::DuckDbSql,
            code: "SELECT 1".into(),
            inputs: vec![Input {
                name: "gone".into(),
                path: "/nonexistent/kako-pool-missing.csv".into(),
            }],
            outputs: vec!["out.csv".into()],
            write_mode: WriteMode::Overwrite,
            wall_clock: Duration::from_secs(5),
            cpu_time: None,
            memory_bytes: None,
            tenant_id: "local".into(),
            namespace: "default".into(),
            runtime_image: None,
            isolation: IsolationTier::LinuxNative,
        }
    }

    fn try_http_get(addr: SocketAddr, path: &str) -> Option<(u16, String)> {
        let mut stream = TcpStream::connect(addr).ok()?;
        let _ = stream.set_nodelay(true);
        stream
            .set_read_timeout(Some(Duration::from_millis(200)))
            .ok()?;
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: probe\r\nConnection: close\r\n\r\n"
        )
        .ok()?;
        stream.flush().ok()?;
        let mut buf = String::new();
        let _ = stream.read_to_string(&mut buf);
        let status = buf.split_whitespace().nth(1).and_then(|s| s.parse().ok())?;
        let body = buf.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
        Some((status, body))
    }

    fn http_get(addr: SocketAddr, path: &str) -> (u16, String) {
        let mut last = String::new();
        for _ in 0..50 {
            match try_http_get(addr, path) {
                Some(result) => return result,
                None => {
                    last = "probe request failed".into();
                    thread::sleep(Duration::from_millis(10));
                }
            }
        }
        panic!("probe GET {path} at {addr} failed: {last:?}");
    }

    #[test]
    fn probes_report_health_ready_and_version() {
        let probes = Arc::new(ProbeState::new("9.9.9-test"));
        let server = ProbeServer::bind(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            probes.clone(),
        )
        .unwrap();
        let addr = server.addr();
        assert_eq!(http_get(addr, "/health"), (200, "ok\n".into()));
        assert_eq!(http_get(addr, "/ready"), (200, "ok\n".into()));
        assert_eq!(http_get(addr, "/version"), (200, "9.9.9-test\n".into()));
        server.shutdown();
    }

    #[test]
    fn slow_probe_client_does_not_block_other_probes() {
        let probes = Arc::new(ProbeState::new("1"));
        let server = ProbeServer::bind(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            probes.clone(),
        )
        .unwrap();
        let addr = server.addr();
        let mut slow = TcpStream::connect(addr).unwrap();
        slow.set_nodelay(true).unwrap();
        write!(slow, "GET /health HTTP/1.1\r\nHost: probe\r\n").unwrap();
        slow.flush().unwrap();
        let started = Instant::now();
        assert_eq!(http_get(addr, "/health").0, 200);
        assert_eq!(http_get(addr, "/ready").0, 200);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "slow client blocked probes for {:?}",
            started.elapsed()
        );
        drop(slow);
        server.shutdown();
    }

    #[test]
    fn probe_deadline_frees_slots_after_slow_clients() {
        let probes = Arc::new(ProbeState::new("1"));
        let server = ProbeServer::bind(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            probes.clone(),
        )
        .unwrap();
        let addr = server.addr();
        let mut slow = Vec::new();
        for _ in 0..MAX_PROBE_CONN {
            let mut stream = TcpStream::connect(addr).unwrap();
            stream.set_nodelay(true).unwrap();
            write!(stream, "GET /health HTTP/1.1\r\n").unwrap();
            stream.flush().unwrap();
            slow.push(stream);
        }
        thread::sleep(Duration::from_millis(50));
        let deadline = Instant::now() + PROBE_DEADLINE + Duration::from_millis(500);
        let mut recovered = false;
        while Instant::now() < deadline {
            if try_http_get(addr, "/health").is_some_and(|result| result.0 == 200) {
                recovered = true;
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        assert!(
            recovered,
            "probe slots did not free after the request deadline"
        );
        drop(slow);
        server.shutdown();
    }

    #[test]
    fn drain_sets_ready_false_while_health_stays_true_during_inflight() {
        let probes = Arc::new(ProbeState::new("1"));
        let drain = DrainHandle::new(probes.clone());
        let server = ProbeServer::bind(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            probes.clone(),
        )
        .unwrap();
        let addr = server.addr();
        let (started_tx, started_rx) = mpsc::channel();
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let mut source = HandBackSource::new([sample("slow")]);
        let drain_for_thread = drain.clone();
        let probes_for_thread = probes.clone();
        let worker = thread::spawn(move || {
            serve_source_with(
                &mut source,
                Duration::from_millis(20),
                DrainMode::FinishCurrent,
                false,
                &probes_for_thread,
                &drain_for_thread,
                move |_| {
                    started_tx.send(()).unwrap();
                    go_rx.recv().unwrap();
                    Executed::Failed(Failure::Job("done".into()))
                },
            )
            .map(|finishes| (finishes, source))
        });
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(http_get(addr, "/ready").0, 200);
        assert_eq!(http_get(addr, "/health").0, 200);
        drain.request();
        assert_eq!(http_get(addr, "/ready").0, 503);
        assert_eq!(http_get(addr, "/health").0, 200);
        go_tx.send(()).unwrap();
        let (finishes, source) = worker.join().unwrap().unwrap();
        assert!(
            matches!(&finishes[..], [JobFinish::Failed { job_id, .. }] if job_id == "slow"),
            "{finishes:?}"
        );
        assert_eq!(source.fails, ["slow"]);
        assert!(source.releases.is_empty());
        assert_eq!(http_get(addr, "/health").0, 503);
        server.shutdown();
    }

    #[test]
    fn hand_back_under_load_neither_loses_nor_duplicates() {
        let probes = Arc::new(ProbeState::new("1"));
        let drain = DrainHandle::new(probes.clone());
        let (started_tx, started_rx) = mpsc::channel();
        let (go_tx, go_rx) = mpsc::channel::<()>();
        let mut source = HandBackSource::new([sample("one"), sample("two")]);
        let drain_for_thread = drain.clone();
        let probes_for_thread = probes.clone();
        let worker = thread::spawn(move || {
            serve_source_with(
                &mut source,
                Duration::from_millis(20),
                DrainMode::HandBack,
                false,
                &probes_for_thread,
                &drain_for_thread,
                move |_| {
                    started_tx.send(()).unwrap();
                    go_rx.recv().unwrap();
                    Executed::Failed(Failure::Job("should not report".into()))
                },
            )
            .map(|finishes| (finishes, source))
        });
        started_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        drain.request();
        go_tx.send(()).unwrap();
        let (finishes, mut source) = worker.join().unwrap().unwrap();
        assert!(
            matches!(&finishes[..], [JobFinish::Released { job_id }] if job_id == "one"),
            "{finishes:?}"
        );
        assert!(source.completes.is_empty(), "{:?}", source.completes);
        assert!(source.fails.is_empty(), "{:?}", source.fails);
        assert_eq!(source.releases, ["one"]);
        let first = source.next_job().unwrap().unwrap();
        let second = source.next_job().unwrap().unwrap();
        assert_eq!(first.id, "one");
        assert_eq!(second.id, "two");
        assert!(source.next_job().unwrap().is_none());
        source
            .complete(&first, &[], &empty_receipt(&first))
            .unwrap();
        source
            .complete(&second, &[], &empty_receipt(&second))
            .unwrap();
        assert_eq!(source.completes, ["one", "two"]);
    }

    #[test]
    fn one_shot_source_goes_not_ready_when_empty() {
        let probes = Arc::new(ProbeState::new("1"));
        let drain = DrainHandle::new(probes.clone());
        let server = ProbeServer::bind(
            SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            probes.clone(),
        )
        .unwrap();
        let addr = server.addr();
        let mut source = HandBackSource::new([sample("only")]);
        let drain_for_thread = drain.clone();
        let probes_for_thread = probes.clone();
        let worker = thread::spawn(move || {
            serve_source_with(
                &mut source,
                Duration::from_millis(20),
                DrainMode::FinishCurrent,
                true,
                &probes_for_thread,
                &drain_for_thread,
                |_| Executed::Failed(Failure::Job("done".into())),
            )
            .map(|finishes| (finishes, source))
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if http_get(addr, "/ready").0 == 503 && http_get(addr, "/health").0 == 200 {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(http_get(addr, "/ready").0, 503);
        assert_eq!(http_get(addr, "/health").0, 200);
        drain.request();
        let (finishes, source) = worker.join().unwrap().unwrap();
        assert!(
            matches!(&finishes[..], [JobFinish::Failed { job_id, .. }] if job_id == "only"),
            "{finishes:?}"
        );
        assert_eq!(source.fails, ["only"]);
        server.shutdown();
    }

    fn empty_receipt(job: &Job) -> Receipt {
        Receipt {
            job_id: job.id.clone(),
            inputs: Vec::new(),
            code: crate::Digest("sha256:code".into()),
            engine_version: "test".into(),
            outputs: Vec::new(),
            started_at: SystemTime::UNIX_EPOCH,
            duration: Duration::from_millis(1),
            tenant_id: job.tenant_id.clone(),
            namespace: job.namespace.clone(),
            cpu_usec: None,
            memory_peak_bytes: None,
            lineage: None,
            runtime_image: None,
            isolation: job.isolation,
            credential_scope: None,
        }
    }
}
