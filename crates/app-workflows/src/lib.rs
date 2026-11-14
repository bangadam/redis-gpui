//! TEST-ONLY crate: workflow boundary tests, fixture harness, fault seams.
//!
//! This crate is **not** a runtime dependency of the app. It holds the
//! acceptance-criteria workflow tests (PLAN.md §7) driven headlessly
//! (no GPUI, no window, no real server) against a scriptable mock of the
//! `redis_io::Connection` trait — the same trait the app's session entity
//! uses.
//!
//! # The mirror approach (honesty note)
//!
//! `src/session.rs` (the app crate) holds a **concrete** `RedisConnection`
//! inside a GPUI entity, so it cannot be driven headlessly without a
//! window. This crate therefore re-implements the *save discipline* of
//! the session — the same ordering of the same building blocks — as
//! [`SaveWorkflow`]. Every step cites the `session.rs` lines it mirrors
//! (see the docs on [`SaveWorkflow::save`] and its helpers). The
//! building blocks themselves are the REAL production crates
//! (`write-safety`, `value-model`, `redis-io`); only the GPUI glue
//! (entities, `run_io`, foreground executors) is mirrored.
//!
//! What the Phase 5 editing UI should do is exactly what [`SaveWorkflow`]
//! does, in the same order, with the same refusal wording and the same
//! post-state updates.

pub mod mock;
pub mod save_workflow;
#[cfg(any(test, feature = "scale"))]
pub mod scale_measure;

pub use mock::ScriptedConnection;
pub use save_workflow::{SaveFlow, SaveFlowError, SaveRefusal, SaveWorkflow};
