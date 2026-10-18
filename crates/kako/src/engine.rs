//! Worker manifest helpers used by the parent run loop and the child dispatcher.

pub use kako_core::manifest::{
    Manifest, StagedInput, write_manifest, write_manifest_from_job, write_manifest_with,
};
pub use kako_worker::execute_in_scratch;
