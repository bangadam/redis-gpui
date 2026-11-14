//! Key browsing at scale, with no window required.
//!
//! This crate makes the million-key problem tractable and testable
//! (PLAN.md section 3): SCAN batching and pagination, progressive and
//! cancellable search, tree/flat key models, and DB switching.
//!
//! Cancellation uses a token checked between SCAN batches plus a generation
//! counter, so stale batches cannot arrive after cancel (acceptance 2).
//!
//! # Phase 2 status
//!
//! Shipped: [`scan`] (batched SCAN pagination + honest counts),
//! [`flat`] (flat key-list projection), [`cancellation`] (token +
//! generation guard). The engine is runtime-agnostic — plain `async fn`s
//! over `&mut dyn Connection`, driven by any executor (GPUI's background
//! executor in the app; tokio in tests).
//!
//! # Phase 3 status
//!
//! Shipped: [`tree`] (colon-hierarchy view with depth cap + lazy
//! expansion), [`search`] (progressive search state machine + MATCH
//! semantics), [`db`] (database count model + `fetch_db_count`),
//! [`type_filter`] (client-side TYPE filter + `SCAN ... TYPE`
//! passthrough via [`scan::KeyScan::with_type`]). Remaining stub:
//! [`metrics`] (server info/periodic monitoring, Phase 4+).
//!
//! All Phase 2 signatures are unchanged; Phase 3 additions are purely
//! additive.

/// SCAN-based batching and pagination of key enumeration.
pub mod scan;

/// Flat key-list projection over scan pages.
pub mod flat;

/// Cancellation token + generation guard (acceptance 2 backbone).
pub mod cancellation;

/// Progressive, cancellable key search: user text → MATCH glob, fresh
/// generation + token per query, honest counts while searching.
pub mod search;

/// Tree (colon-hierarchical) key model with lazy expansion and a
/// rendering depth cap.
pub mod tree;

/// Redis logical DB switching: the count model (`CONFIG GET databases`)
/// and the documented app-side SELECT restart contract.
pub mod db;

/// Key TYPE filtering: client-side fallback matcher + server-side
/// `SCAN ... TYPE` passthrough argument.
pub mod type_filter;

/// Counts and honest totals ("scanning… N so far") for the browse UI.
/// The honest-count model itself lives in [`scan::KeyCount`].
pub mod metrics;
