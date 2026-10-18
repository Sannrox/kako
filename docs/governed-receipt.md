# Governed JobSpec and run-receipt mapping

kako file receipts stay `receipt_version` 3. `kako_core::governed` maps a
`Job` and `Receipt` onto the Sekai isolated-process JobSpec and `TransformRun`
JSON. See [ADR 0012](decisions/0012-governed-receipt-mapping.md).

## JobSpec (`job_spec`)

| Sekai JobSpec | kako |
| --- | --- |
| `contract_version` | `sekai.governed-transform/v1` |
| `namespace` | `job.namespace` |
| `transform_id` | `job.id` |
| `input_dataset_id` | first input name, or empty |
| `output_dataset_id` | first output name, or empty |
| `steps` / `quality_rule` | empty |
| `isolated_process.engine` | `duckdb_sql`, `mizuchi`, or `python` |
| `isolated_process.runtime_image` | Python lockfile pin; empty for SQL (not plane-admissible; see ADR 0012) |
| `isolated_process.inputs` | each input name as alias and dataset id |
| `definition_digest` | plane `definition_digest` over that envelope |

The hashed envelope does not include SQL or Python source. That identity is
`receipt.code`.

## Run receipt (`transform_run`)

| Sekai `TransformRun` | kako |
| --- | --- |
| `run_id` | `receipt.job_id` |
| `namespace` | `receipt.namespace` |
| `transform_id` | `job.id` |
| `definition_digest` | from `job_spec` |
| `input_digest` | kako input file digests (one input: that digest; several: length-prefixed name+digest SHA-256) |
| `output_digest` | kako output file digests, same rule |
| `created_at_ms` | `receipt.started_at` |
| `profile` | `isolated_process` |
| `engine` | from the JobSpec |
| `runtime_image` | `receipt.runtime_image` (catalog default when the job left it unset) |
| `dispatch_status` | `completed` |
| `trigger_cause` | `manual` |
| `rows_out` / `output_partitions` | mizuchi write lineage when present; partition `digest` stays empty (`commit_id` is not a content digest) |
| `last_input_row_id`, `incremental`, `quarantined`, `quality_rule`, `rows_in`, `lineage_parent`, `markings_*`, `trigger_ref`, `input_partitions` | empty / zero / false |

`tenant_id`, `code`, `engine_version`, `duration_ms`, usage, and `isolation`
stay on the kako file receipt only.
