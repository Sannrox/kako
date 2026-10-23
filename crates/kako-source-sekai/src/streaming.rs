//! Streaming runner (#65, ADR 0017): fenced object apply and resume.
//!
//! The runner holds a claim's [`Fence`] and moves one partition forward a
//! batch at a time:
//!
//! 1. heartbeat the claim, so a lost fence stops the runner before it reads;
//! 2. read a batch from the partition, starting at the current position;
//! 3. run the plan over it, giving object edits;
//! 4. apply the edits through the plane with the checkpoint key
//!    `<topic>/<partition>@<start>` and the batch's end position;
//! 5. commit the consumer group to the end the receipt names.
//!
//! The plane commits first and the group after (rule 5), and a replayed key
//! returns the original receipt, whose end decides where reading continues
//! (rule 6). A crash anywhere therefore applies each batch at most once, and
//! a restart replays forward from the group offset until a batch commits
//! fresh. A worker whose fence is no longer current is refused by the plane
//! and stops.
//!
//! Topic, group and plane are traits: the live topic client and the plane
//! transport plug in here.
//!
//! The runner keeps [`StreamingMetrics`] per partition (#66): lag behind the
//! source head, records and batches applied, and checkpoint age.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::Instant;

use kako_core::streaming::StreamingJobSpec;

use crate::{Fence, SekaiSourceError};

/// One record of a partition.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub offset: u64,
    pub payload: Vec<u8>,
}

/// One object edit, as the plane's streaming apply takes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectEdit {
    pub object_id: String,
    pub name: String,
    pub properties: BTreeMap<String, String>,
}

/// Where a batch ends: `source` is `<topic>/<partition>`, `position` the end
/// offset, exclusive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputPosition {
    pub source: String,
    pub position: String,
}

/// The plane's answer to one apply. `replay` is true when the checkpoint was
/// applied before; the positions are then the original batch's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyReceipt {
    pub checkpoint: String,
    pub input_positions: Vec<InputPosition>,
    pub replay: bool,
}

/// A partitioned log topic. Offsets may be sparse, so a read may start past
/// `from`; [`PartitionLog::earliest`] tells a gap from lost retention.
pub trait PartitionLog {
    /// The lowest offset still retained.
    fn earliest(&mut self, partition: u32) -> Result<u64, String>;
    /// The offset after the newest record: the source head lag is measured to.
    fn head(&mut self, partition: u32) -> Result<u64, String>;
    /// Up to `max` records from `from` on, in offset order; empty when none.
    fn read(&mut self, partition: u32, from: u64, max: usize) -> Result<Vec<Record>, String>;
}

/// The consumer group whose committed offsets the worker controls.
pub trait ConsumerGroup {
    fn committed(&mut self, partition: u32) -> Result<Option<u64>, String>;
    fn commit(&mut self, partition: u32, offset: u64) -> Result<(), String>;
}

/// The plane's fenced streaming apply.
pub trait StreamingPlane {
    fn heartbeat(&mut self, fence: &Fence) -> Result<(), SekaiSourceError>;
    /// Applies `edits` and records `input_positions` under `(run, checkpoint)`
    /// in one transaction, or returns the receipt already recorded there.
    fn apply(
        &mut self,
        fence: &Fence,
        checkpoint: &str,
        input_positions: &[InputPosition],
        edits: &[ObjectEdit],
    ) -> Result<ApplyReceipt, SekaiSourceError>;
}

/// The job's streaming plan, run over one batch at a time.
///
/// A windowed plan keeps state across batches, so its state must follow
/// what the plane committed, not what the plan last saw: a batch the plane
/// then answers as a replay may end earlier than the one the plan ran.
/// Before every batch the runner calls [`BatchPlan::rewind`] with the
/// batch's start, and the plan must restore its state to exactly the
/// records before that offset, from durable state when the worker
/// restarted.
pub trait BatchPlan {
    fn rewind(&mut self, partition: u32, start: u64) -> Result<(), String>;
    fn edits(&mut self, partition: u32, records: &[Record]) -> Result<Vec<ObjectEdit>, String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamError {
    /// The plane refused the fence: another worker holds the run.
    FenceLost,
    /// The partition's start fell below retention, so the replay chain
    /// cannot be rebuilt (rule 8).
    CheckpointUnknown {
        partition: u32,
    },
    /// The job does not own this partition.
    ForeignPartition(u32),
    /// The plane's receipt does not belong to the batch that was applied.
    ReceiptMismatch(String),
    Plane(SekaiSourceError),
    Log(String),
    Group(String),
    Plan(String),
}

impl std::fmt::Display for StreamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FenceLost => f.write_str("claim fence was lost"),
            Self::CheckpointUnknown { partition } => {
                write!(
                    f,
                    "checkpoint_unknown: partition {partition} is past retention"
                )
            }
            Self::ForeignPartition(partition) => {
                write!(f, "the job does not own partition {partition}")
            }
            Self::ReceiptMismatch(reason) => write!(f, "receipt mismatch: {reason}"),
            Self::Plane(error) => write!(f, "{error}"),
            Self::Log(error) => write!(f, "topic: {error}"),
            Self::Group(error) => write!(f, "consumer group: {error}"),
            Self::Plan(error) => write!(f, "plan: {error}"),
        }
    }
}

impl std::error::Error for StreamError {}

impl From<SekaiSourceError> for StreamError {
    fn from(error: SekaiSourceError) -> Self {
        match error {
            SekaiSourceError::FenceLost => Self::FenceLost,
            other => Self::Plane(other),
        }
    }
}

/// Progress of one partition, as [`StreamingRunner`] last saw it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PartitionMetrics {
    /// Offsets between the position and the source head, measured on every
    /// step. `None` until measured, or when the head read failed.
    pub lag: Option<u64>,
    /// Records in batches this worker applied fresh; replays are not counted.
    pub records_applied: u64,
    pub batches_applied: u64,
    /// When the consumer group last committed a checkpoint, or when this
    /// worker resumed the partition if it has not committed one yet.
    pub last_checkpoint: Option<Instant>,
}

/// Per-partition streaming metrics of one job.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StreamingMetrics {
    pub partitions: BTreeMap<u32, PartitionMetrics>,
}

impl StreamingMetrics {
    /// Renders the metrics in the Prometheus text format, labelled by
    /// `job_id` and partition. Checkpoint age is measured at `now`.
    pub fn render(&self, job_id: &str, now: Instant) -> String {
        let job = job_id
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n");
        let mut out = String::new();
        type Sample = fn(&PartitionMetrics, Instant) -> Option<f64>;
        let families: [(&str, &str, &str, Sample); 4] = [
            (
                "kako_streaming_lag_records",
                "gauge",
                "Offsets between the partition position and the source head.",
                |partition, _| partition.lag.map(|lag| lag as f64),
            ),
            (
                "kako_streaming_records_applied_total",
                "counter",
                "Records in batches this worker applied fresh.",
                |partition, _| Some(partition.records_applied as f64),
            ),
            (
                "kako_streaming_batches_applied_total",
                "counter",
                "Batches this worker applied fresh.",
                |partition, _| Some(partition.batches_applied as f64),
            ),
            (
                "kako_streaming_checkpoint_age_seconds",
                "gauge",
                "Seconds since the partition's last committed checkpoint.",
                |partition, now| {
                    partition
                        .last_checkpoint
                        .map(|at| now.saturating_duration_since(at).as_secs_f64())
                },
            ),
        ];
        for (name, kind, help, value) in families {
            let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} {kind}");
            for (partition, metrics) in &self.partitions {
                if let Some(value) = value(metrics, now) {
                    let _ = writeln!(
                        out,
                        "{name}{{job_id=\"{job}\",partition=\"{partition}\"}} {value}"
                    );
                }
            }
        }
        out
    }

    fn partition(&mut self, partition: u32) -> &mut PartitionMetrics {
        self.partitions.entry(partition).or_default()
    }
}

/// Moves a claimed streaming job's partitions forward under one fence.
pub struct StreamingRunner<'a, L, G, P, B> {
    pub spec: &'a StreamingJobSpec,
    pub fence: Fence,
    pub log: L,
    pub group: G,
    pub plane: P,
    pub plan: B,
    /// Most records one batch takes.
    pub max_batch: usize,
    pub metrics: StreamingMetrics,
}

impl<L, G, P, B> StreamingRunner<'_, L, G, P, B>
where
    L: PartitionLog,
    G: ConsumerGroup,
    P: StreamingPlane,
    B: BatchPlan,
{
    /// Where `partition` resumes: the group's committed offset, else the
    /// spec's start offset (rule 8), refused when below retention.
    pub fn resume_position(&mut self, partition: u32) -> Result<u64, StreamError> {
        self.require_owned(partition)?;
        let start = match self
            .group
            .committed(partition)
            .map_err(StreamError::Group)?
        {
            Some(offset) => offset,
            None => self
                .spec
                .source
                .start_offsets
                .get(&partition)
                .copied()
                .unwrap_or(0),
        };
        self.require_retained(partition, start)?;
        let metrics = self.metrics.partition(partition);
        metrics.last_checkpoint.get_or_insert_with(Instant::now);
        Ok(start)
    }

    fn require_owned(&self, partition: u32) -> Result<(), StreamError> {
        if self.spec.source.partitions.contains(&partition) {
            Ok(())
        } else {
            Err(StreamError::ForeignPartition(partition))
        }
    }

    /// Records from `start` on must still be retained; past retention the
    /// chain of batches cannot continue without a gap.
    fn require_retained(&mut self, partition: u32, start: u64) -> Result<(), StreamError> {
        if start < self.log.earliest(partition).map_err(StreamError::Log)? {
            Err(StreamError::CheckpointUnknown { partition })
        } else {
            Ok(())
        }
    }

    /// Runs one batch of `partition` from `start`. Returns where the next
    /// batch starts, or `None` when the partition has nothing to read.
    pub fn step(&mut self, partition: u32, start: u64) -> Result<Option<u64>, StreamError> {
        self.require_owned(partition)?;
        self.plane.heartbeat(&self.fence)?;
        let records = self
            .log
            .read(partition, start, self.max_batch.max(1))
            .map_err(StreamError::Log)?;
        // Checked after the read: retention that moved past `start` while
        // the batch was read would otherwise look like sparse offsets.
        self.require_retained(partition, start)?;
        // Measured before the apply, so lag keeps growing while apply is blocked.
        let head = self.log.head(partition).ok();
        self.metrics.partition(partition).lag = head.map(|head| head.saturating_sub(start));
        let Some(last) = records.last() else {
            return Ok(None);
        };
        if records.first().is_some_and(|first| first.offset < start)
            || records
                .windows(2)
                .any(|pair| pair[0].offset >= pair[1].offset)
        {
            return Err(StreamError::Log("records are out of order".into()));
        }
        let end = last.offset + 1;
        let source = format!("{}/{partition}", self.spec.source.topic);
        let checkpoint = self.spec.checkpoint_key(partition, start);
        self.plan
            .rewind(partition, start)
            .map_err(StreamError::Plan)?;
        let edits = self
            .plan
            .edits(partition, &records)
            .map_err(StreamError::Plan)?;
        let receipt = self.plane.apply(
            &self.fence,
            &checkpoint,
            &[InputPosition {
                source: source.clone(),
                position: end.to_string(),
            }],
            &edits,
        )?;
        if receipt.checkpoint != checkpoint {
            return Err(StreamError::ReceiptMismatch(format!(
                "applied {checkpoint}, the receipt names {}",
                receipt.checkpoint
            )));
        }
        // The receipt decides where reading continues: after a replay it
        // names the original batch's end, which may differ from this one's.
        let next = receipt
            .input_positions
            .iter()
            .find(|position| position.source == source)
            .and_then(|position| position.position.parse::<u64>().ok())
            .filter(|next| *next > start)
            .ok_or_else(|| StreamError::ReceiptMismatch(format!("no end position for {source}")))?;
        // Counted before the group commit: a retry after a failed commit
        // replays this checkpoint and would never count it.
        if !receipt.replay {
            let metrics = self.metrics.partition(partition);
            metrics.records_applied += records.len() as u64;
            metrics.batches_applied += 1;
        }
        self.group
            .commit(partition, next)
            .map_err(StreamError::Group)?;
        let metrics = self.metrics.partition(partition);
        metrics.last_checkpoint = Some(Instant::now());
        metrics.lag = head.map(|head| head.saturating_sub(next));
        Ok(Some(next))
    }

    /// Runs `partition` until it has nothing to read; returns the batches run.
    pub fn drain(&mut self, partition: u32) -> Result<usize, StreamError> {
        let mut position = self.resume_position(partition)?;
        let mut batches = 0;
        while let Some(next) = self.step(partition, position)? {
            position = next;
            batches += 1;
        }
        Ok(batches)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::{BTreeMap, HashMap};
    use std::rc::Rc;

    use kako_core::streaming::StreamingJobSpec;
    use serde_json::json;

    use super::*;

    fn spec() -> StreamingJobSpec {
        StreamingJobSpec::from_json(
            &json!({
                "contract_version": kako_core::streaming::STREAMING_JOB_CONTRACT,
                "job_id": "orders-stream",
                "tenant_id": "acme",
                "namespace": "shop",
                "source": {
                    "kind": "topic",
                    "topic": "orders",
                    "partitions": [0],
                    "consumer_group": "kako-orders"
                },
                "plan": {
                    "event_time": "ts",
                    "watermark_delay_ms": 0,
                    "window": {"kind": "tumbling", "size_ms": 1000},
                    "output": "append"
                },
                "sink": {"kind": "objects"}
            })
            .to_string(),
        )
        .unwrap()
    }

    fn fence(generation: i64) -> Fence {
        Fence {
            run_id: "run-1".into(),
            runtime_id: format!("worker-{generation}"),
            generation,
            fencing_token: format!("fence-{generation}"),
        }
    }

    /// A deterministic SplitMix64, so crash points reproduce from a seed.
    struct Rng(u64);

    impl Rng {
        fn below(&mut self, bound: u64) -> u64 {
            self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = self.0;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            (z ^ (z >> 31)) % bound
        }
    }

    /// Where the next call crashes the worker: after this many calls of
    /// each kind, the call fails as if the process died there.
    #[derive(Default, Clone, Copy)]
    struct CrashPlan {
        reads: Option<u32>,
        applies_before: Option<u32>,
        applies_after: Option<u32>,
        commits: Option<u32>,
    }

    fn countdown(slot: &mut Option<u32>) -> bool {
        match slot {
            Some(0) => {
                *slot = None;
                true
            }
            Some(left) => {
                *left -= 1;
                false
            }
            None => false,
        }
    }

    struct Log {
        records: Vec<Record>,
        earliest: u64,
        crash: Rc<RefCell<CrashPlan>>,
    }

    impl PartitionLog for Log {
        fn earliest(&mut self, _: u32) -> Result<u64, String> {
            Ok(self.earliest)
        }

        fn head(&mut self, _: u32) -> Result<u64, String> {
            Ok(self
                .records
                .last()
                .map_or(self.earliest, |last| last.offset + 1))
        }

        fn read(&mut self, _: u32, from: u64, max: usize) -> Result<Vec<Record>, String> {
            if countdown(&mut self.crash.borrow_mut().reads) {
                return Err("crashed while reading".into());
            }
            Ok(self
                .records
                .iter()
                .filter(|record| record.offset >= from)
                .take(max)
                .cloned()
                .collect())
        }
    }

    #[derive(Clone, Default)]
    struct Group {
        offsets: Rc<RefCell<HashMap<u32, u64>>>,
        crash: Rc<RefCell<CrashPlan>>,
    }

    impl ConsumerGroup for Group {
        fn committed(&mut self, partition: u32) -> Result<Option<u64>, String> {
            Ok(self.offsets.borrow().get(&partition).copied())
        }

        fn commit(&mut self, partition: u32, offset: u64) -> Result<(), String> {
            if countdown(&mut self.crash.borrow_mut().commits) {
                return Err("crashed before the group commit".into());
            }
            self.offsets.borrow_mut().insert(partition, offset);
            Ok(())
        }
    }

    /// The plane's semantics as ADR 0017 relies on them: a current fence is
    /// required, and `(run, checkpoint)` replays its receipt.
    #[derive(Default)]
    struct PlaneState {
        generation: i64,
        receipts: HashMap<(String, String), ApplyReceipt>,
        /// How often each object was written by a fresh apply.
        writes: BTreeMap<String, u32>,
        /// Each object's properties after the last fresh apply.
        objects: BTreeMap<String, BTreeMap<String, String>>,
    }

    #[derive(Clone)]
    struct Plane {
        state: Rc<RefCell<PlaneState>>,
        crash: Rc<RefCell<CrashPlan>>,
    }

    impl StreamingPlane for Plane {
        fn heartbeat(&mut self, fence: &Fence) -> Result<(), SekaiSourceError> {
            if fence.generation != self.state.borrow().generation {
                return Err(SekaiSourceError::FenceLost);
            }
            Ok(())
        }

        fn apply(
            &mut self,
            fence: &Fence,
            checkpoint: &str,
            input_positions: &[InputPosition],
            edits: &[ObjectEdit],
        ) -> Result<ApplyReceipt, SekaiSourceError> {
            if countdown(&mut self.crash.borrow_mut().applies_before) {
                return Err(SekaiSourceError::Transport("crashed before apply".into()));
            }
            let mut state = self.state.borrow_mut();
            if fence.generation != state.generation {
                return Err(SekaiSourceError::FenceLost);
            }
            let key = (fence.run_id.clone(), checkpoint.to_string());
            let receipt = match state.receipts.get(&key) {
                Some(original) => ApplyReceipt {
                    replay: true,
                    ..original.clone()
                },
                None => {
                    for edit in edits {
                        *state.writes.entry(edit.object_id.clone()).or_default() += 1;
                        state
                            .objects
                            .insert(edit.object_id.clone(), edit.properties.clone());
                    }
                    let receipt = ApplyReceipt {
                        checkpoint: checkpoint.into(),
                        input_positions: input_positions.to_vec(),
                        replay: false,
                    };
                    state.receipts.insert(key, receipt.clone());
                    receipt
                }
            };
            drop(state);
            // A crash after the plane committed loses only the answer.
            if countdown(&mut self.crash.borrow_mut().applies_after) {
                return Err(SekaiSourceError::Transport("crashed after apply".into()));
            }
            Ok(receipt)
        }
    }

    /// One object per record, so a record applied twice shows as a write
    /// count of two.
    struct PerRecord;

    impl BatchPlan for PerRecord {
        fn rewind(&mut self, _: u32, _: u64) -> Result<(), String> {
            Ok(())
        }

        fn edits(&mut self, _: u32, records: &[Record]) -> Result<Vec<ObjectEdit>, String> {
            Ok(records
                .iter()
                .map(|record| ObjectEdit {
                    object_id: format!("order-{}", record.offset),
                    name: String::from_utf8_lossy(&record.payload).into_owned(),
                    properties: BTreeMap::new(),
                })
                .collect())
        }
    }

    fn records(count: u64) -> Vec<Record> {
        (0..count)
            .map(|offset| Record {
                offset,
                payload: format!("order {offset}").into_bytes(),
            })
            .collect()
    }

    /// A windowed plan: a running total over every record, kept in a
    /// durable snapshot per offset (as a live plan keeps its state with the
    /// checkpoint) and written to one object per batch.
    struct RunningTotal {
        snapshots: Rc<RefCell<BTreeMap<u64, u64>>>,
        total: u64,
    }

    impl BatchPlan for RunningTotal {
        fn rewind(&mut self, _: u32, start: u64) -> Result<(), String> {
            self.total = if start == 0 {
                0
            } else {
                *self
                    .snapshots
                    .borrow()
                    .get(&start)
                    .ok_or("no plan state at the start")?
            };
            Ok(())
        }

        fn edits(&mut self, _: u32, records: &[Record]) -> Result<Vec<ObjectEdit>, String> {
            self.total += records.iter().map(|record| record.offset).sum::<u64>();
            let end = records.last().map_or(0, |record| record.offset + 1);
            self.snapshots.borrow_mut().insert(end, self.total);
            Ok(vec![ObjectEdit {
                object_id: "total".into(),
                name: "total".into(),
                properties: BTreeMap::from([("sum".to_string(), self.total.to_string())]),
            }])
        }
    }

    /// Runs `total` records to the end through restarts at random points,
    /// each a re-claim with a new fence and batch size, and returns the
    /// plane's state.
    fn run_through_crashes<B: BatchPlan>(
        seed: u64,
        total: u64,
        mut plan: impl FnMut() -> B,
    ) -> PlaneState {
        let spec = spec();
        let mut rng = Rng(seed);
        let crash = Rc::new(RefCell::new(CrashPlan::default()));
        let plane = Plane {
            state: Rc::new(RefCell::new(PlaneState::default())),
            crash: crash.clone(),
        };
        let group = Group {
            crash: crash.clone(),
            ..Group::default()
        };
        let mut restarts = 0;
        loop {
            plane.state.borrow_mut().generation += 1;
            let generation = plane.state.borrow().generation;
            let at = Some(rng.below(6) as u32);
            *crash.borrow_mut() = match rng.below(5) {
                0 => CrashPlan::default(),
                1 => CrashPlan {
                    reads: at,
                    ..Default::default()
                },
                2 => CrashPlan {
                    applies_before: at,
                    ..Default::default()
                },
                3 => CrashPlan {
                    applies_after: at,
                    ..Default::default()
                },
                _ => CrashPlan {
                    commits: at,
                    ..Default::default()
                },
            };
            let mut runner = StreamingRunner {
                spec: &spec,
                fence: fence(generation),
                log: Log {
                    records: records(total),
                    earliest: 0,
                    crash: crash.clone(),
                },
                group: group.clone(),
                plane: plane.clone(),
                plan: plan(),
                max_batch: 1 + rng.below(9) as usize,
                metrics: StreamingMetrics::default(),
            };
            if runner.drain(0).is_ok() {
                break;
            }
            restarts += 1;
            assert!(restarts < 100, "seed {seed} never finished");
        }
        assert_eq!(group.offsets.borrow()[&0], total, "seed {seed}");
        plane.state.take()
    }

    #[test]
    fn a_kill_and_restart_at_random_points_applies_every_record_exactly_once() {
        for seed in 0..200 {
            let total = 40 + Rng(seed).below(40);
            let state = run_through_crashes(seed, total, || PerRecord);
            assert_eq!(state.writes.len() as u64, total, "seed {seed}");
            assert!(
                state.writes.values().all(|count| *count == 1),
                "seed {seed}: {:?}",
                state.writes
            );
        }
    }

    #[test]
    fn a_windowed_plan_stays_exact_through_replays_and_restarts() {
        for seed in 0..200 {
            let total = 40 + Rng(seed).below(40);
            // The snapshots outlive each worker, as durable plan state does.
            let snapshots = Rc::new(RefCell::new(BTreeMap::new()));
            let state = run_through_crashes(seed, total, || RunningTotal {
                snapshots: snapshots.clone(),
                total: 0,
            });
            assert_eq!(
                state.objects["total"]["sum"],
                (0..total).sum::<u64>().to_string(),
                "seed {seed}"
            );
        }
    }

    #[test]
    fn a_stale_worker_is_fenced_and_its_apply_refused() {
        let spec = spec();
        let crash = Rc::new(RefCell::new(CrashPlan::default()));
        let plane = Plane {
            state: Rc::new(RefCell::new(PlaneState {
                generation: 1,
                ..PlaneState::default()
            })),
            crash: crash.clone(),
        };
        let group = Group {
            crash: crash.clone(),
            ..Group::default()
        };
        let worker = |generation| StreamingRunner {
            spec: &spec,
            fence: fence(generation),
            log: Log {
                records: records(10),
                earliest: 0,
                crash: crash.clone(),
            },
            group: group.clone(),
            plane: plane.clone(),
            plan: PerRecord,
            max_batch: 4,
            metrics: StreamingMetrics::default(),
        };
        let mut stale = worker(1);
        let first = stale.resume_position(0).unwrap();
        let next = stale.step(0, first).unwrap().unwrap();
        // The run is re-claimed; the earlier worker keeps going.
        plane.state.borrow_mut().generation = 2;
        assert_eq!(stale.step(0, next), Err(StreamError::FenceLost));
        // Even past its heartbeat, its apply is refused and writes nothing.
        let before = plane.state.borrow().writes.clone();
        assert_eq!(
            stale.plane.apply(&stale.fence, "orders/0@4", &[], &[]),
            Err(SekaiSourceError::FenceLost)
        );
        assert_eq!(plane.state.borrow().writes, before);
        // The current worker finishes from where the group stands.
        let mut current = worker(2);
        current.drain(0).unwrap();
        let writes = plane.state.borrow().writes.clone();
        assert_eq!(writes.len(), 10);
        assert!(writes.values().all(|count| *count == 1));
    }

    #[test]
    fn a_start_past_retention_is_refused_and_a_foreign_partition_too() {
        let spec = spec();
        let crash = Rc::new(RefCell::new(CrashPlan::default()));
        let plane = Plane {
            state: Rc::new(RefCell::new(PlaneState::default())),
            crash: crash.clone(),
        };
        let mut runner = StreamingRunner {
            spec: &spec,
            fence: fence(0),
            log: Log {
                records: records(10),
                earliest: 5,
                crash: crash.clone(),
            },
            group: Group {
                crash,
                ..Group::default()
            },
            plane,
            plan: PerRecord,
            max_batch: 4,
            metrics: StreamingMetrics::default(),
        };
        assert_eq!(
            runner.drain(0),
            Err(StreamError::CheckpointUnknown { partition: 0 })
        );
        assert_eq!(runner.drain(3), Err(StreamError::ForeignPartition(3)));
        assert_eq!(runner.step(3, 0), Err(StreamError::ForeignPartition(3)));
        // Retention that passes the next batch's start mid-run is refused,
        // never read across.
        runner.log.earliest = 0;
        let next = runner.step(0, 0).unwrap().unwrap();
        runner.log.earliest = next + 1;
        assert_eq!(
            runner.step(0, next),
            Err(StreamError::CheckpointUnknown { partition: 0 })
        );
    }

    #[test]
    fn metrics_follow_lag_throughput_and_checkpoint_age() {
        let spec = spec();
        let crash = Rc::new(RefCell::new(CrashPlan::default()));
        let mut runner = StreamingRunner {
            spec: &spec,
            fence: fence(0),
            log: Log {
                records: records(10),
                earliest: 0,
                crash: crash.clone(),
            },
            group: Group {
                crash: crash.clone(),
                ..Group::default()
            },
            plane: Plane {
                state: Rc::new(RefCell::new(PlaneState::default())),
                crash: crash.clone(),
            },
            plan: PerRecord,
            max_batch: 4,
            metrics: StreamingMetrics::default(),
        };
        let start = runner.resume_position(0).unwrap();
        runner.step(0, start).unwrap().unwrap();
        let partition = runner.metrics.partitions[&0].clone();
        assert_eq!(
            (
                partition.lag,
                partition.records_applied,
                partition.batches_applied
            ),
            (Some(6), 4, 1)
        );
        runner.drain(0).unwrap();
        let partition = runner.metrics.partitions[&0].clone();
        assert_eq!(
            (
                partition.lag,
                partition.records_applied,
                partition.batches_applied
            ),
            (Some(0), 10, 3)
        );

        // New records arrive while the plane refuses every apply: lag grows,
        // throughput stays, and the last checkpoint does not move.
        let committed = partition.last_checkpoint.unwrap();
        runner.log.records = records(15);
        crash.borrow_mut().applies_before = Some(0);
        assert!(runner.step(0, 10).is_err());
        let blocked = runner.metrics.partitions[&0].clone();
        assert_eq!(
            (
                blocked.lag,
                blocked.records_applied,
                blocked.batches_applied
            ),
            (Some(5), 10, 3)
        );
        assert_eq!(blocked.last_checkpoint, Some(committed));
        let age = |at: Instant| {
            let text = runner.metrics.render("orders-stream", at);
            text.lines()
                .find_map(|line| {
                    line.strip_prefix(
                        "kako_streaming_checkpoint_age_seconds{job_id=\"orders-stream\",partition=\"0\"} ",
                    )
                })
                .unwrap()
                .parse::<f64>()
                .unwrap()
        };
        let later = committed + std::time::Duration::from_secs(5);
        assert!(age(later) >= 5.0 && age(later) > age(committed));
        let text = runner.metrics.render("orders-stream", later);
        for line in [
            "kako_streaming_lag_records{job_id=\"orders-stream\",partition=\"0\"} 5",
            "kako_streaming_records_applied_total{job_id=\"orders-stream\",partition=\"0\"} 10",
            "kako_streaming_batches_applied_total{job_id=\"orders-stream\",partition=\"0\"} 3",
        ] {
            assert!(text.lines().any(|candidate| candidate == line), "{text}");
        }
    }
}
