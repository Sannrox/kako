//! Job-scoped credentials ([ADR 0013](../../../docs/decisions/0013-job-scoped-credentials.md)).
//!
//! A job source that reads or writes remote data mints one short-lived
//! credential per claimed job, scoped to that job's inputs and outputs. The
//! runner process holds it; nothing puts it in the job manifest, the scratch
//! tree, or the sandbox environment. Receipts record the scope id only.

use std::fmt;
use std::time::{Duration, SystemTime};

use crate::Job;

/// Extra lifetime past the job's wall clock for staging and write-back.
pub const WRITE_BACK_GRACE: Duration = Duration::from_secs(60);

/// What one credential may touch: read the job's inputs, write its outputs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialScope {
    /// Issuer-assigned id. This, not the secret, goes on the receipt.
    pub id: String,
    pub job_id: String,
    /// Input names the credential may read.
    pub read: Vec<String>,
    /// Output names the credential may write.
    pub write: Vec<String>,
}

impl CredentialScope {
    /// Scope covering exactly `job`'s inputs (read) and outputs (write).
    pub fn for_job(id: impl Into<String>, job: &Job) -> Self {
        Self {
            id: id.into(),
            job_id: job.id.clone(),
            read: job.inputs.iter().map(|input| input.name.clone()).collect(),
            write: job.outputs.clone(),
        }
    }
}

/// Credential material. `Debug` and `Display` never print it.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The raw value, for the transport call that presents it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Secret([redacted])")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[redacted]")
    }
}

/// A minted credential. The issuer enforces `expires_at`; the source also
/// revokes it as soon as the claim ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobCredential {
    pub scope: CredentialScope,
    pub secret: Secret,
    pub expires_at: SystemTime,
}

/// Lifetime to request for `job`: its wall clock plus [`WRITE_BACK_GRACE`].
pub fn ttl_for(job: &Job) -> Duration {
    job.wall_clock.saturating_add(WRITE_BACK_GRACE)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Engine, Input, IsolationTier, WriteMode};
    use std::path::PathBuf;

    fn job() -> Job {
        Job {
            id: "run-1".into(),
            engine: Engine::DuckDbSql,
            code: "SELECT 1".into(),
            inputs: vec![Input {
                name: "orders".into(),
                path: PathBuf::from("orders.csv"),
            }],
            outputs: vec!["totals.csv".into()],
            write_mode: WriteMode::Overwrite,
            wall_clock: Duration::from_secs(30),
            cpu_time: None,
            memory_bytes: None,
            tenant_id: "t".into(),
            namespace: "ns".into(),
            runtime_image: None,
            isolation: IsolationTier::LinuxNative,
        }
    }

    #[test]
    fn scope_covers_only_the_jobs_inputs_and_outputs() {
        let scope = CredentialScope::for_job("cred-1", &job());
        assert_eq!(scope.job_id, "run-1");
        assert_eq!(scope.read, ["orders"]);
        assert_eq!(scope.write, ["totals.csv"]);
    }

    #[test]
    fn secret_is_redacted_in_debug_and_display() {
        let credential = JobCredential {
            scope: CredentialScope::for_job("cred-1", &job()),
            secret: Secret::new("s3cr3t-value"),
            expires_at: SystemTime::UNIX_EPOCH,
        };
        let printed = format!("{credential:?} {}", credential.secret);
        assert!(!printed.contains("s3cr3t-value"), "{printed}");
        assert!(printed.contains("cred-1"), "{printed}");
    }
}
