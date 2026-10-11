//! Round-robin job source by [`QuotaKey`].
//!
//! The job source owns fair share (ADR 0004). This adapter is a reference
//! implementation other sources can reuse. It is not installed in front of
//! every source. The local CLI stays one-shot.

use std::collections::{BTreeMap, VecDeque};
use std::convert::Infallible;

use crate::{Failure, Job, JobSource, Output, QuotaKey, Receipt};

/// Jobs grouped by owner and yielded round-robin so one flood cannot occupy
/// every consecutive slot.
#[derive(Debug, Default)]
pub struct FairSource {
    by_owner: BTreeMap<QuotaKey, VecDeque<Job>>,
    order: Vec<QuotaKey>,
    cursor: usize,
}

impl FairSource {
    /// Queue `jobs` in first-seen owner order. Fails closed on a missing
    /// tenant or namespace.
    pub fn new(jobs: impl IntoIterator<Item = Job>) -> Result<Self, String> {
        let mut source = Self::default();
        for job in jobs {
            source.push(job)?;
        }
        Ok(source)
    }

    /// Append one job onto its owner's queue.
    pub fn push(&mut self, job: Job) -> Result<(), String> {
        let key = QuotaKey::from_job(&job)?;
        if !self.by_owner.contains_key(&key) {
            self.order.push(key.clone());
        }
        self.by_owner.entry(key).or_default().push_back(job);
        Ok(())
    }
}

impl JobSource for FairSource {
    type Error = Infallible;

    fn next_job(&mut self) -> Result<Option<Job>, Self::Error> {
        if self.order.is_empty() {
            return Ok(None);
        }
        for _ in 0..self.order.len() {
            let i = self.cursor % self.order.len();
            self.cursor += 1;
            let key = &self.order[i];
            if let Some(job) = self.by_owner.get_mut(key).and_then(VecDeque::pop_front) {
                return Ok(Some(job));
            }
        }
        Ok(None)
    }

    fn complete(&mut self, _: &Job, _: &[Output], _: &Receipt) -> Result<(), Self::Error> {
        Ok(())
    }

    fn fail(&mut self, _: &Job, _: &Failure) -> Result<(), Self::Error> {
        Ok(())
    }
}
