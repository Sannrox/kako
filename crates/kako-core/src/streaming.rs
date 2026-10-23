//! Streaming job kind (#64, ADR 0017).
//!
//! A streaming job reads one partitioned log topic, runs a mizuchi streaming
//! plan with an event-time watermark, and applies objects through the plane's
//! streaming batch apply. That is the only supported shape: validation names
//! any other source or sink kind and refuses it, so a streaming job is never
//! run as a batch job. Running a streaming job is a separate concern.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use serde::{Deserialize, Serialize};

pub const STREAMING_JOB_CONTRACT: &str = "kako.streaming-job/v1";
/// The one supported source kind: a partitioned log topic (ADR 0017 rule 1).
pub const SOURCE_TOPIC: &str = "topic";
/// The one supported sink kind: objects through streaming apply (rule 2).
pub const SINK_OBJECTS: &str = "objects";

/// A streaming job as a job source hands it to kako.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamingJobSpec {
    pub contract_version: String,
    pub job_id: String,
    pub tenant_id: String,
    pub namespace: String,
    pub source: StreamingSource,
    /// Required; optional here so a missing plan is refused by name.
    #[serde(default)]
    pub plan: Option<StreamingPlan>,
    pub sink: StreamingSink,
}

/// Where records come from. Fields beyond `kind` belong to the topic kind,
/// so another kind is refused by name rather than as a parse error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamingSource {
    pub kind: String,
    #[serde(default)]
    pub topic: String,
    /// The partitions this run owns. Every batch is cut from one of them.
    #[serde(default)]
    pub partitions: Vec<u32>,
    /// The consumer group whose committed offsets the worker controls.
    #[serde(default)]
    pub consumer_group: String,
    /// Where each partition starts when the group has no committed offset
    /// (rule 8). A partition without an entry starts at offset 0.
    #[serde(default)]
    pub start_offsets: BTreeMap<u32, u64>,
}

/// The mizuchi streaming plan the records run through.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamingPlan {
    /// Event-time column (`Timestamp` or `Int64` microseconds).
    pub event_time: String,
    /// Rows with event time behind `max_event - watermark_delay_ms` are late.
    pub watermark_delay_ms: u64,
    pub window: StreamingWindow,
    /// Grouping columns besides the window.
    #[serde(default)]
    pub group: Vec<String>,
    /// Optional `Int64` column summed per group; `COUNT(*)` always runs.
    #[serde(default)]
    pub value: Option<String>,
    pub output: StreamingOutput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum StreamingWindow {
    Tumbling { size_ms: u64 },
    Sliding { size_ms: u64, slide_ms: u64 },
    Session { gap_ms: u64 },
}

/// Which groups each trigger emits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StreamingOutput {
    Complete,
    Append,
}

/// Where each batch's output goes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StreamingSink {
    pub kind: String,
}

/// Why a streaming job spec is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamingSpecError {
    ContractVersion(String),
    MissingField(&'static str),
    UnsupportedSource(String),
    UnsupportedSink(String),
    MissingPlan,
    InvalidSource(String),
    InvalidPlan(String),
}

impl fmt::Display for StreamingSpecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ContractVersion(found) => write!(
                f,
                "contract_version must be {STREAMING_JOB_CONTRACT}, not `{found}`"
            ),
            Self::MissingField(field) => write!(f, "{field} is required"),
            Self::UnsupportedSource(kind) => write!(
                f,
                "source kind `{kind}` is not supported: a streaming job reads a `{SOURCE_TOPIC}`"
            ),
            Self::UnsupportedSink(kind) => write!(
                f,
                "sink kind `{kind}` is not supported: a streaming job applies `{SINK_OBJECTS}`"
            ),
            Self::MissingPlan => f.write_str("a streaming job needs a streaming plan"),
            Self::InvalidSource(reason) => write!(f, "invalid source: {reason}"),
            Self::InvalidPlan(reason) => write!(f, "invalid plan: {reason}"),
        }
    }
}

impl std::error::Error for StreamingSpecError {}

impl StreamingJobSpec {
    /// Parses and validates a spec from JSON. A spec that parses but fails
    /// validation is refused with the named reason.
    pub fn from_json(raw: &str) -> Result<Self, String> {
        // Kinds first: another kind's own fields would otherwise fail the
        // typed parse as unknown fields, hiding the reason.
        let value: serde_json::Value =
            serde_json::from_str(raw).map_err(|error| error.to_string())?;
        let kind = |part: &str| value[part]["kind"].as_str().map(str::to_owned);
        if let Some(source) = kind("source").filter(|kind| kind != SOURCE_TOPIC) {
            return Err(StreamingSpecError::UnsupportedSource(source).to_string());
        }
        if let Some(sink) = kind("sink").filter(|kind| kind != SINK_OBJECTS) {
            return Err(StreamingSpecError::UnsupportedSink(sink).to_string());
        }
        let spec: Self = serde_json::from_value(value).map_err(|error| error.to_string())?;
        spec.validate().map_err(|error| error.to_string())?;
        Ok(spec)
    }

    pub fn validate(&self) -> Result<(), StreamingSpecError> {
        if self.contract_version != STREAMING_JOB_CONTRACT {
            return Err(StreamingSpecError::ContractVersion(
                self.contract_version.clone(),
            ));
        }
        for (field, value) in [
            ("job_id", &self.job_id),
            ("tenant_id", &self.tenant_id),
            ("namespace", &self.namespace),
        ] {
            if value.trim().is_empty() {
                return Err(StreamingSpecError::MissingField(field));
            }
        }
        let source = &self.source;
        if source.kind != SOURCE_TOPIC {
            return Err(StreamingSpecError::UnsupportedSource(source.kind.clone()));
        }
        if self.sink.kind != SINK_OBJECTS {
            return Err(StreamingSpecError::UnsupportedSink(self.sink.kind.clone()));
        }
        // The checkpoint key is `<topic>/<partition>@<offset>` (rule 4), so
        // a topic must not contain the separators.
        if source.topic.trim().is_empty() || source.topic.contains(['/', '@']) {
            return Err(StreamingSpecError::InvalidSource(
                "topic must be non-empty and contain no `/` or `@`".into(),
            ));
        }
        if source.consumer_group.trim().is_empty() {
            return Err(StreamingSpecError::InvalidSource(
                "consumer_group is required".into(),
            ));
        }
        let partitions: BTreeSet<u32> = source.partitions.iter().copied().collect();
        if partitions.is_empty() || partitions.len() != source.partitions.len() {
            return Err(StreamingSpecError::InvalidSource(
                "partitions must list at least one partition, each once".into(),
            ));
        }
        if let Some(partition) = source
            .start_offsets
            .keys()
            .find(|partition| !partitions.contains(partition))
        {
            return Err(StreamingSpecError::InvalidSource(format!(
                "start offset for partition {partition}, which the job does not own"
            )));
        }
        let plan = self.plan.as_ref().ok_or(StreamingSpecError::MissingPlan)?;
        if plan.event_time.trim().is_empty() {
            return Err(StreamingSpecError::InvalidPlan(
                "event_time is required".into(),
            ));
        }
        let positive = |name: &str, value: u64| {
            if value == 0 {
                Err(StreamingSpecError::InvalidPlan(format!(
                    "{name} must be positive"
                )))
            } else {
                Ok(())
            }
        };
        match plan.window {
            StreamingWindow::Tumbling { size_ms } => positive("size_ms", size_ms)?,
            StreamingWindow::Sliding { size_ms, slide_ms } => {
                positive("size_ms", size_ms)?;
                positive("slide_ms", slide_ms)?;
                if slide_ms > size_ms {
                    return Err(StreamingSpecError::InvalidPlan(
                        "slide_ms must not exceed size_ms".into(),
                    ));
                }
            }
            StreamingWindow::Session { gap_ms } => positive("gap_ms", gap_ms)?,
        }
        if plan.group.iter().any(|column| column.trim().is_empty())
            || plan
                .value
                .as_ref()
                .is_some_and(|column| column.trim().is_empty())
        {
            return Err(StreamingSpecError::InvalidPlan(
                "group and value columns must be named".into(),
            ));
        }
        Ok(())
    }

    /// The checkpoint key of a batch that starts at `offset` in `partition`
    /// (ADR 0017 rule 4): `<topic>/<partition>@<start offset>`.
    pub fn checkpoint_key(&self, partition: u32, offset: u64) -> String {
        format!("{}/{partition}@{offset}", self.source.topic)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn valid() -> Value {
        json!({
            "contract_version": STREAMING_JOB_CONTRACT,
            "job_id": "orders-stream",
            "tenant_id": "acme",
            "namespace": "shop",
            "source": {
                "kind": "topic",
                "topic": "orders",
                "partitions": [0, 1],
                "consumer_group": "kako-orders",
                "start_offsets": {"1": 42}
            },
            "plan": {
                "event_time": "ts",
                "watermark_delay_ms": 5000,
                "window": {"kind": "tumbling", "size_ms": 60000},
                "group": ["sku"],
                "value": "quantity",
                "output": "append"
            },
            "sink": {"kind": "objects"}
        })
    }

    fn refused(spec: Value) -> String {
        StreamingJobSpec::from_json(&spec.to_string()).unwrap_err()
    }

    #[test]
    fn a_valid_streaming_job_round_trips() {
        let spec = StreamingJobSpec::from_json(&valid().to_string()).unwrap();
        assert_eq!(spec.source.start_offsets[&1], 42);
        assert_eq!(spec.checkpoint_key(1, 42), "orders/1@42");
        let again = StreamingJobSpec::from_json(&serde_json::to_string(&spec).unwrap()).unwrap();
        assert_eq!(again, spec);
    }

    #[test]
    fn unsupported_shapes_are_refused_by_name() {
        let mut unknown_source = valid();
        unknown_source["source"] = json!({"kind": "change_capture", "table": "orders"});
        assert_eq!(
            refused(unknown_source),
            "source kind `change_capture` is not supported: a streaming job reads a `topic`"
        );
        let mut dataset_sink = valid();
        dataset_sink["sink"] = json!({"kind": "dataset", "dataset_id": "orders-agg"});
        assert_eq!(
            refused(dataset_sink),
            "sink kind `dataset` is not supported: a streaming job applies `objects`"
        );
        let mut no_plan = valid();
        no_plan.as_object_mut().unwrap().remove("plan");
        assert_eq!(refused(no_plan), "a streaming job needs a streaming plan");
    }

    #[test]
    fn invalid_sources_and_plans_are_refused() {
        for (path, value) in [
            (("source", "topic"), json!("orders/eu")),
            (("source", "partitions"), json!([])),
            (("source", "partitions"), json!([0, 0])),
            (("source", "start_offsets"), json!({"7": 1})),
            (("source", "consumer_group"), json!("")),
            (
                ("plan", "window"),
                json!({"kind": "tumbling", "size_ms": 0}),
            ),
            (
                ("plan", "window"),
                json!({"kind": "sliding", "size_ms": 10, "slide_ms": 20}),
            ),
            (("plan", "event_time"), json!(" ")),
        ] {
            let mut spec = valid();
            spec[path.0][path.1] = value.clone();
            assert!(
                StreamingJobSpec::from_json(&spec.to_string()).is_err(),
                "{path:?} = {value} was accepted"
            );
        }
        let mut wrong_contract = valid();
        wrong_contract["contract_version"] = json!("kako.job/v1");
        assert!(refused(wrong_contract).starts_with("contract_version must be"));
    }
}
