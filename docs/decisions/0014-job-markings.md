# ADR 0014: Markings are opaque labels the job source attaches

- Status: Accepted
- Date: 2026-10-18
- Relates: [0012](0012-governed-receipt-mapping.md), [0013](0013-job-scoped-credentials.md)
- Source: [Issue #51](https://github.com/Sannrox/kako/issues/51)
- Upstream: mizuchi column-level propagation (Sannrox/mizuchi#144)

## Context

Jobs read marked datasets and write outputs. Without a runner check, a
pipeline can copy marked data into an unmarked file. kako does not own the
vocabulary of labels; the job source does. Credentials (#30) scoped a job to
input and output names only, so a minted credential could still read a
label it was not issued for.

## Decision

1. **The job source attaches labels.** Each input carries table-level
   markings and optional column-level markings. The job declares markings
   per output name. kako treats labels as opaque strings.
2. **Credentials cover input labels.** `CredentialScope::for_job` includes
   the union of those labels as `read_markings`. The Sekai adapter refuses
   a minted credential that omits any of them.
3. **Outputs keep the union unless declassified.** The runner refuses an
   output whose declared labels do not include the required set. A job
   source may supply an audited `declassify` decision that lists labels
   an output may omit; the decision is copied onto the receipt.
4. **SQL is column-level; Python is table-level.** Mizuchi jobs attach a
   `PolicyContext` and record per-column labels on lineage. Required
   output labels are the union of those columns. DuckDB and Python jobs
   use the table-level union of every input label, because those engines
   do not propagate columns.
5. **Receipts record what ran.** `receipt_version` 5 adds `input_markings`,
   `output_markings`, and optional `declassify`. The governed mapping fills
   `markings_in` / `markings_out` from those maps. kako-core stays free of
   mizuchi.

## Consequences

- An under-marked output fails closed before write-back.
- Selecting only unmarked columns in mizuchi SQL can produce an unmarked
  output even when another column on the same table is marked.
- A local job file may omit every marking field; unmarked jobs behave as
  they did before.

## Alternatives considered

- **Interpret labels in kako (lattices, dominance).** Rejected: the job
  source owns the vocabulary.
- **Table-level only for SQL.** Rejected: that would refuse a projection
  that dropped every marked column.
- **Governed mizuchi path writes.** Rejected: kako writes CSV itself after
  SELECT; it enforces declared output labels on the receipt path.
