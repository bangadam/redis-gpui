//! The console pipeline: input → decision → output.
//!
//! Console gating is a pure pipeline (PLAN.md section 3): command parsing,
//! policy consultation (via `write-safety`), in-memory history, and the
//! result model. Each entered command resolves to `Run` / `Block` /
//! `NeedsConfirmation` before anything is sent to the server — refusals are
//! proven client-side.
//!
//! Modules:
//! - [`tokenize`] — input line → [`write_safety::ParsedCommand`] (redis-cli
//!   style quoting, subcommand merge).
//! - [`engine`] — [`ConsoleEngine::submit`] / [`confirm`] / [`reject`]: the
//!   decision pipeline around `write_safety::classify` (the one shared
//!   brain — the console never decides policy itself).
//! - [`history`] — in-memory, never-persisted command history with
//!   up/down navigation (PRD story 39).
//! - [`render`] — server replies and tri-state write outcomes → console
//!   display lines.
//!
//! GPUI-free, tokio-free, I/O-free: everything here is exhaustively
//! unit-testable pure logic. The `redis-io` dependency is **type-only**
//! ([`redis_io::connection::Reply`] and [`redis_io::outcome::WriteOutcome`])
//! so the display model cannot drift from the adapter's actual shapes —
//! see `render`'s module docs for the decision.
pub mod engine;
pub mod history;
pub mod render;
pub mod tokenize;

pub use engine::{ConfirmOutcome, ConsoleEngine, Declined, SubmitOutcome};
pub use history::{History, HistoryNav, MAX_ENTRIES};
pub use render::{
    format_write_outcome, render_info, render_reply, ConsoleOutput, OutputKind, MAX_LINES,
};
pub use tokenize::{tokenize, TokenizeError};
