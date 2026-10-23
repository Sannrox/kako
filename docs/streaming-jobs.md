# Streaming jobs

A streaming job (`kako.streaming-job/v1`, #64,
[ADR 0017](decisions/0017-streaming-checkpoints.md)) is defined by
`kako_core::streaming::StreamingJobSpec`. It has three parts:

- one partitioned log topic as its source;
- a mizuchi streaming plan with an event-time watermark;
- objects applied through the plane's streaming batch apply as its sink.

```json
{
  "contract_version": "kako.streaming-job/v1",
  "job_id": "orders-stream",
  "tenant_id": "acme",
  "namespace": "shop",
  "source": {
    "kind": "topic",
    "topic": "orders",
    "partitions": [0, 1],
    "consumer_group": "kako-orders",
    "start_offsets": { "1": 42 }
  },
  "plan": {
    "event_time": "ts",
    "watermark_delay_ms": 5000,
    "window": { "kind": "tumbling", "size_ms": 60000 },
    "group": ["sku"],
    "value": "quantity",
    "output": "append"
  },
  "sink": { "kind": "objects" }
}
```

- `start_offsets` names where a partition starts when its consumer group has
  no committed offset (ADR 0017 rule 8). A partition without an entry
  starts at 0.
- `window` is `tumbling` (`size_ms`), `sliding` (`size_ms`, `slide_ms` no
  larger than the size) or `session` (`gap_ms`).
- `output` is `complete` or `append`.
- A batch's checkpoint key is `<topic>/<partition>@<start offset>`, so a
  topic contains no `/` or `@`.

Validation refuses any other shape by name and never runs it as a batch
job: another source kind, a dataset or other sink, a missing plan, a
partition listed twice or a start offset for a partition the job does not
own. Running a streaming job is a separate issue.

## Runner

`kako_source_sekai::streaming::StreamingRunner` (#65) moves a claimed
streaming job forward under the claim's fence, one batch of one partition at
a time:

1. Heartbeat the claim.
2. Read a batch.
3. Rewind the plan to the batch start, then run it.
4. Apply the edits through the plane with the checkpoint key and the batch
   end.
5. Commit the consumer group to the end the receipt names.

A restart resumes from the group offset, or from the spec's start offset
when the group has none. It replays forward: a replayed checkpoint returns
its original receipt, whose end decides where reading continues. A partition
whose start is past retention is refused as `checkpoint_unknown`. A worker
whose fence is no longer current stops at its next heartbeat or apply.

A windowed plan must restore its state to the batch start on every rewind,
from durable state after a restart. Topic, consumer group, plane and plan
are traits. The live topic client, the plane transport and the mizuchi plan
adapter plug in there.

## Metrics

The runner keeps `StreamingMetrics` per partition (#66), and
`StreamingMetrics::render(job_id, now)` writes them in the Prometheus text
format. Every series is labelled `job_id` and `partition`, so cardinality
stays bounded by the job's partitions.

| Metric | Type | Meaning |
| --- | --- | --- |
| `kako_streaming_lag_records` | gauge | Offsets between the partition position and the source head, measured on every step before the apply. Absent until measured or when the head read failed. |
| `kako_streaming_records_applied_total` | counter | Records in batches this worker applied fresh. Replayed checkpoints are not counted. |
| `kako_streaming_batches_applied_total` | counter | Batches this worker applied fresh. |
| `kako_streaming_checkpoint_age_seconds` | gauge | Seconds since the consumer group last committed a checkpoint, or since this worker resumed the partition when it has not committed one yet. It keeps growing while apply is blocked. |

The topic trait's `head` gives the source head. The counters start at zero
for each worker.
