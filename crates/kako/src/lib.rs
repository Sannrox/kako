//! kako (加工): a sandboxed data-transform runner.
//!
//! kako takes a transform job (SQL or Python plus input datasets) from a
//! pluggable [`JobSource`], runs it in a sandboxed child process, and hands the
//! outputs and a [`Receipt`] back to the source.

pub use kako_core::digest;
pub use kako_core::fair;
pub use kako_core::isolation;
pub use kako_core::manifest;
pub use kako_core::receipt;
pub use kako_core::stage;
pub use kako_core::{
    ColumnMarking, DeclassifyDecision, Digest, Engine, Failure, Input, IsolationTier, Job,
    JobKeyClaim, JobSource, Lineage, LineageRead, LineageWrite, Output, QuotaKey, Receipt,
    WriteMode,
};
pub mod crypto;
pub use kako_sandbox as sandbox;

pub mod engine;
pub mod local;
pub mod pool;
pub mod run;

#[cfg(feature = "sekai")]
pub use kako_source_sekai as sekai_source;

/// When `KAKO_REQUIRE_PYTHON=1`, Python engine tests must run instead of skip.
pub fn python_tests_required() -> bool {
    kako_engine_python::python_tests_required()
}
