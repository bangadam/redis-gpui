//! The console pipeline: tokenize → consult the shared brain → outcome.
//!
//! The console **never decides policy itself** — every decision comes from
//! [`write_safety::classify`], the same brain the edit/save paths use
//! (PLAN.md §4). This module is the pure glue:
//!
//! 1. [`ConsoleEngine::submit`] tokenizes the input and runs it through the
//!    classifier against the current [`WriteGate`], producing a
//!    [`SubmitOutcome`] the UI acts on before **any bytes are sent**
//!    (PRD AC8: refusals are proven client-side).
//! 2. [`ConsoleEngine::confirm`] completes the destructive-command path:
//!    the typed phrase is checked via
//!    [`NeedsConfirmation::is_satisfied_by`], and classification is
//!    **re-checked** at confirm time — if the gate changed between the
//!    prompt and the confirm (e.g. flipped back to read-only), the command
//!    does not run.
//! 3. [`ConsoleEngine::reject`] records a declined confirmation; nothing is
//!    sent, ever.

use write_safety::{classify, Decision, NeedsConfirmation, ParsedCommand, WriteGate};

use crate::tokenize::tokenize;

/// What the UI should do after the user submits an input line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SubmitOutcome {
    /// Safe to send to the server (read-only command, or write mode allows
    /// it).
    Execute(ParsedCommand),
    /// Do not send anything; show `reason` to the user.
    Refuse {
        /// The user-facing refusal message (write-safety's wording).
        reason: String,
    },
    /// Destructive in write mode: render the confirmation prompt; on
    /// approval call [`ConsoleEngine::confirm`].
    NeedConfirm {
        /// The parsed command, held so the confirm step does not re-parse.
        parsed: ParsedCommand,
        /// What the UI must show: what is at stake and the phrase to type.
        confirmation: NeedsConfirmation,
    },
    /// The line was empty or whitespace-only: a no-op.
    Nothing,
}

/// Result of attempting to confirm a pending destructive command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConfirmOutcome {
    /// The phrase matched and the gate still allows it: send to the server.
    Execute(ParsedCommand),
    /// The typed phrase did not match; the confirmation stays pending (the
    /// UI may let the user retry typing it).
    WrongPhrase,
    /// The gate changed between prompt and confirm (e.g. back to
    /// read-only): the command is refused with the fresh classification's
    /// message. Nothing is sent.
    GateChanged {
        /// The refusal reason from the *current* gate state.
        reason: String,
    },
}

/// Result of declining a pending confirmation (PRD AC8: declining means
/// nothing is sent).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Declined;

/// The pure console pipeline. One instance can serve a connection tab; it
/// holds no state of its own (the gate and history belong to the caller).
#[derive(Clone, Copy, Debug, Default)]
pub struct ConsoleEngine;

impl ConsoleEngine {
    /// A new engine.
    pub fn new() -> Self {
        Self
    }

    /// Tokenizes `input` and classifies it against `gate`.
    ///
    /// - Empty / whitespace-only input → [`SubmitOutcome::Nothing`].
    /// - Malformed quoting → [`SubmitOutcome::Refuse`] carrying the
    ///   tokenizer's message (still nothing sent — malformed input is a
    ///   client-side refusal, not a server round-trip).
    /// - Otherwise, [`write_safety::classify`] decides:
    ///   `Run` → [`SubmitOutcome::Execute`]; `Blocked(reason)` →
    ///   [`SubmitOutcome::Refuse`] with the blocked message;
    ///   `Confirm(confirmation)` → [`SubmitOutcome::NeedConfirm`].
    pub fn submit(&self, input: &str, gate: &WriteGate) -> SubmitOutcome {
        let parsed = match tokenize(input) {
            Ok(Some(parsed)) => parsed,
            Ok(None) => return SubmitOutcome::Nothing,
            Err(err) => {
                return SubmitOutcome::Refuse {
                    reason: err.to_string(),
                }
            }
        };

        match classify(&parsed, gate) {
            Decision::Run => SubmitOutcome::Execute(parsed),
            Decision::Blocked(reason) => SubmitOutcome::Refuse {
                reason: reason.message().to_string(),
            },
            Decision::Confirm(confirmation) => SubmitOutcome::NeedConfirm {
                parsed,
                confirmation,
            },
        }
    }

    /// Completes a pending confirmation for `parsed`.
    ///
    /// The phrase check is [`NeedsConfirmation::is_satisfied_by`]
    /// (case-insensitive, trimmed). Because the gate may have changed since
    /// the prompt was shown (disconnect resets it to read-only, the user may
    /// have toggled editing), the command is **re-classified against the
    /// current gate**:
    ///
    /// - still `Confirm` and the phrase matches → [`ConfirmOutcome::Execute`];
    /// - still `Confirm` and the phrase does not match →
    ///   [`ConfirmOutcome::WrongPhrase`];
    /// - now `Blocked` (e.g. gate flipped back to read-only) →
    ///   [`ConfirmOutcome::GateChanged`] with the fresh refusal message;
    /// - now `Run` (gate was read-only at submit time but the user enabled
    ///   editing before confirming) → [`ConfirmOutcome::Execute`]; the
    ///   command is safe under the *current* gate, which is the one that
    ///   counts.
    ///
    /// `confirmation` is the value captured at submit time; it is used for
    /// the phrase check. The re-run classification guards the state change,
    /// the phrase check guards intent. Only when **both** pass does the
    /// command execute.
    pub fn confirm(
        &self,
        parsed: &ParsedCommand,
        typed_phrase: &str,
        gate: &WriteGate,
        confirmation: &NeedsConfirmation,
    ) -> ConfirmOutcome {
        match classify(parsed, gate) {
            Decision::Run => ConfirmOutcome::Execute(parsed.clone()),
            Decision::Blocked(reason) => ConfirmOutcome::GateChanged {
                reason: reason.message().to_string(),
            },
            Decision::Confirm(_current) => {
                if confirmation.is_satisfied_by(typed_phrase) {
                    ConfirmOutcome::Execute(parsed.clone())
                } else {
                    ConfirmOutcome::WrongPhrase
                }
            }
        }
    }

    /// The user declined a pending confirmation: nothing is sent. The UI
    /// logs the decline with the returned marker (which serializes nothing
    /// sensitive — PRD story 39 keeps command results out of logs).
    pub fn reject(&self, _parsed: &ParsedCommand) -> Declined {
        Declined
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use write_safety::GateEvent;

    fn ro() -> WriteGate {
        WriteGate::new()
    }

    fn we() -> WriteGate {
        let mut gate = WriteGate::new();
        gate.apply(GateEvent::EnableEditing).unwrap();
        gate
    }

    #[test]
    fn empty_and_whitespace_is_nothing() {
        let engine = ConsoleEngine::new();
        assert_eq!(engine.submit("", &ro()), SubmitOutcome::Nothing);
        assert_eq!(engine.submit("   ", &ro()), SubmitOutcome::Nothing);
        assert_eq!(engine.submit("\t", &we()), SubmitOutcome::Nothing);
    }

    // ---- Read-only gate (PRD story 37 / AC8) ----

    #[test]
    fn read_only_gate_runs_reads() {
        let engine = ConsoleEngine::new();
        let SubmitOutcome::Execute(cmd) = engine.submit("GET foo", &ro()) else {
            panic!("GET must execute in read-only mode");
        };
        assert_eq!(cmd.name, "GET");
        assert_eq!(cmd.args, vec![b"foo".to_vec()]);
    }

    #[test]
    fn read_only_gate_refuses_writes() {
        let engine = ConsoleEngine::new();
        let SubmitOutcome::Refuse { reason } = engine.submit("SET foo bar", &ro()) else {
            panic!("SET must be refused in read-only mode");
        };
        assert!(
            reason.contains("read-only") && reason.contains("enable editing"),
            "write-blocked wording: {reason}"
        );
    }

    #[test]
    fn read_only_gate_refuses_unknown_commands() {
        let engine = ConsoleEngine::new();
        let SubmitOutcome::Refuse { reason } = engine.submit("WIBBLE x", &ro()) else {
            panic!("unknown command must be refused in read-only mode");
        };
        assert!(reason.contains("not recognized"), "wording: {reason}");
    }

    #[test]
    fn read_only_gate_refuses_scripts() {
        let engine = ConsoleEngine::new();
        for input in [
            "EVAL \"return 1\" 0",
            "EVAL_RO \"return 1\" 0",
            "SCRIPT LOAD x",
        ] {
            let SubmitOutcome::Refuse { reason } = engine.submit(input, &ro()) else {
                panic!("{input} must be refused in read-only mode");
            };
            assert!(reason.contains("server-side script"), "{input}: {reason}");
        }
    }

    // ---- Write-enabled gate (PRD story 38 / AC8) ----

    #[test]
    fn write_gate_executes_plain_writes() {
        let engine = ConsoleEngine::new();
        let SubmitOutcome::Execute(cmd) = engine.submit("SET foo bar", &we()) else {
            panic!("SET must execute in write mode");
        };
        assert_eq!(cmd.name, "SET");
    }

    #[test]
    fn write_gate_needs_confirmation_for_flushdb() {
        let engine = ConsoleEngine::new();
        let SubmitOutcome::NeedConfirm { confirmation, .. } = engine.submit("FLUSHDB", &we())
        else {
            panic!("FLUSHDB must need confirmation in write mode");
        };
        assert_eq!(confirmation.action, "FLUSHDB");
        assert!(
            confirmation.scope_description.contains("current database"),
            "scope text: {}",
            confirmation.scope_description
        );
        assert_eq!(confirmation.confirm_phrase, "FLUSHDB");
    }

    #[test]
    fn confirm_with_wrong_phrase_is_wrong_phrase() {
        let engine = ConsoleEngine::new();
        let SubmitOutcome::NeedConfirm {
            parsed,
            confirmation,
        } = engine.submit("FLUSHDB", &we())
        else {
            panic!();
        };
        assert_eq!(
            engine.confirm(&parsed, "yes please", &we(), &confirmation),
            ConfirmOutcome::WrongPhrase
        );
        assert_eq!(
            engine.confirm(&parsed, "FLUSHALL", &we(), &confirmation),
            ConfirmOutcome::WrongPhrase
        );
    }

    #[test]
    fn confirm_with_right_phrase_executes() {
        let engine = ConsoleEngine::new();
        let SubmitOutcome::NeedConfirm {
            parsed,
            confirmation,
        } = engine.submit("FLUSHDB", &we())
        else {
            panic!();
        };
        // Case-insensitive + trimmed phrase.
        assert_eq!(
            engine.confirm(&parsed, "  flushdb  ", &we(), &confirmation),
            ConfirmOutcome::Execute(parsed.clone())
        );
    }

    #[test]
    fn confirm_after_gate_flipped_back_to_read_only_is_gate_changed() {
        let engine = ConsoleEngine::new();
        let SubmitOutcome::NeedConfirm {
            parsed,
            confirmation,
        } = engine.submit("FLUSHDB", &we())
        else {
            panic!();
        };
        let mut gate = we();
        gate.apply(GateEvent::DisableEditing).unwrap();
        let ConfirmOutcome::GateChanged { reason } =
            engine.confirm(&parsed, "FLUSHDB", &gate, &confirmation)
        else {
            panic!("confirm under a read-only gate must be GateChanged");
        };
        assert!(
            reason.contains("read-only") && reason.contains("enable editing"),
            "gate-changed refusal carries the read-only wording: {reason}"
        );
    }

    #[test]
    fn confirm_after_disconnect_reset_is_gate_changed() {
        let engine = ConsoleEngine::new();
        let SubmitOutcome::NeedConfirm {
            parsed,
            confirmation,
        } = engine.submit("FLUSHDB", &we())
        else {
            panic!();
        };
        let mut gate = we();
        gate.apply(GateEvent::Disconnected).unwrap();
        assert!(matches!(
            engine.confirm(&parsed, "FLUSHDB", &gate, &confirmation),
            ConfirmOutcome::GateChanged { .. }
        ));
    }

    #[test]
    fn confirm_after_enabling_between_prompt_and_confirm_executes() {
        // Submitted under read-only (blocked), user enables editing, then
        // confirms: the current gate allows it, so it executes. The phrase
        // is irrelevant when classification says Run (this path arises from
        // a Refuse, which carries no confirmation; the UI never reaches
        // confirm() from a Refuse, but the engine stays total anyway).
        let engine = ConsoleEngine::new();
        assert!(matches!(
            engine.submit("SET foo bar", &ro()),
            SubmitOutcome::Refuse { .. }
        ));
        let parsed = ParsedCommand::from_strs("SET", &["foo", "bar"]);
        let confirmation = NeedsConfirmation::for_action("SET", "irrelevant");
        assert_eq!(
            engine.confirm(&parsed, "whatever", &we(), &confirmation),
            ConfirmOutcome::Execute(parsed)
        );
    }

    #[test]
    fn reject_is_a_pure_no_op() {
        let engine = ConsoleEngine::new();
        let parsed = ParsedCommand::from_strs("FLUSHDB", &[]);
        assert_eq!(engine.reject(&parsed), Declined);
    }

    // ---- Malformed input refused client-side ----

    #[test]
    fn malformed_quote_is_refused_with_the_tokenizer_message() {
        let engine = ConsoleEngine::new();
        let SubmitOutcome::Refuse { reason } = engine.submit(r#"SET k "oops"#, &we()) else {
            panic!("malformed input must be refused, not executed");
        };
        assert!(reason.contains("quote"), "wording: {reason}");
    }

    #[test]
    fn malformed_escape_is_refused() {
        let engine = ConsoleEngine::new();
        let SubmitOutcome::Refuse { reason } = engine.submit("SET k a\\", &we()) else {
            panic!();
        };
        assert!(reason.contains("backslash"), "wording: {reason}");
    }
}
