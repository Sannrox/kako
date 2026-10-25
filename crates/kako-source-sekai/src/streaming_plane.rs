//! Plane adapter for the streaming runner (#75).
//!
//! [`LiveStreamingPlane`] heartbeats the claim and applies a batch through
//! [`StreamingBatchClient`]. A local client keeps the plane's apply
//! semantics on disk so tests restart a worker without a live plane. The
//! `sekai-client` feature maps the same payload onto
//! `ApplyStreamingTransformBatch`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::streaming::{ApplyReceipt, InputPosition, ObjectEdit, StreamingPlane};
use crate::streaming_fs::{DirLock, atomic_write_json, read_json};
use crate::{Fence, SekaiSourceError};

/// One fenced streaming apply, as the plane RPC takes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamingBatchApply {
    pub checkpoint: String,
    pub action_type_id: String,
    pub action_type_version: String,
    pub edits: Vec<ObjectEdit>,
    pub input_positions: Vec<InputPosition>,
}

/// Heartbeat and fenced apply. The live Sekai client and the local plane
/// both implement this.
pub trait StreamingBatchClient {
    fn heartbeat_transform(&mut self, fence: &Fence, ttl: Duration)
    -> Result<(), SekaiSourceError>;
    fn apply_streaming_transform_batch(
        &mut self,
        fence: &Fence,
        apply: StreamingBatchApply,
    ) -> Result<ApplyReceipt, SekaiSourceError>;
}

/// Streaming plane over a [`StreamingBatchClient`].
pub struct LiveStreamingPlane<C> {
    pub client: C,
    pub action_type_id: String,
    pub action_type_version: String,
    pub ttl: Duration,
}

impl<C> LiveStreamingPlane<C> {
    pub fn new(
        client: C,
        action_type_id: impl Into<String>,
        action_type_version: impl Into<String>,
    ) -> Self {
        Self {
            client,
            action_type_id: action_type_id.into(),
            action_type_version: action_type_version.into(),
            ttl: Duration::from_secs(30),
        }
    }
}

impl<C: StreamingBatchClient> StreamingPlane for LiveStreamingPlane<C> {
    fn heartbeat(&mut self, fence: &Fence) -> Result<(), SekaiSourceError> {
        self.client.heartbeat_transform(fence, self.ttl)
    }

    fn apply(
        &mut self,
        fence: &Fence,
        checkpoint: &str,
        input_positions: &[InputPosition],
        edits: &[ObjectEdit],
    ) -> Result<ApplyReceipt, SekaiSourceError> {
        self.client.apply_streaming_transform_batch(
            fence,
            StreamingBatchApply {
                checkpoint: checkpoint.to_string(),
                action_type_id: self.action_type_id.clone(),
                action_type_version: self.action_type_version.clone(),
                edits: edits.to_vec(),
                input_positions: input_positions.to_vec(),
            },
        )
    }
}

/// In-process plane: `(run, checkpoint)` replays, a current fence is
/// required, and object writes land only on a fresh apply.
#[derive(Debug, Clone)]
pub struct LocalStreamingClient {
    root: PathBuf,
    state: LocalPlaneState,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct LocalPlaneState {
    generation: i64,
    runtime_id: String,
    fencing_token: String,
    run_id: String,
    receipts: BTreeMap<String, BTreeMap<String, ApplyReceipt>>,
    objects: BTreeMap<String, BTreeMap<String, String>>,
    writes: BTreeMap<String, u32>,
}

impl LocalStreamingClient {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, SekaiSourceError> {
        let root = root.into();
        let _lock = DirLock::acquire(&root).map_err(fs_err)?;
        let state = load_state(&root)?;
        Ok(Self { root, state })
    }

    /// The fence this plane currently holds. A worker that re-claims the
    /// run calls this with the new generation before it applies.
    pub fn hold(&mut self, fence: &Fence) -> Result<(), SekaiSourceError> {
        self.with_locked(|state| {
            state.generation = fence.generation;
            state.runtime_id = fence.runtime_id.clone();
            state.fencing_token = fence.fencing_token.clone();
            state.run_id = fence.run_id.clone();
            Ok(())
        })
    }

    pub fn objects(&self) -> &BTreeMap<String, BTreeMap<String, String>> {
        &self.state.objects
    }

    pub fn writes(&self) -> &BTreeMap<String, u32> {
        &self.state.writes
    }

    fn require_fence(state: &LocalPlaneState, fence: &Fence) -> Result<(), SekaiSourceError> {
        if fence.generation != state.generation
            || fence.runtime_id != state.runtime_id
            || fence.fencing_token != state.fencing_token
            || fence.run_id != state.run_id
        {
            Err(SekaiSourceError::FenceLost)
        } else {
            Ok(())
        }
    }

    fn with_locked<T>(
        &mut self,
        f: impl FnOnce(&mut LocalPlaneState) -> Result<T, SekaiSourceError>,
    ) -> Result<T, SekaiSourceError> {
        let _lock = DirLock::acquire(&self.root).map_err(fs_err)?;
        self.state = load_state(&self.root)?;
        let result = f(&mut self.state)?;
        save_state(&self.root, &self.state)?;
        Ok(result)
    }
}

impl StreamingBatchClient for LocalStreamingClient {
    fn heartbeat_transform(
        &mut self,
        fence: &Fence,
        _ttl: Duration,
    ) -> Result<(), SekaiSourceError> {
        self.with_locked(|state| Self::require_fence(state, fence))
    }

    fn apply_streaming_transform_batch(
        &mut self,
        fence: &Fence,
        apply: StreamingBatchApply,
    ) -> Result<ApplyReceipt, SekaiSourceError> {
        self.with_locked(|state| {
            Self::require_fence(state, fence)?;
            if apply.action_type_id.trim().is_empty() || apply.action_type_version.trim().is_empty()
            {
                return Err(SekaiSourceError::Transport(
                    "action_type_id and action_type_version required".into(),
                ));
            }
            if let Some(original) = state
                .receipts
                .get(&fence.run_id)
                .and_then(|run| run.get(&apply.checkpoint))
            {
                return Ok(ApplyReceipt {
                    replay: true,
                    ..original.clone()
                });
            }
            for edit in &apply.edits {
                *state.writes.entry(edit.object_id.clone()).or_default() += 1;
                state
                    .objects
                    .insert(edit.object_id.clone(), edit.properties.clone());
            }
            let receipt = ApplyReceipt {
                checkpoint: apply.checkpoint.clone(),
                input_positions: apply.input_positions,
                replay: false,
            };
            state
                .receipts
                .entry(fence.run_id.clone())
                .or_default()
                .insert(apply.checkpoint, receipt.clone());
            Ok(receipt)
        })
    }
}

fn state_path(root: &Path) -> PathBuf {
    root.join("plane.json")
}

fn fs_err(error: String) -> SekaiSourceError {
    SekaiSourceError::Transport(error)
}

fn load_state(root: &Path) -> Result<LocalPlaneState, SekaiSourceError> {
    Ok(read_json(&state_path(root))
        .map_err(fs_err)?
        .unwrap_or_default())
}

fn save_state(root: &Path, state: &LocalPlaneState) -> Result<(), SekaiSourceError> {
    atomic_write_json(&state_path(root), state).map_err(fs_err)
}

#[cfg(feature = "sekai-client")]
mod sekai {
    use sekai_client::claim::StreamingTransformApply;
    use sekai_client::protocol::sekai::{
        ApplyStreamingTransformBatchResponse, StreamingTransformInputPosition,
        StreamingTransformObjectEdit,
    };
    use sekai_client::{ClientConfig, CoreLoopClient, GrpcTransport};

    use super::*;
    use crate::live::{map_claim_error, transform_fence};

    /// Runtime-claim client mapped onto [`StreamingBatchClient`].
    pub struct SekaiStreamingClient {
        client: CoreLoopClient<GrpcTransport>,
        runtime: tokio::runtime::Runtime,
    }

    impl SekaiStreamingClient {
        pub fn connect(config: ClientConfig) -> Result<Self, SekaiSourceError> {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|error| SekaiSourceError::Transport(error.to_string()))?;
            let client = runtime
                .block_on(CoreLoopClient::connect(config))
                .map_err(|error| SekaiSourceError::Transport(error.to_string()))?;
            Ok(Self { client, runtime })
        }
    }

    /// Wire edits for `ApplyStreamingTransformBatch`.
    pub fn wire_edits(edits: &[ObjectEdit]) -> Vec<StreamingTransformObjectEdit> {
        edits
            .iter()
            .map(|edit| StreamingTransformObjectEdit {
                object_id: edit.object_id.clone(),
                name: edit.name.clone(),
                properties: edit
                    .properties
                    .iter()
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect(),
            })
            .collect()
    }

    /// Wire positions for `ApplyStreamingTransformBatch`.
    pub fn wire_positions(positions: &[InputPosition]) -> Vec<StreamingTransformInputPosition> {
        positions
            .iter()
            .map(|position| StreamingTransformInputPosition {
                source: position.source.clone(),
                position: position.position.clone(),
            })
            .collect()
    }

    /// Maps a plane apply response onto the runner receipt.
    pub fn apply_receipt_from_response(
        response: ApplyStreamingTransformBatchResponse,
    ) -> Result<ApplyReceipt, SekaiSourceError> {
        let receipt = response.receipt.ok_or_else(|| {
            SekaiSourceError::Transport("streaming apply returned no receipt".into())
        })?;
        Ok(ApplyReceipt {
            checkpoint: receipt.checkpoint,
            input_positions: receipt
                .input_positions
                .into_iter()
                .map(|position| InputPosition {
                    source: position.source,
                    position: position.position,
                })
                .collect(),
            replay: receipt.replay,
        })
    }

    pub fn streaming_transform_apply(apply: StreamingBatchApply) -> StreamingTransformApply {
        StreamingTransformApply {
            checkpoint: apply.checkpoint,
            action_type_id: apply.action_type_id,
            action_type_version: apply.action_type_version,
            edits: wire_edits(&apply.edits),
            input_positions: wire_positions(&apply.input_positions),
        }
    }

    impl StreamingBatchClient for SekaiStreamingClient {
        fn heartbeat_transform(
            &mut self,
            fence: &Fence,
            ttl: Duration,
        ) -> Result<(), SekaiSourceError> {
            self.runtime
                .block_on(
                    self.client
                        .runtime_claim()
                        .heartbeat_transform(&transform_fence(fence), ttl),
                )
                .map(|_| ())
                .map_err(map_claim_error)
        }

        fn apply_streaming_transform_batch(
            &mut self,
            fence: &Fence,
            apply: StreamingBatchApply,
        ) -> Result<ApplyReceipt, SekaiSourceError> {
            let response = self
                .runtime
                .block_on(self.client.runtime_claim().apply_streaming_transform_batch(
                    &transform_fence(fence),
                    streaming_transform_apply(apply),
                ))
                .map_err(map_claim_error)?;
            apply_receipt_from_response(response)
        }
    }
}

#[cfg(feature = "sekai-client")]
pub use sekai::{
    SekaiStreamingClient, apply_receipt_from_response, streaming_transform_apply, wire_edits,
    wire_positions,
};

#[cfg(test)]
mod tests {
    use super::*;

    fn fence(generation: i64) -> Fence {
        Fence {
            run_id: "run-1".into(),
            runtime_id: format!("worker-{generation}"),
            generation,
            fencing_token: format!("fence-{generation}"),
        }
    }

    #[test]
    fn live_plane_heartbeats_and_maps_apply_onto_the_client() {
        struct Spy {
            heartbeats: u32,
            applies: Vec<StreamingBatchApply>,
        }
        impl StreamingBatchClient for Spy {
            fn heartbeat_transform(
                &mut self,
                _fence: &Fence,
                _ttl: Duration,
            ) -> Result<(), SekaiSourceError> {
                self.heartbeats += 1;
                Ok(())
            }
            fn apply_streaming_transform_batch(
                &mut self,
                _fence: &Fence,
                apply: StreamingBatchApply,
            ) -> Result<ApplyReceipt, SekaiSourceError> {
                let receipt = ApplyReceipt {
                    checkpoint: apply.checkpoint.clone(),
                    input_positions: apply.input_positions.clone(),
                    replay: false,
                };
                self.applies.push(apply);
                Ok(receipt)
            }
        }

        let mut plane = LiveStreamingPlane::new(
            Spy {
                heartbeats: 0,
                applies: Vec::new(),
            },
            "stream.record.create",
            "1",
        );
        let held = fence(1);
        plane.heartbeat(&held).unwrap();
        let receipt = plane
            .apply(
                &held,
                "orders/0@0",
                &[InputPosition {
                    source: "orders/0".into(),
                    position: "2".into(),
                }],
                &[ObjectEdit {
                    object_id: "rec-0".into(),
                    name: "rec-0".into(),
                    properties: BTreeMap::from([("n".into(), "0".into())]),
                }],
            )
            .unwrap();
        assert_eq!(receipt.checkpoint, "orders/0@0");
        assert_eq!(plane.client.heartbeats, 1);
        assert_eq!(
            plane.client.applies[0].action_type_id,
            "stream.record.create"
        );
        assert_eq!(plane.client.applies[0].checkpoint, "orders/0@0");
        assert_eq!(plane.client.applies[0].edits[0].object_id, "rec-0");
    }

    #[test]
    fn a_reopened_local_plane_replays_and_a_new_generation_fences_the_old_worker() {
        let dir = tempfile::tempdir().unwrap();
        let mut client = LocalStreamingClient::open(dir.path()).unwrap();
        client.hold(&fence(1)).unwrap();
        let mut plane = LiveStreamingPlane::new(client, "stream.record.create", "1");
        let first = plane
            .apply(
                &fence(1),
                "orders/0@0",
                &[InputPosition {
                    source: "orders/0".into(),
                    position: "2".into(),
                }],
                &[ObjectEdit {
                    object_id: "rec-0".into(),
                    name: "rec-0".into(),
                    properties: BTreeMap::from([("n".into(), "1".into())]),
                }],
            )
            .unwrap();
        assert!(!first.replay);
        drop(plane);

        let mut client = LocalStreamingClient::open(dir.path()).unwrap();
        assert_eq!(client.writes()["rec-0"], 1);
        client.hold(&fence(2)).unwrap();
        let mut plane = LiveStreamingPlane::new(client, "stream.record.create", "1");
        assert_eq!(plane.heartbeat(&fence(1)), Err(SekaiSourceError::FenceLost));
        let replay = plane
            .apply(
                &fence(2),
                "orders/0@0",
                &[InputPosition {
                    source: "orders/0".into(),
                    position: "9".into(),
                }],
                &[ObjectEdit {
                    object_id: "rec-0".into(),
                    name: "rec-0".into(),
                    properties: BTreeMap::from([("n".into(), "99".into())]),
                }],
            )
            .unwrap();
        assert!(replay.replay);
        assert_eq!(replay.input_positions[0].position, "2");
        assert_eq!(plane.client.writes()["rec-0"], 1);
        assert_eq!(plane.client.objects()["rec-0"]["n"], "1");
    }

    #[test]
    fn a_replaced_worker_is_refused_from_durable_ownership() {
        let dir = tempfile::tempdir().unwrap();
        let mut stale = LocalStreamingClient::open(dir.path()).unwrap();
        stale.hold(&fence(1)).unwrap();
        let mut current = LocalStreamingClient::open(dir.path()).unwrap();
        current.hold(&fence(2)).unwrap();
        assert_eq!(
            stale.heartbeat_transform(&fence(1), Duration::from_secs(1)),
            Err(SekaiSourceError::FenceLost)
        );
        assert_eq!(
            stale.apply_streaming_transform_batch(
                &fence(1),
                StreamingBatchApply {
                    checkpoint: "orders/0@0".into(),
                    action_type_id: "stream.record.create".into(),
                    action_type_version: "1".into(),
                    edits: vec![ObjectEdit {
                        object_id: "rec-0".into(),
                        name: "rec-0".into(),
                        properties: BTreeMap::new(),
                    }],
                    input_positions: vec![],
                }
            ),
            Err(SekaiSourceError::FenceLost)
        );
        assert!(stale.writes().is_empty());
    }

    #[cfg(feature = "sekai-client")]
    #[test]
    fn apply_payload_maps_onto_the_streaming_rpc() {
        let apply = StreamingBatchApply {
            checkpoint: "orders/0@0".into(),
            action_type_id: "stream.record.create".into(),
            action_type_version: "1".into(),
            edits: vec![ObjectEdit {
                object_id: "rec-0".into(),
                name: "rec-0".into(),
                properties: BTreeMap::from([("n".into(), "0".into())]),
            }],
            input_positions: vec![InputPosition {
                source: "orders/0".into(),
                position: "2".into(),
            }],
        };
        let wire = streaming_transform_apply(apply);
        assert_eq!(wire.checkpoint, "orders/0@0");
        assert_eq!(wire.action_type_id, "stream.record.create");
        assert_eq!(wire.edits[0].object_id, "rec-0");
        assert_eq!(wire.edits[0].properties["n"], "0");
        assert_eq!(wire.input_positions[0].position, "2");
        let receipt = apply_receipt_from_response(
            sekai_client::protocol::sekai::ApplyStreamingTransformBatchResponse {
                receipt: Some(
                    sekai_client::protocol::sekai::StreamingTransformApplyReceipt {
                        checkpoint: "orders/0@0".into(),
                        input_positions: wire_positions(&[InputPosition {
                            source: "orders/0".into(),
                            position: "2".into(),
                        }]),
                        replay: true,
                        ..Default::default()
                    },
                ),
            },
        )
        .unwrap();
        assert!(receipt.replay);
        assert_eq!(receipt.input_positions[0].position, "2");
    }
}
