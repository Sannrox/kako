//! Job-output envelope encryption.
//!
//! The runner process unwraps the claim DEK and seals each output after the
//! worker exits. The DEK never enters the scratch tree, the worker manifest,
//! or the sandbox environment (ADR 0015).

use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use rand::RngCore;

use kako_core::{Failure, Job, JobKeyClaim};

pub use sekai_keys::{
    Dek, KeyError, KeyId, KeyProvider, KeyPurpose, MemoryKeyProvider, RuntimeProfile, TenantId,
    UnwrapGrant, WrappedDek,
};

const SEAL_MAGIC: &[u8; 8] = b"KAKOSEAL";
const ENGINE: &str = "kako";

/// Unwrap the claim DEK in the runner process.
pub fn unwrap_job_dek(
    provider: &Arc<dyn KeyProvider + Send + Sync>,
    job: &Job,
    claim: &JobKeyClaim,
) -> Result<Dek, Failure> {
    let wrapped: WrappedDek = serde_json::from_str(&claim.wrapped_dek)
        .map_err(|error| Failure::Runner(format!("parse wrapped dek: {error}")))?;
    if wrapped.dek_id.as_str() != claim.dek_id {
        return Err(Failure::Runner(
            "wrapped dek_id does not match the claim".into(),
        ));
    }
    if wrapped.kek_id.as_str() != claim.kek_id {
        return Err(Failure::Runner(
            "wrapped kek_id does not match the claim".into(),
        ));
    }
    let tenant =
        TenantId::new(&job.tenant_id).map_err(|error| Failure::Runner(error.to_string()))?;
    let grant = UnwrapGrant {
        tenant_id: tenant,
        purpose: KeyPurpose::Store,
        expires_unix_secs: grant_expiry(job),
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    match provider.unwrap(&wrapped, &grant, ENGINE, now) {
        Ok(out) => {
            // Receipts record key ids. kako has no unwrap-audit sink in this slice.
            let _audit = out.audit;
            Ok(out.dek)
        }
        Err(KeyError::Revoked) => Err(Failure::KeyRevoked {
            dek_id: claim.dek_id.clone(),
            kek_id: claim.kek_id.clone(),
        }),
        Err(error) => Err(Failure::Runner(format!("unwrap dek: {error}"))),
    }
}

fn grant_expiry(job: &Job) -> i64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    now.saturating_add(job.wall_clock.as_secs() as i64)
        .saturating_add(60)
}

/// Fail closed when `path` is missing, a directory, or a symlink.
pub(crate) fn require_regular_output(path: &Path, name: &str) -> Result<(), Failure> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(_) => {
            return Err(Failure::Job(format!(
                "expected output `{name}` was not written"
            )));
        }
    };
    if meta.file_type().is_symlink() {
        return Err(Failure::Job(format!(
            "expected output `{name}` must not be a symlink"
        )));
    }
    if !meta.is_file() {
        return Err(Failure::Job(format!(
            "expected output `{name}` was not written"
        )));
    }
    Ok(())
}

/// Replace `path` with an authenticated ciphertext envelope.
///
/// AES-256-GCM here is one-shot. An output-size quota on the runner is
/// follow-up; hashing already streams, and a chunked envelope would change
/// `KAKOSEAL`.
pub fn seal_output(
    path: &Path,
    dek: &Dek,
    claim: &JobKeyClaim,
    job_id: &str,
) -> Result<(), Failure> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("output");
    require_regular_output(path, name)?;
    let plain = fs::read(path).map_err(|error| Failure::Runner(format!("read output: {error}")))?;
    let sealed = seal_bytes(&plain, dek, claim, job_id)?;
    fs::write(path, sealed)
        .map_err(|error| Failure::Runner(format!("write sealed output: {error}")))
}

fn seal_bytes(
    plain: &[u8],
    dek: &Dek,
    claim: &JobKeyClaim,
    job_id: &str,
) -> Result<Vec<u8>, Failure> {
    let cipher = Aes256Gcm::new_from_slice(dek.as_bytes())
        .map_err(|error| Failure::Runner(format!("aes key: {error}")))?;
    let mut nonce = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce);
    let aad = aad(claim, job_id);
    let ct = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plain,
                aad: &aad,
            },
        )
        .map_err(|error| Failure::Runner(format!("seal output: {error}")))?;
    let mut out = Vec::with_capacity(SEAL_MAGIC.len() + nonce.len() + ct.len());
    out.extend_from_slice(SEAL_MAGIC);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

fn aad(claim: &JobKeyClaim, job_id: &str) -> Vec<u8> {
    let mut aad = Vec::from(*SEAL_MAGIC);
    aad.extend_from_slice(job_id.as_bytes());
    aad.extend_from_slice(claim.dek_id.as_bytes());
    aad
}

/// True when `bytes` contain the UTF-8 needle. Used by the disk-scan test.
pub fn contains_plaintext(bytes: &[u8], needle: &str) -> bool {
    memchr_slice(bytes, needle.as_bytes())
}

fn memchr_slice(haystack: &[u8], needle: &[u8]) -> bool {
    haystack.windows(needle.len()).any(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claim() -> JobKeyClaim {
        JobKeyClaim {
            wrapped_dek: "{}".into(),
            dek_id: "dek-1".into(),
            kek_id: "kek-1".into(),
        }
    }

    #[test]
    fn seal_hides_plaintext() {
        let dek = Dek::generate();
        let secret = b"tenant-secret-value";
        let sealed = seal_bytes(secret, &dek, &claim(), "job-1").unwrap();
        assert!(sealed.starts_with(SEAL_MAGIC));
        assert!(!contains_plaintext(&sealed, "tenant-secret-value"));
    }

    #[test]
    fn seal_output_replaces_file_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("out-{}.csv", std::process::id()));
        let needle = format!("secret-{}", std::process::id());
        fs::write(&path, format!("note\n{needle}\n")).unwrap();
        let dek = Dek::generate();
        seal_output(&path, &dek, &claim(), "job-1").unwrap();
        let sealed = fs::read(&path).unwrap();
        assert!(sealed.starts_with(SEAL_MAGIC));
        assert!(!contains_plaintext(&sealed, &needle));
    }

    #[cfg(unix)]
    #[test]
    fn seal_output_refuses_symlink_and_leaves_target() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join(format!("host-{}.txt", std::process::id()));
        let needle = format!("host-secret-{}", std::process::id());
        fs::write(&target, format!("{needle}\n")).unwrap();
        let path = dir.path().join("out.csv");
        std::os::unix::fs::symlink(&target, &path).unwrap();
        let dek = Dek::generate();
        let err = seal_output(&path, &dek, &claim(), "job-1").unwrap_err();
        assert!(
            matches!(err, Failure::Job(ref msg) if msg.contains("symlink")),
            "{err}"
        );
        assert_eq!(fs::read_to_string(&target).unwrap(), format!("{needle}\n"));
    }

    fn sample_job(claim: JobKeyClaim) -> Job {
        use std::time::Duration;
        Job {
            id: "job-1".into(),
            engine: kako_core::Engine::DuckDbSql,
            code: "SELECT 1".into(),
            inputs: vec![],
            outputs: vec!["out.csv".into()],
            wall_clock: Duration::from_secs(5),
            cpu_time: None,
            memory_bytes: None,
            tenant_id: "acme".into(),
            namespace: "default".into(),
            runtime_image: None,
            write_mode: kako_core::WriteMode::Overwrite,
            isolation: kako_core::IsolationTier::LinuxNative,
            output_markings: Default::default(),
            declassify: None,
            encryption: Some(claim),
        }
    }

    #[test]
    fn unwrap_rejects_kek_id_mismatch() {
        let provider: std::sync::Arc<dyn KeyProvider + Send + Sync> =
            std::sync::Arc::new(MemoryKeyProvider::new(RuntimeProfile::Development).unwrap());
        let tenant = TenantId::new("acme").unwrap();
        let dek = Dek::generate();
        let wrapped = provider.wrap(&tenant, KeyPurpose::Store, &dek).unwrap();
        let claim = JobKeyClaim {
            wrapped_dek: serde_json::to_string(&wrapped).unwrap(),
            dek_id: wrapped.dek_id.as_str().to_string(),
            kek_id: "other-kek".into(),
        };
        let err = unwrap_job_dek(&provider, &sample_job(claim.clone()), &claim).unwrap_err();
        assert!(
            matches!(err, Failure::Runner(ref msg) if msg.contains("kek_id")),
            "{err}"
        );
    }
}
