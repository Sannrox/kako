# ADR 0017: Streaming jobs checkpoint on the batch start position and resume from the receipt

- Status: Accepted
- Date: 2026-10-23
- Relates: [0002](0002-sandboxed-transform-runner.md), [0007](0007-single-node-execution.md), [0012](0012-governed-receipt-mapping.md)
- Source: [Issue #63](https://github.com/Sannrox/kako/issues/63), part of [#52](https://github.com/Sannrox/kako/issues/52)
- Upstream: streaming object apply (sekai-chisei#1583), streaming plan (mizuchi#72)

## Context

Batch jobs commit once. A streaming job reads without end and commits many
times, so exactly-once output depends on two choices: which key identifies
a committed batch, and where a restarted worker starts reading. A wrong key
double-applies a batch after a crash; a wrong resume point skips or repeats
input.

The plane already offers the commit primitive. `ApplyStreamingTransformBatch`
applies a batch's object edits and records its input positions in one
transaction, keyed by `(run_id, checkpoint)`. Replaying a key returns the
original receipt with `replay = true` and applies nothing. The call is fenced
by the claim (`runtime_id`, `claim_generation`, `fencing_token`), and the
fence is checked against the claim held now, not the one that wrote the key.

## Decision

1. **One source kind: a partitioned log topic.** Ordered partitions with
   integer offsets and a consumer group whose committed offsets the worker
   controls. Other source kinds stay out of v1.
2. **One sink: objects through `ApplyStreamingTransformBatch`.** Dataset
   sinks stay out of v1.
3. **One batch reads one partition.** A run may own several partitions, but
   every batch is cut from a single partition, so its start is one offset.
4. **The checkpoint key is the batch start position.** It is
   `<topic>/<partition>@<start offset>`, with the start offset inclusive.
   The key does not depend on how many records the batch holds, so a worker
   that re-reads from the same offset produces the same key even if it cuts
   a batch of a different size. `input_positions` on the request carry the
   end offset (exclusive), and that becomes the next batch's start.
5. **The plane commits first, then the consumer group.** The group offset
   is committed to a receipt's end position only after that receipt
   returns. The group may lag the plane but never leads it.
6. **Resume replays forward from the group offset.** A restarted or
   re-claimed worker starts at the group's committed offset and cuts a batch
   there. If the plane answers with `replay = true`, that batch was already
   applied: the worker seeks to the receipt's `input_positions`, commits the
   group offset there, and cuts the next batch. It continues until a batch
   commits fresh. The receipt, not the records just read, decides where
   reading continues.
7. **A new `claim_generation` replays earlier checkpoints.** A worker that
   re-claims the run gets a new generation and fence. The plane looks up a
   replayed key by `(run_id, checkpoint)` alone and returns the receipt
   written under the earlier generation, provided the caller's fence is
   current. A stale worker's call fails with the stale-claim precondition,
   so it cannot apply or replay. No core change is needed for re-claim.
8. **Without a group offset, replay from the job spec's start.** A worker
   cannot tell a partition the run never applied from one whose group offset
   was lost, so in both cases it starts at the offset the job spec names and
   replays forward as in rule 6. Replay needs the real records to cut each
   batch, because a batch that does not replay is applied as new. If the
   start offset has fallen below the topic's retention, the worker cannot
   rebuild the chain: it refuses the partition and reports
   `checkpoint_unknown` instead of guessing a start.

## Consequences

- A crash at any point applies each batch at most once: before the plane
  commit nothing is applied, and after it the same start offset replays the
  receipt. Between the plane commit and the group commit, the restart
  replays one or more receipts and moves on.
- Recovery cost is one replay call per batch the group lagged by, so
  workers should commit the group offset after every receipt.
- Receipts are the audit trail. Each one names its partition, start key, end
  position and output object versions, so the sequence of receipts for a run
  is the record of exactly what was applied.
- The plane does not check that batches are contiguous. Contiguity is the
  worker's rule 4: the next start is always the previous receipt's end.
- Rescale (moving partitions between runs) needs a fenced handover and
  stays out of v1.

## Rejected alternatives

- Keying on the batch end position, or on a digest of the batch contents.
  A worker that re-reads after a crash may cut a different-sized batch, so
  the key would change and the batch would apply twice.
- Committing the group offset before the plane commit. A crash in between
  would skip the batch.
- Treating the group offset alone as the resume point. It lags the plane,
  so it would re-apply the batches committed after it; the replay in rule 6
  is what makes the lag harmless.
- A plane RPC that reads the last committed checkpoint of a run. The replay
  chain from the group offset, or from the job spec's start, already finds
  it, including after a re-claim. The RPC would only help once the needed
  records are past retention, and rule 8 fails closed there instead. If
  long-lived runs make that case common, the RPC is the follow-up.
- Probing a key with an empty batch. A key that was never committed would
  commit the empty batch as new and skip its records.
