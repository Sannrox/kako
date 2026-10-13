//! CLI serve: probes, local job claim, and SIGTERM drain.

#![cfg(unix)]

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn worker() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_BIN_EXE_kako"))
}

fn http_get(addr: SocketAddr, path: &str) -> (u16, String) {
    let mut last = String::new();
    for _ in 0..50 {
        match TcpStream::connect(addr) {
            Ok(mut stream) => {
                let _ = stream.set_nodelay(true);
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                if write!(
                    stream,
                    "GET {path} HTTP/1.1\r\nHost: probe\r\nConnection: close\r\n\r\n"
                )
                .is_err()
                    || stream.flush().is_err()
                {
                    last = "write failed".into();
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                let mut buf = String::new();
                let _ = stream.read_to_string(&mut buf);
                if let Some(status) = buf.split_whitespace().nth(1).and_then(|s| s.parse().ok()) {
                    let body = buf.split("\r\n\r\n").nth(1).unwrap_or("").to_string();
                    return (status, body);
                }
                last = buf;
            }
            Err(error) => last = error.to_string(),
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("probe GET {path} at {addr} failed: {last:?}");
}

#[test]
fn serve_exposes_probes_and_exits_on_sigterm() {
    let mut child = Command::new(worker())
        .args(["serve", "--listen", "127.0.0.1:0", "--idle-ms", "20"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().expect("stdout");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .expect("serve prints the listen address");
    let addr: SocketAddr = line
        .strip_prefix("listening on ")
        .unwrap_or(&line)
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("listen line: {line:?}"));
    assert_eq!(http_get(addr, "/health").0, 200);
    assert_eq!(http_get(addr, "/ready").0, 503);
    let (status, body) = http_get(addr, "/version");
    assert_eq!(status, 200);
    assert_eq!(body, format!("{}\n", env!("CARGO_PKG_VERSION")));
    let term = Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status()
        .unwrap();
    assert!(term.success(), "kill -TERM the serve process");
    let finished = child.wait_with_output().unwrap();
    assert!(
        finished.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&finished.stderr)
    );
}

#[test]
fn serve_claims_a_local_job_then_idles() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("in.csv"), "a\n1\n").unwrap();
    let job = dir.path().join("job.json");
    fs::write(
        &job,
        r#"{
          "sql": "COPY (SELECT * FROM in) TO 'outputs/out.csv' (HEADER);",
          "inputs": {"in": "in.csv"},
          "outputs": ["out.csv"]
        }"#,
    )
    .unwrap();
    let out = dir.path().join("out");
    let mut child = Command::new(worker())
        .args([
            "serve",
            "--listen",
            "127.0.0.1:0",
            "--idle-ms",
            "20",
            "--output",
            out.to_str().unwrap(),
            job.to_str().unwrap(),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().expect("stdout");
    let mut reader = BufReader::new(stdout);
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .expect("serve prints the listen address");
    let addr: SocketAddr = line
        .strip_prefix("listening on ")
        .unwrap_or(&line)
        .trim()
        .parse()
        .unwrap_or_else(|_| panic!("listen line: {line:?}"));
    assert_eq!(http_get(addr, "/health").0, 200);
    let deadline = Instant::now() + Duration::from_secs(30);
    while Instant::now() < deadline {
        if out.join("receipt.json").exists() || out.join("failure.txt").exists() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        out.join("receipt.json").exists() || out.join("failure.txt").exists(),
        "serve did not claim the local job; stderr pending until exit"
    );
    assert_eq!(http_get(addr, "/ready").0, 503);
    assert_eq!(http_get(addr, "/health").0, 200);
    let term = Command::new("kill")
        .arg("-TERM")
        .arg(child.id().to_string())
        .status()
        .unwrap();
    assert!(term.success(), "kill -TERM the serve process");
    let finished = child.wait_with_output().unwrap();
    assert!(
        finished.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&finished.stderr)
    );
}
