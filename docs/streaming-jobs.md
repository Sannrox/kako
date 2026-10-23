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
