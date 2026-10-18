//! Maps the published runtime-claim client onto this adapter.
//!
//! Connecting to a live plane is kako#35. This module translates fence loss,
//! stages claim-scoped input snapshots, and maps kako receipts onto the
//! fenced write-back payload so a live client cannot report a lost fence as
//! a successful complete.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kako_core::{Engine, Failure, Input, IsolationTier, Job, Output, Receipt, WriteMode};
use sekai_client::claim::{
    RuntimeClaimError, TransformFence, TransformWrite, transform_quota_tenant_id,
};
use sekai_client::protocol::sekai::{GovernedTransform, TransformRunInputRow, TransformWork};
use sha2::{Digest as Sha256Digest, Sha256};

use crate::{Fence, SekaiSourceError};

const ISOLATED_CLASS: &str = "isolated_process";
const DEFAULT_WALL_CLOCK: Duration = Duration::from_secs(30);
const OUTPUT_DIGEST_PREFIX: &str = "sha256-sum-v1:";
const HEX: &[u8; 16] = b"0123456789abcdef";
const MAX_OUTPUT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_OUTPUT_ROWS: usize = 1_048_576;
const MAX_FIELDS_PER_ROW: usize = 4096;
const MAX_OUTPUT_CELLS: usize = 1_048_576;

pub fn fence_from_transform(fence: &TransformFence) -> Fence {
    Fence {
        run_id: fence.run_id.clone(),
        runtime_id: fence.runtime_id.clone(),
        generation: fence.generation,
        fencing_token: fence.fencing_token.clone(),
    }
}

pub fn transform_fence(fence: &Fence) -> TransformFence {
    TransformFence {
        run_id: fence.run_id.clone(),
        runtime_id: fence.runtime_id.clone(),
        generation: fence.generation,
        fencing_token: fence.fencing_token.clone(),
    }
}

pub fn map_claim_error(error: RuntimeClaimError) -> SekaiSourceError {
    match error {
        RuntimeClaimError::FenceLost => SekaiSourceError::FenceLost,
        RuntimeClaimError::Unavailable(inner) => SekaiSourceError::Heartbeat(inner.to_string()),
        other => SekaiSourceError::Transport(other.to_string()),
    }
}

/// Build a kako job from a claimed isolated-process transform and staged inputs.
pub fn job_from_claimed(
    work: &TransformWork,
    transform: &GovernedTransform,
    inputs: Vec<Input>,
) -> Result<Job, SekaiSourceError> {
    let profile = transform.profile.trim();
    if !profile.is_empty() && profile != ISOLATED_CLASS {
        return Err(SekaiSourceError::Transport(format!(
            "transform profile {profile} is not isolated_process"
        )));
    }
    // Isolated-process proto `sql` is analytical-only and empty on the pinned
    // claim client. If a later revision carries code there, honor it; otherwise
    // fail closed rather than inventing a program.
    let code = transform.sql.trim();
    if code.is_empty() {
        return Err(SekaiSourceError::Transport(
            "isolated-process claim does not include executable code".into(),
        ));
    }
    let namespace = if work.namespace.is_empty() {
        transform.namespace.clone()
    } else {
        work.namespace.clone()
    };
    if namespace.is_empty() {
        return Err(SekaiSourceError::Transport(
            "claimed transform has no namespace".into(),
        ));
    }
    // JobSource quota keys require a tenant_id. Prefer the submitting owner
    // from the claim; community work has none and keeps today's namespace key.
    let tenant_id = match transform_quota_tenant_id(work) {
        owner if !owner.is_empty() => owner.to_string(),
        _ => namespace.clone(),
    };
    let output = transform.output_dataset_id.trim();
    if output.is_empty() {
        return Err(SekaiSourceError::Transport(
            "isolated transform has no output dataset".into(),
        ));
    }
    let outputs = vec![csv_output_name(output)?.to_string()];
    let engine = engine_from_name(&transform.engine)?;
    Ok(Job {
        id: work.run_id.clone(),
        engine,
        code: code.to_string(),
        inputs,
        outputs,
        write_mode: WriteMode::Overwrite,
        wall_clock: if transform.time_limit_ms == 0 {
            DEFAULT_WALL_CLOCK
        } else {
            Duration::from_millis(transform.time_limit_ms)
        },
        cpu_time: None,
        memory_bytes: (transform.memory_limit_bytes > 0).then_some(transform.memory_limit_bytes),
        tenant_id,
        namespace,
        runtime_image: python_runtime_image(engine, &transform.runtime_image)?,
        isolation: IsolationTier::LinuxNative,
        output_markings: Default::default(),
        declassify: None,
    })
}

/// Write claim-scoped snapshot rows into scratch as one CSV per alias.
pub fn stage_input_rows(
    scratch: &Path,
    rows: &[TransformRunInputRow],
) -> Result<Vec<Input>, SekaiSourceError> {
    if rows.len() > MAX_OUTPUT_ROWS {
        return Err(SekaiSourceError::Transport(
            "input snapshot exceeds row write-back budget".into(),
        ));
    }
    let mut cells = 0usize;
    let mut grouped: BTreeMap<String, Vec<&TransformRunInputRow>> = BTreeMap::new();
    for row in rows {
        cells = cells.saturating_add(row.values.len().max(1));
        if cells > MAX_OUTPUT_CELLS {
            return Err(SekaiSourceError::Transport(
                "input snapshot exceeds cell write-back budget".into(),
            ));
        }
        let alias = if row.alias.trim().is_empty() {
            row.dataset_id.trim()
        } else {
            row.alias.trim()
        };
        if alias.is_empty() {
            return Err(SekaiSourceError::Transport(
                "input snapshot row has no alias".into(),
            ));
        }
        grouped.entry(alias.to_string()).or_default().push(row);
    }
    let mut inputs = Vec::with_capacity(grouped.len());
    for (alias, alias_rows) in grouped {
        let path = staging_csv_path(scratch, &alias)?;
        write_csv(&path, &alias_rows)?;
        inputs.push(Input {
            name: alias,
            path,
            markings: Vec::new(),
            column_markings: Default::default(),
        });
    }
    Ok(inputs)
}

/// Map kako outputs and receipt onto the fenced write-back payload.
pub fn transform_write_from_complete(
    outputs: &[Output],
    _receipt: &Receipt,
) -> Result<TransformWrite, SekaiSourceError> {
    let rows = output_rows(outputs)?;
    let output_digest = plane_rows_digest(&rows);
    Ok(TransformWrite {
        rows,
        output_digest,
        quarantined: false,
        quality_rule: String::new(),
        partitions: Vec::new(),
    })
}

pub fn transform_write_from_failure(failure: &Failure) -> TransformWrite {
    TransformWrite {
        rows: Vec::new(),
        output_digest: String::new(),
        quarantined: true,
        quality_rule: failure.to_string(),
        partitions: Vec::new(),
    }
}

fn engine_from_name(name: &str) -> Result<Engine, SekaiSourceError> {
    match name.trim().to_ascii_lowercase().as_str() {
        "python" => Ok(Engine::Python),
        "mizuchi" => Ok(Engine::Mizuchi),
        "duckdb" | "sql" | "duckdb_sql" | "" => Ok(Engine::DuckDbSql),
        other => Err(SekaiSourceError::Transport(format!(
            "unsupported isolated-process engine {other}"
        ))),
    }
}

fn python_runtime_image(engine: Engine, value: &str) -> Result<Option<String>, SekaiSourceError> {
    if engine != Engine::Python {
        return Ok(None);
    }
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    if trimmed.starts_with("python-lockfile:sha256:") {
        return Ok(Some(trimmed.to_string()));
    }
    Err(SekaiSourceError::Transport(format!(
        "unsupported python runtime_image `{trimmed}`"
    )))
}

fn output_rows(outputs: &[Output]) -> Result<Vec<TransformRunInputRow>, SekaiSourceError> {
    let mut rows = Vec::new();
    for output in outputs {
        rows.extend(csv_rows(output)?);
    }
    Ok(rows)
}

fn csv_rows(output: &Output) -> Result<Vec<TransformRunInputRow>, SekaiSourceError> {
    csv_rows_limited(output, MAX_OUTPUT_BYTES, MAX_OUTPUT_ROWS)
}

fn csv_rows_limited(
    output: &Output,
    max_bytes: u64,
    max_rows: usize,
) -> Result<Vec<TransformRunInputRow>, SekaiSourceError> {
    let meta = fs::metadata(&output.path).map_err(|error| {
        SekaiSourceError::Transport(format!("stat output {}: {error}", output.path.display()))
    })?;
    if meta.len() > max_bytes {
        return Err(SekaiSourceError::Transport(format!(
            "output {} exceeds {max_bytes} byte write-back budget",
            output.path.display()
        )));
    }
    let text = fs::read_to_string(&output.path).map_err(|error| {
        SekaiSourceError::Transport(format!("read output {}: {error}", output.path.display()))
    })?;
    let mut records = parse_csv_records(
        &text,
        max_rows.saturating_add(1),
        MAX_FIELDS_PER_ROW,
        MAX_OUTPUT_CELLS,
    )?;
    if records.is_empty() {
        return Ok(Vec::new());
    }
    let columns = records.remove(0);
    let mut unique = std::collections::HashSet::new();
    for column in &columns {
        if !unique.insert(column) {
            return Err(SekaiSourceError::Transport(format!(
                "duplicate csv column `{column}`"
            )));
        }
    }
    let mut rows = Vec::new();
    for (index, values) in records.into_iter().enumerate() {
        if values.len() != columns.len() {
            return Err(SekaiSourceError::Transport(format!(
                "output {} row {} has {} fields, expected {}",
                output.path.display(),
                index + 1,
                values.len(),
                columns.len()
            )));
        }
        let mut map = HashMap::new();
        for (column, value) in columns.iter().zip(values) {
            map.insert(column.clone(), value);
        }
        rows.push(TransformRunInputRow {
            alias: output.name.clone(),
            dataset_id: output.name.clone(),
            row_id: i64::try_from(index + 1).unwrap_or(i64::MAX),
            values: map,
            ..Default::default()
        });
    }
    Ok(rows)
}

fn write_csv(path: &Path, rows: &[&TransformRunInputRow]) -> Result<(), SekaiSourceError> {
    let mut columns = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for row in rows {
        let mut keys: Vec<&String> = row.values.keys().collect();
        keys.sort();
        for key in keys {
            if seen.insert(key.clone()) {
                columns.push(key.clone());
            }
        }
    }
    if columns.len() > MAX_FIELDS_PER_ROW {
        return Err(SekaiSourceError::Transport(
            "csv record exceeds field budget".into(),
        ));
    }
    let expanded = columns.len().saturating_mul(rows.len().saturating_add(1));
    if expanded > MAX_OUTPUT_CELLS {
        return Err(SekaiSourceError::Transport(
            "csv exceeds cell write-back budget".into(),
        ));
    }
    let mut file = fs::File::create(path).map_err(|error| {
        SekaiSourceError::Transport(format!("stage {}: {error}", path.display()))
    })?;
    let mut written = 0u64;
    let header = columns
        .iter()
        .map(|c| csv_field(c))
        .collect::<Vec<_>>()
        .join(",");
    written = account_csv_bytes(written, header.len())?;
    writeln!(file, "{header}").map_err(stage_io)?;
    for row in rows {
        let line = columns
            .iter()
            .map(|column| csv_field(row.values.get(column).map(String::as_str).unwrap_or("")))
            .collect::<Vec<_>>()
            .join(",");
        written = account_csv_bytes(written, line.len())?;
        writeln!(file, "{line}").map_err(stage_io)?;
    }
    Ok(())
}

fn account_csv_bytes(written: u64, line_len: usize) -> Result<u64, SekaiSourceError> {
    let added = u64::try_from(line_len)
        .unwrap_or(u64::MAX)
        .saturating_add(1);
    let total = written.saturating_add(added);
    if total > MAX_OUTPUT_BYTES {
        return Err(SekaiSourceError::Transport(
            "csv exceeds byte write-back budget".into(),
        ));
    }
    Ok(total)
}

fn csv_field(value: &str) -> String {
    if value.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", value.replace('"', "\"\""))
    } else {
        value.to_string()
    }
}

fn staging_csv_path(scratch: &Path, alias: &str) -> Result<PathBuf, SekaiSourceError> {
    let alias = safe_filename(alias)?;
    let file_name = format!("{alias}.csv");
    let path = scratch.join(&file_name);
    if path.parent() != Some(scratch) {
        return Err(SekaiSourceError::Transport(
            "input snapshot path escaped the scratch directory".into(),
        ));
    }
    if let Ok(meta) = fs::symlink_metadata(&path) {
        if meta.file_type().is_symlink() {
            fs::remove_file(&path).map_err(|error| {
                SekaiSourceError::Transport(format!(
                    "replace snapshot symlink {}: {error}",
                    path.display()
                ))
            })?;
        } else if meta.is_dir() {
            return Err(SekaiSourceError::Transport(format!(
                "snapshot path {} is a directory",
                path.display()
            )));
        }
    }
    Ok(path)
}

fn parse_csv_records(
    text: &str,
    max_rows: usize,
    max_fields: usize,
    max_cells: usize,
) -> Result<Vec<Vec<String>>, SekaiSourceError> {
    let mut records = Vec::new();
    let mut fields = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut after_quoted = false;
    let mut in_record = false;
    let mut cells = 0usize;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if after_quoted && !quoted && ch != ',' && ch != '\n' && ch != '\r' {
            return Err(SekaiSourceError::Transport(
                "malformed csv: data after a quoted field".into(),
            ));
        }
        match ch {
            '"' if quoted => match chars.peek() {
                Some('"') => {
                    chars.next();
                    current.push('"');
                }
                _ => {
                    quoted = false;
                    after_quoted = true;
                }
            },
            '"' if current.is_empty() => {
                quoted = true;
                in_record = true;
                after_quoted = false;
            }
            '"' => {
                return Err(SekaiSourceError::Transport(
                    "malformed csv: quote inside an unquoted field".into(),
                ));
            }
            ',' if !quoted => {
                if fields.len() >= max_fields {
                    return Err(SekaiSourceError::Transport(
                        "csv record exceeds field budget".into(),
                    ));
                }
                fields.push(std::mem::take(&mut current));
                in_record = true;
                after_quoted = false;
            }
            '\r' if !quoted => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                finish_csv_record(
                    &mut records,
                    &mut fields,
                    &mut current,
                    max_rows,
                    max_fields,
                    max_cells,
                    &mut cells,
                )?;
                in_record = false;
                after_quoted = false;
            }
            '\n' if !quoted => {
                finish_csv_record(
                    &mut records,
                    &mut fields,
                    &mut current,
                    max_rows,
                    max_fields,
                    max_cells,
                    &mut cells,
                )?;
                in_record = false;
                after_quoted = false;
            }
            _ => {
                current.push(ch);
                in_record = true;
            }
        }
    }
    if quoted {
        return Err(SekaiSourceError::Transport(
            "unterminated quoted csv field".into(),
        ));
    }
    if in_record || !fields.is_empty() || !current.is_empty() {
        finish_csv_record(
            &mut records,
            &mut fields,
            &mut current,
            max_rows,
            max_fields,
            max_cells,
            &mut cells,
        )?;
    }
    Ok(records)
}

fn csv_output_name(name: &str) -> Result<&str, SekaiSourceError> {
    let name = safe_filename(name)?;
    let lower = name.to_ascii_lowercase();
    if lower.ends_with(".parquet")
        || lower.ends_with(".arrow")
        || lower.ends_with(".ipc")
        || lower.ends_with(".feather")
        || lower.ends_with(".orc")
    {
        return Err(SekaiSourceError::Transport(
            "isolated-process write-back only accepts csv outputs".into(),
        ));
    }
    Ok(name)
}

fn safe_filename(name: &str) -> Result<&str, SekaiSourceError> {
    let unsafe_name = name.is_empty()
        || name == "."
        || name == ".."
        || name.contains(['/', '\\', '\0'])
        || Path::new(name).is_absolute()
        || Path::new(name).components().count() != 1
        || !matches!(
            Path::new(name).components().next(),
            Some(std::path::Component::Normal(component)) if component == name
        );
    if unsafe_name {
        return Err(SekaiSourceError::Transport(
            "name is not a safe filename".into(),
        ));
    }
    Ok(name)
}

fn plane_rows_digest(rows: &[TransformRunInputRow]) -> String {
    let mut acc = [0u8; 32];
    for row in rows {
        add_u256(&mut acc, &row_canonical_hash(&row.values));
    }
    encode_digest(acc)
}

fn row_canonical_hash(row: &HashMap<String, String>) -> [u8; 32] {
    let ordered: BTreeMap<&str, &str> = row
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    Sha256::digest(serde_json::to_vec(&ordered).unwrap_or_default()).into()
}

fn add_u256(acc: &mut [u8; 32], addend: &[u8; 32]) {
    let mut carry = 0u16;
    for index in (0..32).rev() {
        let sum = u16::from(acc[index]) + u16::from(addend[index]) + carry;
        acc[index] = sum as u8;
        carry = sum >> 8;
    }
}

fn encode_digest(bytes: [u8; 32]) -> String {
    let mut hex = String::with_capacity(OUTPUT_DIGEST_PREFIX.len() + 64);
    hex.push_str(OUTPUT_DIGEST_PREFIX);
    for byte in bytes {
        hex.push(HEX[(byte >> 4) as usize] as char);
        hex.push(HEX[(byte & 0x0f) as usize] as char);
    }
    hex
}

fn finish_csv_record(
    records: &mut Vec<Vec<String>>,
    fields: &mut Vec<String>,
    current: &mut String,
    max_rows: usize,
    max_fields: usize,
    max_cells: usize,
    cells: &mut usize,
) -> Result<(), SekaiSourceError> {
    if fields.len() >= max_fields {
        return Err(SekaiSourceError::Transport(
            "csv record exceeds field budget".into(),
        ));
    }
    fields.push(std::mem::take(current));
    let added = fields.len();
    if *cells > max_cells.saturating_sub(added) {
        return Err(SekaiSourceError::Transport(
            "csv exceeds cell write-back budget".into(),
        ));
    }
    if records.len() >= max_rows {
        return Err(SekaiSourceError::Transport(
            "csv exceeds row write-back budget".into(),
        ));
    }
    *cells += added;
    records.push(std::mem::take(fields));
    Ok(())
}

fn stage_io(error: std::io::Error) -> SekaiSourceError {
    SekaiSourceError::Transport(format!("stage csv: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kako_core::Digest;
    use sekai_client::SdkError;
    use sekai_client::SdkErrorCode;
    use std::time::SystemTime;

    fn transform(engine: &str, sql: &str) -> GovernedTransform {
        GovernedTransform {
            profile: ISOLATED_CLASS.into(),
            namespace: "ops".into(),
            transform_id: "iso1".into(),
            output_dataset_id: "out".into(),
            engine: engine.into(),
            sql: sql.into(),
            runtime_image: String::new(),
            memory_limit_bytes: 32 * 1024 * 1024,
            time_limit_ms: 5_000,
            ..GovernedTransform::default()
        }
    }

    fn work() -> TransformWork {
        TransformWork {
            run_id: "run-1".into(),
            namespace: "ops".into(),
            ..TransformWork::default()
        }
    }

    fn receipt_for(job: &Job, digest: &str) -> Receipt {
        Receipt {
            job_id: job.id.clone(),
            inputs: Vec::new(),
            code: Digest("sha256:code".into()),
            engine_version: "test".into(),
            outputs: vec![("out".into(), Digest(digest.into()))],
            started_at: SystemTime::UNIX_EPOCH,
            duration: Duration::from_millis(1),
            tenant_id: job.tenant_id.clone(),
            namespace: job.namespace.clone(),
            cpu_usec: None,
            memory_peak_bytes: None,
            lineage: None,
            runtime_image: job.runtime_image.clone(),
            isolation: job.isolation,
            credential_scope: None,
            input_markings: Vec::new(),
            output_markings: Vec::new(),
            declassify: None,
        }
    }

    #[test]
    fn fence_loss_stays_distinct() {
        assert_eq!(
            map_claim_error(RuntimeClaimError::FenceLost),
            SekaiSourceError::FenceLost
        );
    }

    #[test]
    fn unavailable_heartbeat_is_not_a_successful_complete() {
        let error = map_claim_error(RuntimeClaimError::Unavailable(SdkError::new(
            SdkErrorCode::Unavailable,
        )));
        assert!(matches!(error, SekaiSourceError::Heartbeat(_)));
    }

    #[test]
    fn job_from_claimed_keys_quota_on_the_work_owner_not_the_namespace() {
        let mut owned = work();
        owned.tenant_id = " tenant-a ".into();
        let job = job_from_claimed(&owned, &transform("python", "print('ok')"), Vec::new())
            .expect("mapped");
        assert_eq!(job.tenant_id, "tenant-a");
        assert_eq!(job.namespace, "ops");
        let community = job_from_claimed(&work(), &transform("python", "print('ok')"), Vec::new())
            .expect("mapped");
        assert_eq!(community.tenant_id, "ops");
        assert_eq!(community.namespace, "ops");
    }

    #[test]
    fn job_from_claimed_maps_engine_limits_and_image() {
        let job = job_from_claimed(&work(), &transform("python", "print('ok')"), Vec::new())
            .expect("mapped");
        assert_eq!(job.id, "run-1");
        assert_eq!(job.engine, Engine::Python);
        assert_eq!(job.code, "print('ok')");
        assert_eq!(job.outputs, vec!["out".to_string()]);
        assert_eq!(job.wall_clock, Duration::from_millis(5_000));
        assert_eq!(job.memory_bytes, Some(32 * 1024 * 1024));
        assert_eq!(job.runtime_image, None);
        assert_eq!(job.isolation, IsolationTier::LinuxNative);
    }

    #[test]
    fn pinned_isolated_claim_without_code_fails_closed() {
        let mut claimed = transform("python", "");
        claimed.runtime_image = "runner:1".into();
        let error = job_from_claimed(&work(), &claimed, Vec::new()).unwrap_err();
        assert!(
            matches!(error, SekaiSourceError::Transport(message) if message.contains("does not include executable code"))
        );
    }

    #[test]
    fn sql_jobs_drop_runtime_image() {
        let mut claimed = transform("duckdb", "select 1");
        claimed.runtime_image = "runner:1".into();
        let job = job_from_claimed(&work(), &claimed, Vec::new()).expect("mapped");
        assert_eq!(job.engine, Engine::DuckDbSql);
        assert_eq!(job.runtime_image, None);
    }

    #[test]
    fn python_rejects_unsupported_runtime_image() {
        let mut claimed = transform("python", "print('ok')");
        claimed.runtime_image = "runner:1".into();
        let error = job_from_claimed(&work(), &claimed, Vec::new()).unwrap_err();
        assert!(
            matches!(error, SekaiSourceError::Transport(message) if message.contains("unsupported python runtime_image"))
        );
    }

    #[test]
    fn python_lockfile_image_is_kept() {
        let mut claimed = transform("python", "print('ok')");
        claimed.runtime_image =
            "python-lockfile:sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .into();
        let job = job_from_claimed(&work(), &claimed, Vec::new()).expect("mapped");
        assert_eq!(
            job.runtime_image.as_deref(),
            Some(claimed.runtime_image.as_str())
        );
    }

    #[test]
    fn missing_code_fails_closed() {
        let error = job_from_claimed(&work(), &transform("python", ""), Vec::new()).unwrap_err();
        assert!(
            matches!(error, SekaiSourceError::Transport(message) if message.contains("does not include executable code"))
        );
    }

    #[test]
    fn staged_rows_become_scratch_csv_inputs() {
        let scratch = tempfile::tempdir().unwrap();
        let rows = [TransformRunInputRow {
            alias: "in".into(),
            dataset_id: "in".into(),
            row_id: 1,
            values: [("id".into(), "1".into())].into_iter().collect(),
            ..Default::default()
        }];
        let inputs = stage_input_rows(scratch.path(), &rows).expect("staged");
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0].name, "in");
        assert_eq!(fs::read_to_string(&inputs[0].path).unwrap(), "id\n1\n");
    }

    #[test]
    fn complete_write_maps_receipt_digest_and_output_rows() {
        let scratch = tempfile::tempdir().unwrap();
        let out = scratch.path().join("out.csv");
        fs::write(&out, "id\n1\n").unwrap();
        let job = job_from_claimed(&work(), &transform("python", "print('ok')"), Vec::new())
            .expect("mapped");
        let write = transform_write_from_complete(
            &[Output {
                name: "out".into(),
                path: out,
                digest: Digest("sha256:out".into()),
            }],
            &receipt_for(&job, "sha256:out"),
        )
        .expect("mapped write");
        assert!(write.output_digest.starts_with("sha256-sum-v1:"));
        assert_ne!(write.output_digest, "sha256:out");
        assert_eq!(write.output_digest.len(), OUTPUT_DIGEST_PREFIX.len() + 64);
        assert!(!write.quarantined);
        assert_eq!(write.rows.len(), 1);
        assert_eq!(
            write.rows[0].values.get("id").map(String::as_str),
            Some("1")
        );
    }

    #[test]
    fn plane_row_digest_is_order_independent() {
        fn row(id: &str) -> TransformRunInputRow {
            TransformRunInputRow {
                alias: "out".into(),
                dataset_id: "out".into(),
                row_id: 1,
                values: HashMap::from([("id".into(), id.into())]),
                ..Default::default()
            }
        }
        let first = row("1");
        let second = row("2");
        assert_eq!(
            plane_rows_digest(&[first.clone(), second.clone()]),
            plane_rows_digest(&[second, first])
        );
    }

    #[test]
    fn oversized_output_fails_closed_before_parse() {
        let scratch = tempfile::tempdir().unwrap();
        let out = scratch.path().join("out.csv");
        fs::write(&out, "id\n1\n").unwrap();
        let error = csv_rows_limited(
            &Output {
                name: "out".into(),
                path: out,
                digest: Digest("sha256:out".into()),
            },
            3,
            10,
        )
        .unwrap_err();
        assert!(
            matches!(error, SekaiSourceError::Transport(message) if message.contains("write-back budget"))
        );
    }

    #[test]
    fn row_budget_is_enforced_while_parsing() {
        let error = parse_csv_records("h\n1\n2\n3\n", 2, 8, 64).unwrap_err();
        assert!(
            matches!(error, SekaiSourceError::Transport(message) if message.contains("row write-back budget"))
        );
    }

    #[test]
    fn misplaced_csv_quotes_fail_closed() {
        for text in ["value\nab\"cd\"\n", "value\n\"cd\"ef\n"] {
            let error = parse_csv_records(text, 8, 8, 64).unwrap_err();
            assert!(
                matches!(error, SekaiSourceError::Transport(ref message) if message.contains("malformed csv")),
                "{text}: {error}"
            );
        }
    }

    #[test]
    fn cell_budget_is_enforced_while_parsing() {
        let error = parse_csv_records("a,b,c\n1,2,3\n4,5,6\n", 8, 8, 5).unwrap_err();
        assert!(
            matches!(error, SekaiSourceError::Transport(message) if message.contains("cell write-back budget"))
        );
    }

    #[test]
    fn field_budget_is_enforced_while_parsing() {
        let error = parse_csv_records("a,b,c\n", 8, 2, 64).unwrap_err();
        assert!(
            matches!(error, SekaiSourceError::Transport(message) if message.contains("field budget"))
        );
    }

    #[test]
    fn empty_valued_csv_records_are_preserved() {
        let scratch = tempfile::tempdir().unwrap();
        let out = scratch.path().join("out.csv");
        fs::write(&out, "a,b\n,\n").unwrap();
        let job = job_from_claimed(&work(), &transform("python", "print('ok')"), Vec::new())
            .expect("mapped");
        let write = transform_write_from_complete(
            &[Output {
                name: "out".into(),
                path: out,
                digest: Digest("sha256:out".into()),
            }],
            &receipt_for(&job, "sha256:out"),
        )
        .expect("mapped write");
        assert_eq!(write.rows.len(), 1);
        assert_eq!(write.rows[0].values.get("a").map(String::as_str), Some(""));
        assert_eq!(write.rows[0].values.get("b").map(String::as_str), Some(""));
    }

    #[test]
    fn trailing_quoted_empty_csv_field_is_preserved() {
        let scratch = tempfile::tempdir().unwrap();
        let out = scratch.path().join("out.csv");
        fs::write(&out, "value\n\"\"").unwrap();
        let job = job_from_claimed(&work(), &transform("python", "print('ok')"), Vec::new())
            .expect("mapped");
        let write = transform_write_from_complete(
            &[Output {
                name: "out".into(),
                path: out,
                digest: Digest("sha256:out".into()),
            }],
            &receipt_for(&job, "sha256:out"),
        )
        .expect("mapped write");
        assert_eq!(write.rows.len(), 1);
        assert_eq!(
            write.rows[0].values.get("value").map(String::as_str),
            Some("")
        );
    }

    #[test]
    fn duplicate_csv_headers_fail_closed() {
        let scratch = tempfile::tempdir().unwrap();
        let out = scratch.path().join("out.csv");
        fs::write(&out, "id,id\n1,2\n").unwrap();
        let job = job_from_claimed(&work(), &transform("python", "print('ok')"), Vec::new())
            .expect("mapped");
        let error = transform_write_from_complete(
            &[Output {
                name: "out".into(),
                path: out,
                digest: Digest("sha256:out".into()),
            }],
            &receipt_for(&job, "sha256:out"),
        )
        .unwrap_err();
        assert!(
            matches!(error, SekaiSourceError::Transport(message) if message.contains("duplicate csv column"))
        );
    }

    #[test]
    fn parquet_output_dataset_is_rejected() {
        let mut claimed = transform("python", "print('ok')");
        claimed.output_dataset_id = "out.parquet".into();
        let error = job_from_claimed(&work(), &claimed, Vec::new()).unwrap_err();
        assert!(
            matches!(error, SekaiSourceError::Transport(message) if message.contains("csv outputs"))
        );
    }

    #[test]
    fn unsafe_output_dataset_is_rejected() {
        let mut claimed = transform("python", "print('ok')");
        claimed.output_dataset_id = "../out".into();
        let error = job_from_claimed(&work(), &claimed, Vec::new()).unwrap_err();
        assert!(
            matches!(error, SekaiSourceError::Transport(message) if message.contains("safe filename"))
        );
    }

    #[test]
    fn failure_write_is_quarantined_and_carries_no_rows() {
        let write = transform_write_from_failure(&Failure::Job("boom".into()));
        assert!(write.quarantined);
        assert!(write.rows.is_empty());
        assert_eq!(write.quality_rule, "job: boom");
    }

    #[test]
    fn unsafe_snapshot_alias_is_rejected() {
        let scratch = tempfile::tempdir().unwrap();
        for alias in ["../escape", "/tmp/x", "a/b", ".", ".."] {
            let rows = [TransformRunInputRow {
                alias: alias.into(),
                dataset_id: alias.into(),
                row_id: 1,
                values: [("id".into(), "1".into())].into_iter().collect(),
                ..Default::default()
            }];
            let error = stage_input_rows(scratch.path(), &rows).unwrap_err();
            assert!(
                matches!(error, SekaiSourceError::Transport(ref message) if message.contains("safe filename")),
                "{alias}: {error}"
            );
        }
        assert!(scratch.path().read_dir().unwrap().next().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_symlink_is_replaced_not_followed() {
        let scratch = tempfile::tempdir().unwrap();
        let target = scratch.path().join("secret");
        fs::write(&target, "secret\n").unwrap();
        std::os::unix::fs::symlink(&target, scratch.path().join("in.csv")).unwrap();
        let rows = [TransformRunInputRow {
            alias: "in".into(),
            dataset_id: "in".into(),
            row_id: 1,
            values: [("id".into(), "1".into())].into_iter().collect(),
            ..Default::default()
        }];
        let inputs = stage_input_rows(scratch.path(), &rows).expect("staged");
        assert_eq!(fs::read_to_string(&target).unwrap(), "secret\n");
        assert_eq!(fs::read_to_string(&inputs[0].path).unwrap(), "id\n1\n");
        assert!(
            !fs::symlink_metadata(&inputs[0].path)
                .unwrap()
                .file_type()
                .is_symlink()
        );
    }

    #[test]
    fn quoted_multiline_csv_round_trips_on_write_back() {
        let scratch = tempfile::tempdir().unwrap();
        let rows = [TransformRunInputRow {
            alias: "in".into(),
            dataset_id: "in".into(),
            row_id: 1,
            values: [
                ("id".into(), "1".into()),
                ("message".into(), "hello\nworld".into()),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        }];
        let inputs = stage_input_rows(scratch.path(), &rows).expect("staged");
        let job = job_from_claimed(&work(), &transform("python", "print('ok')"), Vec::new())
            .expect("mapped");
        let write = transform_write_from_complete(
            &[Output {
                name: "in".into(),
                path: inputs[0].path.clone(),
                digest: Digest("sha256:out".into()),
            }],
            &receipt_for(&job, "sha256:out"),
        )
        .expect("mapped write");
        assert_eq!(write.rows.len(), 1);
        assert_eq!(
            write.rows[0].values.get("message").map(String::as_str),
            Some("hello\nworld")
        );
    }
}
