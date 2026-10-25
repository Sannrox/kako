//! Mizuchi streaming plan adapter (#75).
//!
//! [`MizuchiBatchPlan`] runs one [`kako_core::streaming::StreamingPlan`]
//! through [`mizuchi::StreamQuery`]. Window state is snapshotted at the
//! receipt end so [`BatchPlan::rewind`] restores committed state after a
//! restart.

use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use arrow::array::{Array, Int64Array, StringArray, TimestampMicrosecondArray};
use arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use kako_core::streaming::{
    BatchPlan, ObjectEdit, Record, StreamingOutput, StreamingPlan, StreamingWindow,
};
use mizuchi::{
    ArrayRef, Batch, EventWindow, ParquetReadOptions, StreamOutput, StreamQuery, StreamQueryConfig,
    read_parquet,
};

/// Durable mizuchi plan. `root` holds per-partition snapshots keyed by the
/// receipt end the runner next rewinds to. Each worker gets its own work
/// directory so a takeover cannot delete another worker's in-flight query.
pub struct MizuchiBatchPlan {
    job_id: String,
    plan: StreamingPlan,
    schema: SchemaRef,
    root: PathBuf,
    work_id: String,
    query: Option<StreamQuery>,
    work_partition: Option<u32>,
    rewound_at: Option<u64>,
    processed_end: Option<u64>,
}

impl MizuchiBatchPlan {
    pub fn open(
        job_id: impl Into<String>,
        plan: StreamingPlan,
        root: impl Into<PathBuf>,
    ) -> Result<Self, String> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(|error| format!("create plan state: {error}"))?;
        if plan.output == StreamingOutput::Complete {
            return Err(
                "complete output is not supported: the streaming apply uses one mutation action"
                    .into(),
            );
        }
        for name in &plan.group {
            if matches!(
                name.as_str(),
                "window_start" | "window_end" | "count" | "sum"
            ) {
                return Err(format!(
                    "group column `{name}` collides with a reserved aggregate property"
                ));
            }
        }
        let schema = input_schema(&plan)?;
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        Ok(Self {
            job_id: job_id.into(),
            plan,
            schema,
            root,
            work_id: format!("{}-{nanos}", std::process::id()),
            query: None,
            work_partition: None,
            rewound_at: None,
            processed_end: None,
        })
    }

    fn work_dir(&self, partition: u32) -> PathBuf {
        self.root
            .join("work")
            .join(&self.work_id)
            .join(partition.to_string())
    }

    fn snapshot_dir(&self, partition: u32, start: u64) -> PathBuf {
        self.root
            .join("at")
            .join(partition.to_string())
            .join(start.to_string())
    }

    fn partition_has_snapshots(&self, partition: u32) -> bool {
        let dir = self.root.join("at").join(partition.to_string());
        fs::read_dir(&dir)
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .any(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
            })
            .unwrap_or(false)
    }

    fn config(&self, checkpoint: PathBuf, sink: PathBuf) -> StreamQueryConfig {
        StreamQueryConfig {
            checkpoint,
            sink,
            event_time: self.plan.event_time.clone(),
            delay: Duration::from_millis(self.plan.watermark_delay_ms),
            window: match self.plan.window {
                StreamingWindow::Tumbling { size_ms } => EventWindow::Tumbling {
                    size: Duration::from_millis(size_ms),
                },
                StreamingWindow::Sliding { size_ms, slide_ms } => EventWindow::Sliding {
                    size: Duration::from_millis(size_ms),
                    slide: Duration::from_millis(slide_ms),
                },
                StreamingWindow::Session { gap_ms } => EventWindow::Session {
                    gap: Duration::from_millis(gap_ms),
                },
            },
            group: self.plan.group.clone(),
            value: self.plan.value.clone(),
            output: match self.plan.output {
                StreamingOutput::Complete => StreamOutput::Complete,
                StreamingOutput::Append => StreamOutput::Append,
            },
        }
    }
}

impl BatchPlan for MizuchiBatchPlan {
    fn rewind(&mut self, partition: u32, start: u64) -> Result<(), String> {
        self.query = None;
        self.rewound_at = None;
        self.processed_end = None;
        let work = self.work_dir(partition);
        reset_dir(&work)?;
        let checkpoint = work.join("chk");
        let sink = work.join("sink");
        let snap = self.snapshot_dir(partition, start);
        let query = if snap.join("chk").join("metadata.json").exists() {
            restore_snapshot(&snap, &checkpoint, &sink)?;
            StreamQuery::open(&checkpoint).map_err(|error| error.to_string())?
        } else if start != 0 && self.partition_has_snapshots(partition) {
            return Err(format!("no plan state at {start}"));
        } else {
            let query = StreamQuery::try_new(self.schema.clone(), self.config(checkpoint, sink))
                .map_err(|error| error.to_string())?;
            publish_snapshot(&work, &snap)?;
            query
        };
        self.query = Some(query);
        self.work_partition = Some(partition);
        self.rewound_at = Some(start);
        Ok(())
    }

    fn edits(&mut self, partition: u32, records: &[Record]) -> Result<Vec<ObjectEdit>, String> {
        if self.work_partition != Some(partition) {
            return Err("rewind a different partition first".into());
        }
        let query = self.query.as_mut().ok_or("plan was not rewound")?;
        if !records.is_empty() {
            query
                .ingest(records_to_batch(&self.schema, &self.plan, records)?)
                .map_err(|error| error.to_string())?;
        }
        let trigger = query.trigger().map_err(|error| error.to_string())?;
        self.processed_end = Some(records.last().map_or(0, |record| record.offset + 1));
        if trigger.rows_out == 0 {
            return Ok(Vec::new());
        }
        let work = self.work_dir(partition);
        let path = work
            .join("sink")
            .join(format!("batch-{}.parquet", trigger.batch_id));
        let batches = read_parquet(&path, ParquetReadOptions::default())
            .map_err(|error| format!("read plan sink: {error}"))?;
        batches_to_edits(&self.job_id, partition, &self.plan, &batches)
    }

    fn persist(&mut self, partition: u32, end: u64) -> Result<(), String> {
        if self.work_partition != Some(partition) {
            return Err("rewind a different partition first".into());
        }
        let start = self.rewound_at.ok_or("plan was not rewound")?;
        let dest = self.snapshot_dir(partition, end);
        if self.processed_end != Some(end) {
            if dest.join("chk").join("metadata.json").exists() {
                return Ok(());
            }
            return Err(format!("replay has no plan state at {end}"));
        }
        let work = self.work_dir(partition);
        publish_snapshot(&work, &dest)?;
        reclaim_snapshots(&self.root, partition, start, end);
        Ok(())
    }
}

fn input_schema(plan: &StreamingPlan) -> Result<SchemaRef, String> {
    if plan.event_time.trim().is_empty() {
        return Err("event_time is required".into());
    }
    let mut fields = vec![Field::new(&plan.event_time, DataType::Int64, true)];
    for name in &plan.group {
        fields.push(Field::new(name, DataType::Utf8, true));
    }
    if let Some(value) = &plan.value {
        fields.push(Field::new(value, DataType::Int64, true));
    }
    Ok(Arc::new(Schema::new(fields)))
}

fn records_to_batch(
    schema: &SchemaRef,
    plan: &StreamingPlan,
    records: &[Record],
) -> Result<Batch, String> {
    let mut event = Vec::with_capacity(records.len());
    let mut groups: Vec<Vec<Option<String>>> = plan.group.iter().map(|_| Vec::new()).collect();
    let mut values: Vec<Option<i64>> = Vec::new();
    for record in records {
        let payload: serde_json::Value = serde_json::from_slice(&record.payload)
            .map_err(|error| format!("record {}: {error}", record.offset))?;
        let ts = json_i64(&payload[&plan.event_time])
            .ok_or_else(|| format!("record {} missing {}", record.offset, plan.event_time))?;
        event.push(Some(ts));
        for (index, name) in plan.group.iter().enumerate() {
            groups[index].push(json_utf8(&payload[name]));
        }
        if plan.value.is_some() {
            values.push(
                plan.value
                    .as_ref()
                    .and_then(|name| json_i64(&payload[name])),
            );
        }
    }
    let mut columns: Vec<ArrayRef> = vec![Arc::new(Int64Array::from(event))];
    for group in groups {
        columns.push(Arc::new(StringArray::from(group)));
    }
    if plan.value.is_some() {
        columns.push(Arc::new(Int64Array::from(values)));
    }
    Batch::try_new(schema.clone(), columns).map_err(|error| error.to_string())
}

fn json_i64(value: &serde_json::Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_u64().and_then(|n| i64::try_from(n).ok()))
}

fn json_utf8(value: &serde_json::Value) -> Option<String> {
    match value {
        serde_json::Value::Null => None,
        serde_json::Value::String(text) => Some(text.clone()),
        other => Some(other.to_string()),
    }
}

fn batches_to_edits(
    job_id: &str,
    partition: u32,
    plan: &StreamingPlan,
    batches: &[Batch],
) -> Result<Vec<ObjectEdit>, String> {
    let mut edits = Vec::new();
    for batch in batches {
        let arrow = batch.as_arrow();
        let start = downcast_ts(arrow.column(0))?;
        let end = downcast_ts(arrow.column(1))?;
        let group_cols: Vec<&StringArray> = plan
            .group
            .iter()
            .enumerate()
            .map(|(index, _)| downcast_utf8(arrow.column(2 + index)))
            .collect::<Result<_, _>>()?;
        let count = downcast_i64(arrow.column(2 + plan.group.len()))?;
        let sum = if plan.value.is_some() {
            Some(downcast_i64(arrow.column(3 + plan.group.len()))?)
        } else {
            None
        };
        for row in 0..arrow.num_rows() {
            let dims: Vec<Option<String>> = plan
                .group
                .iter()
                .enumerate()
                .map(|(index, _)| {
                    if group_cols[index].is_null(row) {
                        None
                    } else {
                        Some(group_cols[index].value(row).to_string())
                    }
                })
                .collect();
            let mut properties = std::collections::BTreeMap::from([
                ("window_start".into(), start.value(row).to_string()),
                ("window_end".into(), end.value(row).to_string()),
                ("count".into(), count.value(row).to_string()),
            ]);
            for (name, dim) in plan.group.iter().zip(&dims) {
                properties.insert(name.clone(), dim.clone().unwrap_or_default());
            }
            if let Some(sum) = sum
                && !sum.is_null(row)
            {
                properties.insert("sum".into(), sum.value(row).to_string());
            }
            let object_id = object_id(job_id, partition, start.value(row), &dims);
            edits.push(ObjectEdit {
                object_id: object_id.clone(),
                name: object_id,
                properties,
            });
        }
    }
    Ok(edits)
}

fn encode_id_token(value: &str) -> String {
    let mut out = String::new();
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => {
                out.push(byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn object_id(job_id: &str, partition: u32, window_start: i64, dims: &[Option<String>]) -> String {
    let encoded = serde_json::to_string(dims).unwrap_or_else(|_| "[]".into());
    format!(
        "{}:p{partition}:{window_start}:{}",
        encode_id_token(job_id),
        encode_id_token(&encoded)
    )
}

fn downcast_ts(column: &ArrayRef) -> Result<&TimestampMicrosecondArray, String> {
    column
        .as_any()
        .downcast_ref::<TimestampMicrosecondArray>()
        .ok_or_else(|| "plan sink window is not timestamp(us)".into())
}

fn downcast_utf8(column: &ArrayRef) -> Result<&StringArray, String> {
    column
        .as_any()
        .downcast_ref::<StringArray>()
        .ok_or_else(|| "plan sink group is not utf8".into())
}

fn downcast_i64(column: &ArrayRef) -> Result<&Int64Array, String> {
    column
        .as_any()
        .downcast_ref::<Int64Array>()
        .ok_or_else(|| "plan sink measure is not int64".into())
}

fn reset_dir(path: &Path) -> Result<(), String> {
    if path.exists() {
        fs::remove_dir_all(path).map_err(|error| format!("reset {}: {error}", path.display()))?;
    }
    fs::create_dir_all(path).map_err(|error| format!("create {}: {error}", path.display()))
}

fn copy_dir(src: &Path, dest: &Path) -> Result<(), String> {
    if !src.exists() {
        fs::create_dir_all(dest).map_err(|error| format!("create {}: {error}", dest.display()))?;
        return Ok(());
    }
    fs::create_dir_all(dest).map_err(|error| format!("create {}: {error}", dest.display()))?;
    for entry in fs::read_dir(src).map_err(|error| format!("read {}: {error}", src.display()))? {
        let entry = entry.map_err(|error| format!("read {}: {error}", src.display()))?;
        let from = entry.path();
        let to = dest.join(entry.file_name());
        if from.is_dir() {
            copy_dir(&from, &to)?;
        } else {
            fs::copy(&from, &to)
                .map_err(|error| format!("copy {} -> {}: {error}", from.display(), to.display()))?;
        }
    }
    Ok(())
}

fn rewrite_sink_meta(checkpoint: &Path, sink: &Path) -> Result<(), String> {
    let path = checkpoint.join("metadata.json");
    if !path.exists() {
        return Ok(());
    }
    let text = fs::read_to_string(&path).map_err(|error| format!("read metadata: {error}"))?;
    let mut meta: serde_json::Value =
        serde_json::from_str(&text).map_err(|error| format!("parse metadata: {error}"))?;
    meta["sink"] = serde_json::Value::String(sink.display().to_string());
    let text =
        serde_json::to_vec_pretty(&meta).map_err(|error| format!("serialize metadata: {error}"))?;
    fs::write(&path, text).map_err(|error| format!("write metadata: {error}"))?;
    File::open(&path)
        .map_err(|error| format!("sync metadata: {error}"))?
        .sync_all()
        .map_err(|error| format!("sync metadata: {error}"))?;
    Ok(())
}

fn restore_snapshot(src: &Path, dest_chk: &Path, dest_sink: &Path) -> Result<(), String> {
    copy_dir(&src.join("chk"), dest_chk)?;
    copy_dir(&src.join("sink"), dest_sink)?;
    rewrite_sink_meta(dest_chk, dest_sink)
}

fn sync_tree(path: &Path) -> Result<(), String> {
    if path.is_dir() {
        for entry in
            fs::read_dir(path).map_err(|error| format!("sync {}: {error}", path.display()))?
        {
            let entry = entry.map_err(|error| format!("sync {}: {error}", path.display()))?;
            sync_tree(&entry.path())?;
        }
    }
    File::open(path)
        .map_err(|error| format!("sync {}: {error}", path.display()))?
        .sync_all()
        .map_err(|error| format!("sync {}: {error}", path.display()))?;
    Ok(())
}

fn reclaim_snapshots(root: &Path, partition: u32, start: u64, end: u64) {
    let dir = root.join("at").join(partition.to_string());
    let Ok(entries) = fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.filter_map(Result::ok) {
        let name = entry.file_name();
        let Some(offset) = name.to_str().and_then(|s| s.parse::<u64>().ok()) else {
            continue;
        };
        // Keep this batch's start (retry), its receipt end, and later
        // snapshots a replacement worker may already have published.
        if offset == start || offset >= end {
            continue;
        }
        let _ = fs::remove_dir_all(entry.path());
    }
}

fn unique_tmp(parent: &Path, name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    parent.join(format!(".{name}.{}-{nanos}.tmp", std::process::id()))
}

fn sync_dir(path: &Path) -> Result<(), String> {
    File::open(path)
        .map_err(|error| format!("sync {}: {error}", path.display()))?
        .sync_all()
        .map_err(|error| format!("sync {}: {error}", path.display()))
}

/// Sync `dir` and its parents up through the snapshot root (`.../at`).
fn sync_lineage(dir: &Path) -> Result<(), String> {
    let mut current = Some(dir);
    for _ in 0..3 {
        let Some(path) = current else {
            break;
        };
        sync_dir(path)?;
        current = path.parent();
    }
    Ok(())
}

/// Publish `work` as `dest` if `dest` does not already exist. The directory
/// rename is the atomic publication; an existing snapshot is left alone.
fn publish_snapshot(work: &Path, dest: &Path) -> Result<(), String> {
    let parent = dest
        .parent()
        .ok_or_else(|| format!("no parent for {}", dest.display()))?;
    if dest.join("chk").join("metadata.json").exists() {
        return sync_lineage(parent);
    }
    fs::create_dir_all(parent).map_err(|error| format!("create {}: {error}", parent.display()))?;
    sync_lineage(parent)?;
    let tmp = unique_tmp(
        parent,
        &dest
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "snap".into()),
    );
    let prepared = (|| {
        reset_dir(&tmp)?;
        copy_dir(&work.join("chk"), &tmp.join("chk"))?;
        copy_dir(&work.join("sink"), &tmp.join("sink"))?;
        rewrite_sink_meta(&tmp.join("chk"), &tmp.join("sink"))?;
        sync_tree(&tmp)
    })();
    if let Err(error) = prepared {
        let _ = fs::remove_dir_all(&tmp);
        return Err(error);
    }
    match fs::rename(&tmp, dest) {
        Ok(()) => sync_lineage(parent),
        Err(error) => {
            let _ = fs::remove_dir_all(&tmp);
            if dest.join("chk").join("metadata.json").exists() {
                sync_lineage(parent)
            } else {
                Err(format!(
                    "publish {} -> {}: {error}",
                    tmp.display(),
                    dest.display()
                ))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use kako_core::streaming::StreamingJobSpec;
    use kako_source_sekai::streaming::{ConsumerGroup, StreamingMetrics, StreamingRunner};
    use kako_source_sekai::{
        Fence, LiveStreamingPlane, LocalStreamingClient, TopicGroup, TopicLog,
    };
    use serde_json::json;

    use super::*;

    fn plan() -> StreamingPlan {
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
                    "window": {"kind": "tumbling", "size_ms": 10000},
                    "group": ["sku"],
                    "value": "quantity",
                    "output": "append"
                },
                "sink": {"kind": "objects"}
            })
            .to_string(),
        )
        .unwrap()
        .plan
        .unwrap()
    }

    fn record(offset: u64, ts_us: i64, sku: &str, quantity: i64) -> Record {
        Record {
            offset,
            payload: json!({"ts": ts_us, "sku": sku, "quantity": quantity})
                .to_string()
                .into_bytes(),
        }
    }

    #[test]
    fn rewind_after_reopen_restores_the_window_and_emits_once() {
        let dir = tempfile::tempdir().unwrap();
        let mut live = MizuchiBatchPlan::open("orders-stream", plan(), dir.path()).unwrap();
        live.rewind(0, 0).unwrap();
        // Two on-time rows in the first 10s window; it does not close yet.
        let first = live
            .edits(
                0,
                &[record(0, 1_000_000, "a", 1), record(1, 2_000_000, "a", 5)],
            )
            .unwrap();
        assert!(first.is_empty(), "{first:?}");
        live.persist(0, 2).unwrap();
        drop(live);

        let mut live = MizuchiBatchPlan::open("orders-stream", plan(), dir.path()).unwrap();
        live.rewind(0, 2).unwrap();
        // Event at 11s closes the first window. A later 4s row is late.
        let closed = live.edits(0, &[record(2, 11_000_000, "a", 1)]).unwrap();
        assert_eq!(closed.len(), 1, "{closed:?}");
        assert_eq!(closed[0].properties["count"], "2");
        assert_eq!(closed[0].properties["sum"], "6");
        assert_eq!(closed[0].properties["sku"], "a");
        let late = live.edits(0, &[record(3, 4_000_000, "a", 99)]).unwrap();
        assert!(late.is_empty(), "{late:?}");
    }

    #[test]
    fn a_failed_batch_does_not_leave_speculative_window_state() {
        let dir = tempfile::tempdir().unwrap();
        let mut live = MizuchiBatchPlan::open("orders-stream", plan(), dir.path()).unwrap();
        live.rewind(0, 0).unwrap();
        // 11s then 10s together: both on-time in the open [10s, 20s) window.
        let _ = live
            .edits(
                0,
                &[record(0, 11_000_000, "a", 1), record(1, 10_000_000, "a", 1)],
            )
            .unwrap();
        drop(live);

        let mut live = MizuchiBatchPlan::open("orders-stream", plan(), dir.path()).unwrap();
        assert!(
            live.rewind(0, 2).is_err(),
            "edits must not publish at the batch end before the plane accepts it"
        );

        live.rewind(0, 0).unwrap();
        let first = live.edits(0, &[record(0, 11_000_000, "a", 1)]).unwrap();
        assert!(first.is_empty(), "{first:?}");
        live.persist(0, 1).unwrap();
        live.rewind(0, 1).unwrap();
        let late = live.edits(0, &[record(1, 10_000_000, "a", 1)]).unwrap();
        assert!(late.is_empty(), "10s is late after 11s: {late:?}");
        live.persist(0, 2).unwrap();
        live.rewind(0, 2).unwrap();
        let closed = live.edits(0, &[record(2, 21_000_000, "a", 1)]).unwrap();
        assert_eq!(closed.len(), 1, "{closed:?}");
        assert_eq!(closed[0].properties["count"], "1");
        assert_eq!(closed[0].properties["sum"], "1");
    }

    #[test]
    fn a_stale_persist_does_not_delete_later_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        let mut live = MizuchiBatchPlan::open("orders-stream", plan(), dir.path()).unwrap();
        live.rewind(0, 0).unwrap();
        let _ = live.edits(0, &[record(0, 1_000_000, "a", 1)]).unwrap();
        live.persist(0, 1).unwrap();
        live.rewind(0, 1).unwrap();
        let _ = live.edits(0, &[record(1, 2_000_000, "a", 5)]).unwrap();
        live.persist(0, 2).unwrap();

        let mut stale = MizuchiBatchPlan::open("orders-stream", plan(), dir.path()).unwrap();
        stale.rewind(0, 0).unwrap();
        let _ = stale.edits(0, &[record(0, 1_000_000, "a", 1)]).unwrap();
        stale.persist(0, 1).unwrap();

        let mut live = MizuchiBatchPlan::open("orders-stream", plan(), dir.path()).unwrap();
        live.rewind(0, 2).unwrap();
        let closed = live.edits(0, &[record(2, 11_000_000, "a", 1)]).unwrap();
        assert_eq!(closed.len(), 1, "{closed:?}");
        assert_eq!(closed[0].properties["count"], "2");
        assert_eq!(closed[0].properties["sum"], "6");
    }

    #[test]
    fn a_fresh_nonzero_start_opens_empty_state() {
        let dir = tempfile::tempdir().unwrap();
        let mut live = MizuchiBatchPlan::open("orders-stream", plan(), dir.path()).unwrap();
        live.rewind(0, 42).unwrap();
        let first = live.edits(0, &[record(42, 1_000_000, "a", 1)]).unwrap();
        assert!(first.is_empty(), "{first:?}");
        drop(live);
        let mut live = MizuchiBatchPlan::open("orders-stream", plan(), dir.path()).unwrap();
        live.rewind(0, 42).unwrap();
        let again = live.edits(0, &[record(42, 1_000_000, "a", 1)]).unwrap();
        assert!(again.is_empty(), "{again:?}");
    }

    #[test]
    fn complete_output_and_reserved_group_names_are_refused() {
        let dir = tempfile::tempdir().unwrap();
        let mut complete = plan();
        complete.output = StreamingOutput::Complete;
        let complete_err = match MizuchiBatchPlan::open("orders-stream", complete, dir.path()) {
            Ok(_) => panic!("complete output was accepted"),
            Err(error) => error,
        };
        assert!(complete_err.contains("complete"), "{complete_err}");
        let mut reserved = plan();
        reserved.group = vec!["count".into()];
        let reserved_err = match MizuchiBatchPlan::open("orders-stream", reserved, dir.path()) {
            Ok(_) => panic!("reserved group name was accepted"),
            Err(error) => error,
        };
        assert!(reserved_err.contains("count"), "{reserved_err}");
    }

    #[test]
    fn group_dimensions_keep_their_boundaries_in_the_object_id() {
        let a = object_id(
            "orders-stream",
            0,
            0,
            &[Some("a:b".into()), Some("c".into())],
        );
        let b = object_id(
            "orders-stream",
            0,
            0,
            &[Some("a".into()), Some("b:c".into())],
        );
        let empty = object_id("orders-stream", 0, 0, &[Some(String::new())]);
        let null = object_id("orders-stream", 0, 0, &[None]);
        assert_ne!(a, b);
        assert_ne!(empty, null);
        assert!(a.starts_with("orders-stream:"));
        let spaced = object_id("orders-stream", 0, 0, &[Some("New York".into())]);
        assert!(!spaced.chars().any(char::is_whitespace), "{spaced}");
        assert_ne!(
            spaced,
            object_id("orders-stream", 0, 0, &[Some("NewYork".into())])
        );
        assert_ne!(
            object_id("orders-stream", 0, 0, &[Some("a".into())]),
            object_id("other-job", 0, 0, &[Some("a".into())])
        );
    }

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
                    "window": {"kind": "tumbling", "size_ms": 10000},
                    "group": ["sku"],
                    "value": "quantity",
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

    fn produce(log: &mut TopicLog, events: &[(i64, &str, i64)]) {
        for (ts, sku, quantity) in events {
            log.append(
                0,
                json!({"ts": ts, "sku": sku, "quantity": quantity})
                    .to_string()
                    .into_bytes(),
            )
            .unwrap();
        }
    }

    /// Events match the mizuchi stream-kill fixture: the first window closes
    /// with count 2 / sum 6, late rows stay out.
    fn events() -> Vec<(i64, &'static str, i64)> {
        vec![
            (1_000_000, "a", 1),
            (2_000_000, "a", 5),
            (11_000_000, "a", 1),
            (4_000_000, "a", 99),
            (21_000_000, "a", 1),
            (12_000_000, "b", 7),
            (31_000_000, "b", 4),
        ]
    }

    #[test]
    fn a_worker_restart_applies_each_window_exactly_once() {
        let spec = spec();
        let events = events();
        let dir = tempfile::tempdir().unwrap();
        let topic = dir.path().join("topic");
        let mut log = TopicLog::open(&topic).unwrap();
        produce(&mut log, &events);

        let mut generation = 0i64;
        let mut steps_left = 2u32;
        loop {
            generation += 1;
            let mut client = LocalStreamingClient::open(dir.path().join("plane")).unwrap();
            client.hold(&fence(generation)).unwrap();
            let mut runner = StreamingRunner {
                spec: &spec,
                fence: fence(generation),
                log: TopicLog::open(&topic).unwrap(),
                group: TopicGroup::open(&topic, "kako-orders").unwrap(),
                plane: LiveStreamingPlane::new(client, "stream.record.create", "1"),
                plan: MizuchiBatchPlan::open(
                    spec.job_id.clone(),
                    spec.plan.clone().unwrap(),
                    dir.path().join("plan"),
                )
                .unwrap(),
                max_batch: 1,
                metrics: StreamingMetrics::default(),
            };
            let mut position = runner.resume_position(0).unwrap();
            let mut ran = 0u32;
            let mut drained = true;
            while let Some(next) = runner.step(0, position).unwrap() {
                position = next;
                ran += 1;
                if generation == 1 && ran >= steps_left {
                    drained = false;
                    break;
                }
            }
            if drained {
                break;
            }
            steps_left = 0;
            assert!(generation < 8, "restart never finished");
        }

        let client = LocalStreamingClient::open(dir.path().join("plane")).unwrap();
        assert!(
            client.writes().values().all(|count| *count == 1),
            "{:?}",
            client.writes()
        );
        let windows: Vec<_> = client
            .objects()
            .iter()
            .map(|(id, props)| {
                (
                    id.clone(),
                    props["sku"].clone(),
                    props["count"].clone(),
                    props["sum"].clone(),
                )
            })
            .collect();
        assert!(
            windows
                .iter()
                .any(|w| w.1 == "a" && w.2 == "2" && w.3 == "6"),
            "{windows:?}"
        );
        assert!(
            windows
                .iter()
                .all(|w| w.3 != "99" && !(w.1 == "b" && w.3 == "7")),
            "late rows must not land: {windows:?}"
        );
        let mut group = TopicGroup::open(&topic, "kako-orders").unwrap();
        assert_eq!(group.committed(0).unwrap(), Some(events.len() as u64));
    }
}
