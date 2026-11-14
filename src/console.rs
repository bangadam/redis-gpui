//! The per-connection console state (PLAN.md Phase 4, PRD stories 36–40).
//!
//! This module is the *entity-owned* half of the console: the output log,
//! the pending destructive confirmation, the in-memory history, and the
//! running-command guard. All decision logic lives in
//! `crates/console-engine` (tokenize / classify / confirm); all flows are
//! driven through [`crate::session::ConnectionSession`] so the gate, the
//! connection, and the serial guard stay coherent — the view never runs a
//! command.
//!
//! # Output log discipline (PRD story 39: privacy)
//!
//! The log holds only what the user already saw: input echoes and the
//! rendered [`ConsoleOutput`] lines from `console_engine::render` (which
//! guarantees no raw bulk bytes — hex previews for non-UTF-8). It lives in
//! the session entity (memory only, dropped with the tab) and is capped at
//! [`MAX_LOG_ENTRIES`] with oldest-first eviction, so a long session
//! cannot grow it without bound.
//!
//! # One command at a time
//!
//! Connections are serial by design (see `session.rs` module docs), and the
//! console makes that visible: a submission while a command is in flight
//! is refused with "a command is already running" (an Info line, nothing
//! sent). A `Stop` (or Esc) cancels the *wait* only — the serial guard
//! makes the late result a no-op and the honest wording notes the command
//! may still have reached the server.

use console_engine::{
    ConfirmOutcome, ConsoleEngine, ConsoleOutput, History, HistoryNav, SubmitOutcome,
};
use write_safety::{NeedsConfirmation, ParsedCommand, WriteGate};

/// Maximum retained log entries; oldest are evicted on overflow.
pub const MAX_LOG_ENTRIES: usize = 1000;

/// One rendered log line: its style kind and text.
///
/// Flat by design — the panel renders each entry as a styled line and the
/// truncation count is baked into the `ConsoleOutput` lines at render time
/// (in `console-engine`), so the log stores ready-to-display text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogEntry {
    /// The style kind (Success / Error / Info) for coloring.
    pub kind: console_engine::OutputKind,
    /// The display text.
    pub text: String,
}

/// The pending destructive confirmation (PRD story 38, AC8).
///
/// Holds everything the confirm step needs: the parsed command, the
/// confirmation captured at submit time (scope + phrase — the engine
/// re-checks the gate against the *current* gate at confirm time), and the
/// phrase typed so far in the modal's input field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingConfirm {
    /// The parsed command awaiting approval.
    pub parsed: ParsedCommand,
    /// The confirmation captured at submit time (what the modal shows).
    pub confirmation: NeedsConfirmation,
    /// What the user has typed into the confirm field so far.
    pub typed: String,
}

impl PendingConfirm {
    /// The modal title (the command name, e.g. `FLUSHDB`).
    pub fn title(&self) -> &str {
        &self.confirmation.action
    }

    /// The modal body (what is at stake, from write-safety's scope).
    pub fn body(&self) -> &str {
        &self.confirmation.scope_description
    }

    /// The input placeholder, e.g. `type FLUSHDB to confirm`.
    pub fn placeholder(&self) -> String {
        format!("type {} to confirm", self.confirmation.confirm_phrase)
    }

    /// True when the typed phrase satisfies the confirmation
    /// (case-insensitive; write-safety's check).
    pub fn phrase_matches(&self) -> bool {
        self.confirmation.is_satisfied_by(&self.typed)
    }
}

/// The console state owned by the session entity.
///
/// The session drives every transition (see `session.rs`'s console
/// methods); this struct is the data + pure helpers so the whole thing is
/// unit-testable without GPUI.
#[derive(Debug, Default)]
pub struct ConsoleState {
    /// The rendered output log (input echoes + results), capped.
    log: Vec<LogEntry>,
    /// In-memory history (never persisted; PRD story 39).
    history: History,
    /// The active history navigation, if up/down was pressed since the
    /// last submit (re-created per keystroke-free state; `None` = at the
    /// prompt).
    nav: Option<HistoryNav>,
    /// The draft the user was typing before up-arrow recall began (restored
    /// by down-arrow past the newest entry).
    draft: String,
    /// The pending destructive confirmation, if any.
    pending: Option<PendingConfirm>,
}

impl ConsoleState {
    /// The full log, oldest first.
    pub fn log(&self) -> &[LogEntry] {
        &self.log
    }

    /// The pending confirmation, if any.
    pub fn pending(&self) -> Option<&PendingConfirm> {
        self.pending.as_ref()
    }

    /// Append one entry, evicting the oldest past the cap.
    fn push_log(&mut self, kind: console_engine::OutputKind, text: impl Into<String>) {
        if self.log.len() >= MAX_LOG_ENTRIES {
            self.log.remove(0);
        }
        self.log.push(LogEntry {
            kind,
            text: text.into(),
        });
    }

    /// Echo the submitted input line (the `› input` line, Info kind).
    pub fn echo_input(&mut self, line: &str) {
        self.push_log(console_engine::OutputKind::Info, format!("› {line}"));
    }

    /// Append a rendered [`ConsoleOutput`] (result / refusal / notice).
    pub fn push_output(&mut self, out: &ConsoleOutput) {
        for line in &out.lines {
            self.push_log(out.kind, line.clone());
        }
    }

    /// Append a refusal message as an Info output (write-safety's honest
    /// wording, verbatim).
    pub fn push_refusal(&mut self, reason: &str) {
        self.push_output(&console_engine::render_info(reason));
    }

    /// Record a submitted line into history and reset navigation.
    pub fn record_history(&mut self, line: &str) {
        self.history.push(line);
        self.nav = None;
        self.draft.clear();
    }

    /// Up-arrow: recall the next older history entry. Returns the text to
    /// place in the input field, or `None` when already at the oldest
    /// (the caller keeps the current field text).
    pub fn history_up(&mut self, current_field: &str) -> Option<String> {
        if self.nav.is_none() {
            // First recall: remember the draft so down-arrow past the end
            // restores it, and start from the newest entry.
            self.draft = current_field.to_string();
            self.nav = Some(self.history.nav());
        }
        let nav = self.nav.as_mut()?;
        let ix = nav.older()?;
        self.history.entries().get(ix).cloned()
    }

    /// Down-arrow: walk toward the newer entries. Returns the text to
    /// place in the input field; `None` past the newest entry restores the
    /// saved draft (back at the prompt).
    pub fn history_down(&mut self) -> Option<String> {
        let nav = self.nav.as_mut()?;
        match nav.newer() {
            Some(ix) => self.history.entries().get(ix).cloned(),
            None => {
                // Back at the empty prompt: restore the draft, drop the nav.
                let draft = self.draft.clone();
                self.nav = None;
                Some(draft)
            }
        }
    }

    /// A pending confirmation was opened (from `SubmitOutcome::NeedConfirm`).
    pub fn open_confirm(&mut self, parsed: ParsedCommand, confirmation: NeedsConfirmation) {
        self.pending = Some(PendingConfirm {
            parsed,
            confirmation,
            typed: String::new(),
        });
    }

    /// Update the typed phrase in the pending confirmation.
    /// Returns false when there is no pending confirmation.
    pub fn set_typed(&mut self, typed: String) -> bool {
        match &mut self.pending {
            Some(p) => {
                p.typed = typed;
                true
            }
            None => false,
        }
    }

    /// Take the pending confirmation (closing the modal), returning it.
    pub fn take_pending(&mut self) -> Option<PendingConfirm> {
        self.pending.take()
    }

    /// Cancel/decline the pending confirmation: nothing is sent. Echoes
    /// the honest decline line.
    pub fn decline_confirm(&mut self) {
        if let Some(p) = self.pending.take() {
            self.push_log(
                console_engine::OutputKind::Info,
                format!("{} — declined, nothing was sent", p.confirmation.action),
            );
        }
    }
}

// ---- The pure decision helpers the session calls ----------------------
//
// These wrap `ConsoleEngine` so `session.rs` reads as flow, not plumbing.

/// Classify a submitted line against the gate snapshot.
pub fn submit_line(line: &str, gate: &WriteGate) -> SubmitOutcome {
    ConsoleEngine.submit(line, gate)
}

/// Run the confirm step against the current gate (the engine re-classifies;
/// `GateChanged` is the "gate moved between prompt and confirm" path).
pub fn confirm_pending(pending: &PendingConfirm, gate: &WriteGate) -> ConfirmOutcome {
    ConsoleEngine.confirm(&pending.parsed, &pending.typed, gate, &pending.confirmation)
}

#[cfg(test)]
mod tests {
    use super::*;
    use console_engine::OutputKind;
    use write_safety::GateEvent;

    fn gate_write() -> WriteGate {
        let mut g = WriteGate::new();
        g.apply(GateEvent::EnableEditing).unwrap();
        g
    }

    fn out(kind: OutputKind, lines: &[&str]) -> ConsoleOutput {
        ConsoleOutput {
            kind,
            lines: lines.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn log_cap_evicts_oldest() {
        let mut c = ConsoleState::default();
        for i in 0..(MAX_LOG_ENTRIES + 25) {
            c.push_log(OutputKind::Info, format!("line {i}"));
        }
        assert_eq!(c.log().len(), MAX_LOG_ENTRIES);
        assert_eq!(c.log().first().unwrap().text, "line 25");
        assert_eq!(
            c.log().last().unwrap().text,
            format!("line {}", MAX_LOG_ENTRIES + 24)
        );
    }

    #[test]
    fn echo_and_outputs_land_in_order() {
        let mut c = ConsoleState::default();
        c.echo_input("GET foo");
        c.push_output(&out(OutputKind::Success, &["bar"]));
        assert_eq!(
            c.log(),
            &[
                LogEntry {
                    kind: OutputKind::Info,
                    text: "› GET foo".into()
                },
                LogEntry {
                    kind: OutputKind::Success,
                    text: "bar".into()
                },
            ]
        );
    }

    #[test]
    fn history_up_down_round_trip_restores_draft() {
        let mut c = ConsoleState::default();
        c.record_history("one");
        c.record_history("two");
        // User typed a draft, then pressed up twice.
        assert_eq!(c.history_up("dra"), Some("two".into()));
        assert_eq!(c.history_up("two"), Some("one".into()));
        assert_eq!(c.history_up("one"), None, "already at oldest");
        // Down past the newest restores the draft and clears the nav.
        assert_eq!(c.history_down(), Some("two".into()));
        assert_eq!(c.history_down(), Some("dra".into()));
        assert_eq!(c.history_down(), None, "at the prompt, nothing left");
    }

    #[test]
    fn history_records_and_resets_nav() {
        let mut c = ConsoleState::default();
        c.record_history("GET a");
        assert_eq!(c.history_up(""), Some("GET a".into()));
        c.record_history("GET b"); // submit resets nav
        assert_eq!(
            c.history_up("x"),
            Some("GET b".into()),
            "nav restarted from newest"
        );
    }

    #[test]
    fn consecutive_duplicate_history_collapses() {
        // The engine's History does this; the console just forwards.
        let mut c = ConsoleState::default();
        c.record_history("GET a");
        c.record_history("GET a");
        assert_eq!(c.history_up(""), Some("GET a".into()));
        assert_eq!(c.history_up("GET a"), None, "only one entry");
    }

    #[test]
    fn pending_confirm_lifecycle() {
        let mut c = ConsoleState::default();
        // Open via a real NeedConfirm (FLUSHDB under a write gate).
        if let SubmitOutcome::NeedConfirm {
            parsed,
            confirmation,
        } = submit_line("FLUSHDB", &gate_write())
        {
            c.open_confirm(parsed, confirmation);
        } else {
            panic!("FLUSHDB under write gate must need confirmation");
        }
        let p = c.pending().unwrap();
        assert_eq!(p.title(), "FLUSHDB");
        assert_eq!(p.body(), "all keys in the current database");
        assert!(p.placeholder().contains("FLUSHDB"));
        assert!(!p.phrase_matches());
        // Wrong phrase.
        c.set_typed("flushall".into());
        assert!(!c.pending().unwrap().phrase_matches());
        // Right phrase, case-insensitive.
        c.set_typed("  flushdb ".into());
        assert!(c.pending().unwrap().phrase_matches());
        // Decline: log gets the honest line, pending cleared.
        c.decline_confirm();
        assert!(c.pending().is_none());
        assert_eq!(
            c.log().last().unwrap(),
            &LogEntry {
                kind: OutputKind::Info,
                text: "FLUSHDB — declined, nothing was sent".into()
            }
        );
    }

    #[test]
    fn confirm_right_phrase_executes_under_write_gate() {
        let mut c = ConsoleState::default();
        if let SubmitOutcome::NeedConfirm {
            parsed,
            confirmation,
        } = submit_line("FLUSHDB", &gate_write())
        {
            c.open_confirm(parsed, confirmation);
        }
        c.set_typed("flushdb".into());
        let pending = c.pending().unwrap().clone();
        match confirm_pending(&pending, &gate_write()) {
            ConfirmOutcome::Execute(cmd) => assert_eq!(cmd.name, "FLUSHDB"),
            other => panic!("expected Execute, got {other:?}"),
        }
    }

    #[test]
    fn confirm_after_gate_reset_is_gate_changed() {
        // The disconnect reset flips the gate to read-only between prompt
        // and confirm: the command must be refused, never silently run.
        let mut c = ConsoleState::default();
        if let SubmitOutcome::NeedConfirm {
            parsed,
            confirmation,
        } = submit_line("FLUSHDB", &gate_write())
        {
            c.open_confirm(parsed, confirmation);
        }
        c.set_typed("FLUSHDB".into());
        let mut gate = gate_write();
        gate.apply(GateEvent::Disconnected).unwrap();
        let pending = c.pending().unwrap().clone();
        match confirm_pending(&pending, &gate) {
            ConfirmOutcome::GateChanged { reason } => {
                assert!(reason.contains("read-only"), "refusal wording: {reason}");
            }
            other => panic!("expected GateChanged, got {other:?}"),
        }
    }

    #[test]
    fn confirm_with_wrong_phrase_stays_pending() {
        let mut c = ConsoleState::default();
        if let SubmitOutcome::NeedConfirm {
            parsed,
            confirmation,
        } = submit_line("FLUSHDB", &gate_write())
        {
            c.open_confirm(parsed, confirmation);
        }
        c.set_typed("nope".into());
        let pending = c.pending().unwrap().clone();
        assert_eq!(
            confirm_pending(&pending, &gate_write()),
            ConfirmOutcome::WrongPhrase
        );
    }

    #[test]
    fn read_only_submissions_are_refused_client_side() {
        let gate = WriteGate::new(); // read-only by default
        match submit_line("SET k v", &gate) {
            SubmitOutcome::Refuse { reason } => {
                assert!(reason.contains("read-only"));
            }
            other => panic!("expected Refuse, got {other:?}"),
        }
        match submit_line("EVAL \"return 1\" 0", &gate) {
            SubmitOutcome::Refuse { reason } => {
                assert!(reason.contains("script"));
            }
            other => panic!("expected Refuse, got {other:?}"),
        }
        match submit_line("WIBBLE 1", &gate) {
            SubmitOutcome::Refuse { reason } => {
                assert!(reason.contains("not recognized"));
            }
            other => panic!("expected Refuse, got {other:?}"),
        }
        // Reads run.
        assert!(matches!(
            submit_line("GET k", &gate),
            SubmitOutcome::Execute(_)
        ));
        // Empty line: a no-op.
        assert!(matches!(submit_line("   ", &gate), SubmitOutcome::Nothing));
    }
}
