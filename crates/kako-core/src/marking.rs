//! Opaque marking labels on jobs, credentials, and receipts.
//!
//! The job source owns the vocabulary. kako unions labels, refuses an
//! under-marked output unless the source supplies a declassify decision,
//! and records what ran. SQL column-level propagation lives in the
//! mizuchi engine; this module is the table-level contract every engine
//! shares.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::credential::CredentialScope;
use crate::{Job, Lineage};

/// Audited decision from the job source that some input markings may be
/// dropped from outputs. Recorded on the receipt; kako does not interpret
/// `reason`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeclassifyDecision {
    /// Job-source audit id.
    pub id: String,
    /// Labels the decision allows an output to omit.
    pub from: Vec<String>,
    /// Opaque source-supplied reason, recorded as given.
    pub reason: String,
}

/// One result column with the labels that reached it (mizuchi SQL).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ColumnMarking {
    pub name: String,
    pub labels: Vec<String>,
}

/// Sorted unique labels.
pub fn normalize(labels: impl IntoIterator<Item = impl Into<String>>) -> Vec<String> {
    let set: BTreeSet<String> = labels.into_iter().map(Into::into).collect();
    set.into_iter().collect()
}

/// Union of table-level and column-level labels on every input.
pub fn input_union(job: &Job) -> Vec<String> {
    let mut set = BTreeSet::new();
    for input in &job.inputs {
        set.extend(input.markings.iter().cloned());
        for labels in input.column_markings.values() {
            set.extend(labels.iter().cloned());
        }
    }
    set.into_iter().collect()
}

/// Union of labels that reached SQL output columns. Empty column reports
/// with marked inputs fail closed: the engine did not propagate.
pub fn sql_union(job: &Job, lineage: Option<&Lineage>) -> Result<Vec<String>, String> {
    let Some(lineage) = lineage else {
        return Err("mizuchi job produced no lineage for marking propagation".into());
    };
    if lineage.column_markings.is_empty() {
        if input_union(job).is_empty() {
            return Ok(Vec::new());
        }
        return Err("mizuchi produced no column markings for marked inputs".into());
    }
    let mut set = BTreeSet::new();
    for column in &lineage.column_markings {
        set.extend(column.labels.iter().cloned());
    }
    Ok(set.into_iter().collect())
}

/// Declared labels for one output name. Missing name is unmarked.
pub fn declared_for<'a>(job: &'a Job, output: &str) -> &'a [String] {
    job.output_markings
        .get(output)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

/// Refuse outputs whose declared labels do not cover `required`, unless a
/// declassify decision lists every dropped label.
pub fn refuse_under_marked(job: &Job, required: &[String]) -> Result<(), String> {
    let required: BTreeSet<&str> = required.iter().map(String::as_str).collect();
    let allowed_drop: BTreeSet<&str> = job
        .declassify
        .as_ref()
        .map(|decision| decision.from.iter().map(String::as_str).collect())
        .unwrap_or_default();
    for output in &job.outputs {
        let declared: BTreeSet<&str> = declared_for(job, output)
            .iter()
            .map(String::as_str)
            .collect();
        let mut missing: Vec<&str> = required.difference(&declared).copied().collect();
        missing.sort_unstable();
        let dropped: Vec<&str> = missing
            .iter()
            .copied()
            .filter(|label| !allowed_drop.contains(label))
            .collect();
        if dropped.is_empty() {
            continue;
        }
        return Err(format!(
            "output `{output}` is missing marking `{}`",
            dropped[0]
        ));
    }
    Ok(())
}

impl CredentialScope {
    /// Every table- and column-level input label is in `read_markings`.
    pub fn covers_input_markings(&self, job: &Job) -> Result<(), String> {
        for label in input_union(job) {
            if !self.read_markings.iter().any(|allowed| allowed == &label) {
                return Err(format!("credential does not cover input marking `{label}`"));
            }
        }
        Ok(())
    }
}

/// Receipt map: input name → union of that input's table and column labels.
pub fn input_receipt_map(job: &Job) -> Vec<(String, Vec<String>)> {
    job.inputs
        .iter()
        .map(|input| {
            let mut labels = input.markings.clone();
            for extra in input.column_markings.values() {
                labels.extend(extra.iter().cloned());
            }
            (input.name.clone(), normalize(labels))
        })
        .collect()
}

/// Receipt map: output name → declared labels.
pub fn output_receipt_map(job: &Job) -> Vec<(String, Vec<String>)> {
    job.outputs
        .iter()
        .map(|name| {
            (
                name.clone(),
                normalize(declared_for(job, name).iter().cloned()),
            )
        })
        .collect()
}

/// Stable string for the governed `markings_in` / `markings_out` fields.
pub fn joined(entries: &[(String, Vec<String>)]) -> String {
    let mut set = BTreeSet::new();
    for (_, labels) in entries {
        set.extend(labels.iter().cloned());
    }
    set.into_iter().collect::<Vec<_>>().join(",")
}

/// Job-file maps keyed by input or output name.
pub type LabelMap = BTreeMap<String, Vec<String>>;
/// Job-file maps keyed by input name then column.
pub type ColumnLabelMap = BTreeMap<String, BTreeMap<String, Vec<String>>>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Engine, Input, IsolationTier, Job, WriteMode};
    use std::path::PathBuf;
    use std::time::Duration;

    fn job_with(
        inputs: Vec<Input>,
        outputs: Vec<String>,
        output_markings: LabelMap,
        declassify: Option<DeclassifyDecision>,
    ) -> Job {
        Job {
            id: "run-1".into(),
            engine: Engine::DuckDbSql,
            code: "SELECT 1".into(),
            inputs,
            outputs,
            write_mode: WriteMode::Overwrite,
            wall_clock: Duration::from_secs(30),
            cpu_time: None,
            memory_bytes: None,
            tenant_id: "t".into(),
            namespace: "ns".into(),
            runtime_image: None,
            isolation: IsolationTier::LinuxNative,
            output_markings,
            declassify,
            encryption: None,
        }
    }

    fn marked_input(name: &str, labels: &[&str]) -> Input {
        Input {
            name: name.into(),
            path: PathBuf::from(format!("{name}.csv")),
            markings: labels.iter().map(|s| (*s).to_string()).collect(),
            column_markings: BTreeMap::new(),
        }
    }

    #[test]
    fn under_marked_output_is_refused() {
        let job = job_with(
            vec![marked_input("orders", &["pii"])],
            vec!["out.csv".into()],
            LabelMap::new(),
            None,
        );
        let error = refuse_under_marked(&job, &input_union(&job)).unwrap_err();
        assert!(
            error.contains("out.csv") && error.contains("pii"),
            "{error}"
        );
    }

    #[test]
    fn declared_union_is_accepted() {
        let mut output_markings = LabelMap::new();
        output_markings.insert("out.csv".into(), vec!["pii".into(), "restricted".into()]);
        let job = job_with(
            vec![
                marked_input("orders", &["pii"]),
                marked_input("events", &["restricted"]),
            ],
            vec!["out.csv".into()],
            output_markings,
            None,
        );
        refuse_under_marked(&job, &input_union(&job)).unwrap();
    }

    #[test]
    fn declassify_allows_dropping_listed_labels_and_is_recorded() {
        let mut output_markings = LabelMap::new();
        output_markings.insert("out.csv".into(), vec!["restricted".into()]);
        let declassify = DeclassifyDecision {
            id: "dec-1".into(),
            from: vec!["pii".into()],
            reason: "public aggregate".into(),
        };
        let job = job_with(
            vec![
                marked_input("orders", &["pii"]),
                marked_input("events", &["restricted"]),
            ],
            vec!["out.csv".into()],
            output_markings,
            Some(declassify.clone()),
        );
        refuse_under_marked(&job, &input_union(&job)).unwrap();
        assert_eq!(job.declassify, Some(declassify));
        assert_eq!(input_receipt_map(&job)[0].1, ["pii"]);
        assert_eq!(output_receipt_map(&job)[0].1, ["restricted"]);
    }

    #[test]
    fn declassify_does_not_cover_an_unlisted_drop() {
        let declassify = DeclassifyDecision {
            id: "dec-1".into(),
            from: vec!["other".into()],
            reason: "wrong label".into(),
        };
        let job = job_with(
            vec![marked_input("orders", &["pii"])],
            vec!["out.csv".into()],
            LabelMap::new(),
            Some(declassify),
        );
        let error = refuse_under_marked(&job, &input_union(&job)).unwrap_err();
        assert!(error.contains("pii"), "{error}");
    }

    #[test]
    fn credential_without_input_markings_is_refused() {
        let job = job_with(
            vec![marked_input("orders", &["pii"])],
            vec!["out.csv".into()],
            LabelMap::new(),
            None,
        );
        let mut scope = CredentialScope::for_job("cred-1", &job);
        scope.read_markings.clear();
        let error = scope.covers_input_markings(&job).unwrap_err();
        assert!(error.contains("pii"), "{error}");
    }

    #[test]
    fn sql_union_uses_column_labels_not_the_whole_table() {
        let mut columns = BTreeMap::new();
        columns.insert("ssn".into(), vec!["pii".into()]);
        let input = Input {
            name: "orders".into(),
            path: PathBuf::from("orders.csv"),
            markings: Vec::new(),
            column_markings: columns,
        };
        let job = job_with(vec![input], vec!["out.csv".into()], LabelMap::new(), None);
        let lineage = Lineage {
            plan: "select".into(),
            reads: Vec::new(),
            writes: Vec::new(),
            column_markings: vec![ColumnMarking {
                name: "id".into(),
                labels: Vec::new(),
            }],
        };
        assert_eq!(input_union(&job), ["pii"]);
        assert!(sql_union(&job, Some(&lineage)).unwrap().is_empty());
        refuse_under_marked(&job, &sql_union(&job, Some(&lineage)).unwrap()).unwrap();
        let marked = Lineage {
            column_markings: vec![ColumnMarking {
                name: "ssn".into(),
                labels: vec!["pii".into()],
            }],
            ..lineage
        };
        let error =
            refuse_under_marked(&job, &sql_union(&job, Some(&marked)).unwrap()).unwrap_err();
        assert!(error.contains("pii"), "{error}");
    }
}
