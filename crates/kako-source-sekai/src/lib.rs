//! Sekai isolated-process [`JobSource`] adapter.
//!
//! Fence, heartbeat, and write-back stay in this crate. kako-core does not
//! depend on Sekai. Default tests use a fake claim transport; the published
//! runtime-claim client is an optional feature.

use std::path::{Path, PathBuf};
use std::time::Duration;

use kako_core::{
    Engine, Failure, Input, IsolationTier, Job, JobSource, Output, Receipt, WriteMode,
};

/// Claim fence the adapter must present on heartbeat and write-back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fence {
    pub run_id: String,
    pub runtime_id: String,
    pub generation: i64,
    pub fencing_token: String,
}

/// Work delivered by a successful claim, with inputs already on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimedWork {
    pub fence: Fence,
    pub job: Job,
}

/// Errors the Sekai adapter reports to the run loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SekaiSourceError {
    FenceLost,
    Heartbeat(String),
    Transport(String),
}

impl std::fmt::Display for SekaiSourceError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FenceLost => formatter.write_str("claim fence was lost"),
            Self::Heartbeat(message) => write!(formatter, "heartbeat: {message}"),
            Self::Transport(message) => write!(formatter, "sekai source: {message}"),
        }
    }
}

impl std::error::Error for SekaiSourceError {}

/// Claim, heartbeat, and fenced write-back. Fake tests implement this;
/// the optional `sekai-client` feature wraps the published client.
pub trait ClaimTransport {
    fn claim_next(&mut self, runtime_id: &str) -> Result<Option<ClaimedWork>, SekaiSourceError>;
    fn heartbeat(&mut self, fence: &Fence) -> Result<(), SekaiSourceError>;
    fn write_outputs(
        &mut self,
        fence: &Fence,
        outputs: &[Output],
        receipt: &Receipt,
    ) -> Result<(), SekaiSourceError>;
    fn report_failure(&mut self, fence: &Fence, failure: &Failure) -> Result<(), SekaiSourceError>;

    /// Return the held claim to the source without write-back.
    ///
    /// Default fails closed. Adapters that can requeue must override and
    /// must not call `write_outputs` or `report_failure`.
    fn release(&mut self, fence: &Fence) -> Result<(), SekaiSourceError> {
        let _ = fence;
        Err(SekaiSourceError::Transport(
            "claim transport does not support hand-back".into(),
        ))
    }
}

/// Job source that claims isolated-process work and writes back under the fence.
pub struct SekaiSource<T> {
    transport: T,
    runtime_id: String,
    held: Option<Fence>,
}

impl<T: ClaimTransport> SekaiSource<T> {
    pub fn new(transport: T, runtime_id: impl Into<String>) -> Self {
        Self {
            transport,
            runtime_id: runtime_id.into(),
            held: None,
        }
    }

    pub fn into_transport(self) -> T {
        self.transport
    }

    fn require_held<'a>(&'a self, job: &Job) -> Result<&'a Fence, SekaiSourceError> {
        let fence = self
            .held
            .as_ref()
            .ok_or_else(|| SekaiSourceError::Transport("no held claim".into()))?;
        if fence.run_id != job.id {
            return Err(SekaiSourceError::Transport(
                "job does not match the held claim".into(),
            ));
        }
        Ok(fence)
    }
}

impl<T: ClaimTransport> JobSource for SekaiSource<T> {
    type Error = SekaiSourceError;

    // Heartbeat is required at complete/fail so fence loss cannot become a
    // successful write-back. JobSource has no execute-time hook (unchanged
    // for #8); mid-run lease renewal belongs with the live plane.

    fn next_job(&mut self) -> Result<Option<Job>, Self::Error> {
        if self.held.is_some() {
            return Err(SekaiSourceError::Transport(
                "previous claim is still held".into(),
            ));
        }
        let Some(claimed) = self.transport.claim_next(&self.runtime_id)? else {
            return Ok(None);
        };
        self.held = Some(claimed.fence);
        Ok(Some(claimed.job))
    }

    fn complete(
        &mut self,
        job: &Job,
        outputs: &[Output],
        receipt: &Receipt,
    ) -> Result<(), Self::Error> {
        let fence = self.require_held(job)?.clone();
        if let Err(error) = self.transport.heartbeat(&fence) {
            self.held = None;
            return Err(error);
        }
        let result = self.transport.write_outputs(&fence, outputs, receipt);
        if result.is_ok() || matches!(result, Err(SekaiSourceError::FenceLost)) {
            self.held = None;
        }
        result
    }

    fn fail(&mut self, job: &Job, failure: &Failure) -> Result<(), Self::Error> {
        let fence = self.require_held(job)?.clone();
        if let Err(error) = self.transport.heartbeat(&fence) {
            self.held = None;
            return Err(error);
        }
        let result = self.transport.report_failure(&fence, failure);
        if result.is_ok() || matches!(result, Err(SekaiSourceError::FenceLost)) {
            self.held = None;
        }
        result
    }

    fn release(&mut self, job: &Job) -> Result<(), Self::Error> {
        let fence = self.require_held(job)?.clone();
        let result = self.transport.release(&fence);
        if result.is_ok() || matches!(result, Err(SekaiSourceError::FenceLost)) {
            self.held = None;
        }
        result
    }
}

/// In-memory claim transport for adapter tests.
#[derive(Debug, Default)]
pub struct FakeClaimTransport {
    queued: Option<ClaimedWork>,
    held_work: Option<ClaimedWork>,
    fence_lost: bool,
    fence_lost_on_write: bool,
    heartbeat_error: Option<String>,
    report_error: Option<String>,
    writes: Vec<PathBuf>,
    failures: Vec<String>,
}

impl FakeClaimTransport {
    pub fn with_job(job: Job, fence: Fence) -> Self {
        Self {
            queued: Some(ClaimedWork { fence, job }),
            ..Self::default()
        }
    }

    pub fn lose_fence(&mut self) {
        self.fence_lost = true;
    }

    pub fn lose_fence_on_write(&mut self) {
        self.fence_lost_on_write = true;
    }

    pub fn fail_heartbeat(&mut self, message: impl Into<String>) {
        self.heartbeat_error = Some(message.into());
    }

    pub fn fail_report(&mut self, message: impl Into<String>) {
        self.report_error = Some(message.into());
    }

    pub fn writes(&self) -> &[PathBuf] {
        &self.writes
    }

    pub fn failures(&self) -> &[String] {
        &self.failures
    }
}

impl ClaimTransport for FakeClaimTransport {
    fn claim_next(&mut self, runtime_id: &str) -> Result<Option<ClaimedWork>, SekaiSourceError> {
        let Some(claimed) = self.queued.take() else {
            return Ok(None);
        };
        if claimed.fence.runtime_id != runtime_id {
            return Err(SekaiSourceError::Transport(
                "runtime_id does not match the claimed fence".into(),
            ));
        }
        self.held_work = Some(claimed.clone());
        Ok(Some(claimed))
    }

    fn heartbeat(&mut self, _fence: &Fence) -> Result<(), SekaiSourceError> {
        if let Some(message) = &self.heartbeat_error {
            return Err(SekaiSourceError::Heartbeat(message.clone()));
        }
        if self.fence_lost {
            return Err(SekaiSourceError::FenceLost);
        }
        Ok(())
    }

    fn write_outputs(
        &mut self,
        _fence: &Fence,
        outputs: &[Output],
        _receipt: &Receipt,
    ) -> Result<(), SekaiSourceError> {
        if self.fence_lost || self.fence_lost_on_write {
            return Err(SekaiSourceError::FenceLost);
        }
        self.writes
            .extend(outputs.iter().map(|output| output.path.clone()));
        self.held_work = None;
        Ok(())
    }

    fn report_failure(
        &mut self,
        _fence: &Fence,
        failure: &Failure,
    ) -> Result<(), SekaiSourceError> {
        if self.fence_lost {
            return Err(SekaiSourceError::FenceLost);
        }
        if let Some(message) = &self.report_error {
            return Err(SekaiSourceError::Transport(message.clone()));
        }
        self.failures.push(failure.to_string());
        self.held_work = None;
        Ok(())
    }

    fn release(&mut self, fence: &Fence) -> Result<(), SekaiSourceError> {
        if self.fence_lost {
            self.held_work = None;
            return Err(SekaiSourceError::FenceLost);
        }
        let work = self
            .held_work
            .as_ref()
            .ok_or_else(|| SekaiSourceError::Transport("no held claim".into()))?;
        if work.fence != *fence {
            return Err(SekaiSourceError::Transport(
                "release fence does not match the held claim".into(),
            ));
        }
        if self.queued.is_some() {
            return Err(SekaiSourceError::Transport(
                "cannot hand back while another claim is queued".into(),
            ));
        }
        self.queued = self.held_work.take();
        Ok(())
    }
}

/// Helper for tests: a python isolated-process shaped job with one staged input.
pub fn sample_claimed_job(scratch: &Path, runtime_id: &str) -> (Job, Fence) {
    let input = scratch.join("in.csv");
    std::fs::write(&input, "id\n1\n").expect("stage input");
    let job = Job {
        id: "run-1".into(),
        engine: Engine::Python,
        code: "print('ok')".into(),
        inputs: vec![Input {
            name: "in".into(),
            path: input,
        }],
        outputs: vec!["out".into()],
        write_mode: WriteMode::Overwrite,
        wall_clock: Duration::from_secs(30),
        cpu_time: None,
        memory_bytes: None,
        tenant_id: "ops".into(),
        namespace: "ops".into(),
        runtime_image: Some("runner:1".into()),
        isolation: IsolationTier::LinuxNative,
    };
    let fence = Fence {
        run_id: "run-1".into(),
        runtime_id: runtime_id.into(),
        generation: 1,
        fencing_token: "fence-1".into(),
    };
    (job, fence)
}

#[cfg(feature = "sekai-client")]
pub mod live;

#[cfg(test)]
mod tests {
    use super::*;
    use kako_core::Digest;
    use std::time::SystemTime;

    fn receipt(job: &Job) -> Receipt {
        Receipt {
            job_id: job.id.clone(),
            inputs: Vec::new(),
            code: Digest("sha256:code".into()),
            engine_version: "test".into(),
            outputs: Vec::new(),
            started_at: SystemTime::UNIX_EPOCH,
            duration: Duration::from_millis(1),
            tenant_id: job.tenant_id.clone(),
            namespace: job.namespace.clone(),
            cpu_usec: None,
            memory_peak_bytes: None,
            lineage: None,
            runtime_image: job.runtime_image.clone(),
            isolation: job.isolation,
        }
    }

    #[test]
    fn lost_fence_aborts_write_back_and_leaves_no_output() {
        let scratch = tempfile::tempdir().unwrap();
        let (job, fence) = sample_claimed_job(scratch.path(), "runner-a");
        let mut source =
            SekaiSource::new(FakeClaimTransport::with_job(job.clone(), fence), "runner-a");
        let claimed = source.next_job().unwrap().expect("claimed");
        source.transport.lose_fence();
        let error = source
            .complete(
                &claimed,
                &[Output {
                    name: "out".into(),
                    path: scratch.path().join("out.csv"),
                    digest: Digest("sha256:out".into()),
                }],
                &receipt(&claimed),
            )
            .unwrap_err();
        assert_eq!(error, SekaiSourceError::FenceLost);
        assert!(source.into_transport().writes().is_empty());
    }

    #[test]
    fn heartbeat_failure_aborts_complete_without_write_back() {
        let scratch = tempfile::tempdir().unwrap();
        let (job, fence) = sample_claimed_job(scratch.path(), "runner-a");
        let mut transport = FakeClaimTransport::with_job(job, fence);
        transport.fail_heartbeat("lease expired");
        let mut source = SekaiSource::new(transport, "runner-a");
        let claimed = source.next_job().unwrap().expect("claimed");
        let error = source
            .complete(
                &claimed,
                &[Output {
                    name: "out".into(),
                    path: scratch.path().join("out.csv"),
                    digest: Digest("sha256:out".into()),
                }],
                &receipt(&claimed),
            )
            .unwrap_err();
        assert!(
            matches!(error, SekaiSourceError::Heartbeat(message) if message == "lease expired")
        );
        assert!(source.into_transport().writes().is_empty());
    }

    #[test]
    fn successful_complete_records_write_under_the_fence() {
        let scratch = tempfile::tempdir().unwrap();
        let (job, fence) = sample_claimed_job(scratch.path(), "runner-a");
        let mut source =
            SekaiSource::new(FakeClaimTransport::with_job(job.clone(), fence), "runner-a");
        let claimed = source.next_job().unwrap().expect("claimed");
        let out = scratch.path().join("out.csv");
        std::fs::write(&out, "id\n1\n").unwrap();
        source
            .complete(
                &claimed,
                &[Output {
                    name: "out".into(),
                    path: out,
                    digest: Digest("sha256:out".into()),
                }],
                &receipt(&claimed),
            )
            .unwrap();
        assert_eq!(source.into_transport().writes().len(), 1);
    }

    #[test]
    fn write_time_fence_loss_after_heartbeat_leaves_no_output() {
        let scratch = tempfile::tempdir().unwrap();
        let (job, fence) = sample_claimed_job(scratch.path(), "runner-a");
        let mut transport = FakeClaimTransport::with_job(job, fence);
        transport.lose_fence_on_write();
        let mut source = SekaiSource::new(transport, "runner-a");
        let claimed = source.next_job().unwrap().expect("claimed");
        let error = source
            .complete(
                &claimed,
                &[Output {
                    name: "out".into(),
                    path: scratch.path().join("out.csv"),
                    digest: Digest("sha256:out".into()),
                }],
                &receipt(&claimed),
            )
            .unwrap_err();
        assert_eq!(error, SekaiSourceError::FenceLost);
        assert!(source.into_transport().writes().is_empty());
    }

    #[test]
    fn heartbeat_failure_on_fail_does_not_report() {
        let scratch = tempfile::tempdir().unwrap();
        let (job, fence) = sample_claimed_job(scratch.path(), "runner-a");
        let mut transport = FakeClaimTransport::with_job(job, fence);
        transport.fail_heartbeat("lease expired");
        let mut source = SekaiSource::new(transport, "runner-a");
        let claimed = source.next_job().unwrap().expect("claimed");
        let error = source
            .fail(&claimed, &Failure::Job("boom".into()))
            .unwrap_err();
        assert!(
            matches!(error, SekaiSourceError::Heartbeat(message) if message == "lease expired")
        );
        assert!(source.into_transport().failures().is_empty());
    }

    #[test]
    fn transport_error_on_fail_keeps_the_held_claim() {
        let scratch = tempfile::tempdir().unwrap();
        let (job, fence) = sample_claimed_job(scratch.path(), "runner-a");
        let mut transport = FakeClaimTransport::with_job(job, fence);
        transport.fail_report("unavailable");
        let mut source = SekaiSource::new(transport, "runner-a");
        let claimed = source.next_job().unwrap().expect("claimed");
        let error = source
            .fail(&claimed, &Failure::Job("boom".into()))
            .unwrap_err();
        assert!(matches!(error, SekaiSourceError::Transport(message) if message == "unavailable"));
        let error = source
            .fail(&claimed, &Failure::Job("boom".into()))
            .unwrap_err();
        assert!(matches!(error, SekaiSourceError::Transport(message) if message == "unavailable"));
    }

    #[test]
    fn release_requeues_without_write_back() {
        let scratch = tempfile::tempdir().unwrap();
        let (job, fence) = sample_claimed_job(scratch.path(), "runner-a");
        let mut source =
            SekaiSource::new(FakeClaimTransport::with_job(job.clone(), fence), "runner-a");
        let claimed = source.next_job().unwrap().expect("claimed");
        source.release(&claimed).unwrap();
        let again = source.next_job().unwrap().expect("requeued");
        assert_eq!(again.id, claimed.id);
        source
            .complete(
                &again,
                &[Output {
                    name: "out".into(),
                    path: scratch.path().join("out.csv"),
                    digest: Digest("sha256:out".into()),
                }],
                &receipt(&again),
            )
            .unwrap();
        let transport = source.into_transport();
        assert_eq!(transport.writes().len(), 1);
        assert!(transport.failures().is_empty());
    }

    #[test]
    fn complete_after_release_does_not_write() {
        let scratch = tempfile::tempdir().unwrap();
        let (job, fence) = sample_claimed_job(scratch.path(), "runner-a");
        let mut source =
            SekaiSource::new(FakeClaimTransport::with_job(job.clone(), fence), "runner-a");
        let claimed = source.next_job().unwrap().expect("claimed");
        source.release(&claimed).unwrap();
        let error = source
            .complete(
                &claimed,
                &[Output {
                    name: "out".into(),
                    path: scratch.path().join("out.csv"),
                    digest: Digest("sha256:out".into()),
                }],
                &receipt(&claimed),
            )
            .unwrap_err();
        assert!(
            matches!(error, SekaiSourceError::Transport(message) if message.contains("no held claim"))
        );
        assert!(source.into_transport().writes().is_empty());
    }

    #[test]
    fn failed_hand_back_keeps_the_held_claim() {
        let scratch = tempfile::tempdir().unwrap();
        let (job, fence) = sample_claimed_job(scratch.path(), "runner-a");
        let mut source =
            SekaiSource::new(FakeClaimTransport::with_job(job.clone(), fence), "runner-a");
        let claimed = source.next_job().unwrap().expect("claimed");
        let mut extra = claimed.clone();
        extra.id = "run-2".into();
        source.transport.queued = Some(ClaimedWork {
            fence: Fence {
                run_id: "run-2".into(),
                runtime_id: "runner-a".into(),
                generation: 1,
                fencing_token: "fence-2".into(),
            },
            job: extra,
        });
        let error = source.release(&claimed).unwrap_err();
        assert!(
            matches!(error, SekaiSourceError::Transport(ref message) if message.contains("queued")),
            "{error}"
        );
        source.transport.queued = None;
        source.release(&claimed).unwrap();
        let again = source.next_job().unwrap().expect("requeued");
        assert_eq!(again.id, claimed.id);
        assert!(source.into_transport().writes().is_empty());
    }

    #[test]
    fn lost_fence_on_release_does_not_requeue_or_write() {
        let scratch = tempfile::tempdir().unwrap();
        let (job, fence) = sample_claimed_job(scratch.path(), "runner-a");
        let mut source = SekaiSource::new(FakeClaimTransport::with_job(job, fence), "runner-a");
        let claimed = source.next_job().unwrap().expect("claimed");
        source.transport.lose_fence();
        let error = source.release(&claimed).unwrap_err();
        assert_eq!(error, SekaiSourceError::FenceLost);
        assert!(source.next_job().unwrap().is_none());
        assert!(source.into_transport().writes().is_empty());
    }
}
