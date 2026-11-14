//! Tri-state write outcome: the PRD's acceptance-3 backbone.
//!
//! A **write** whose acknowledgment arrived is [`WriteOutcome::Acknowledged`].
//! A write the server definitively rejected is [`WriteOutcome::Failed`].
//! A write whose fate we do not know — typically because the connection
//! dropped before the reply arrived — is [`WriteOutcome::OutcomeUnknown`].
//!
//! `OutcomeUnknown` is terminal for the automatic machinery: it is **never
//! auto-retried** (PRD story 40 / acceptance 3). The user may re-issue the
//! command deliberately after checking; the app surfaces a dedicated
//! "check before retrying" state and refuses to pretend a later read
//! always proves the outcome.

use std::fmt;

use crate::connection::Command;

/// A definitive error: the server (or the client, pre-send) rejected the
/// command. Retrying *may* be legitimate, but only as an explicit user
/// action — the error is honest about what happened.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DefinitiveError {
    /// Short, stable classification usable by UI/log layers.
    pub kind: DefinitiveErrorKind,
    /// Human-readable, non-sensitive description (no key bytes, no secrets —
    /// see the log-hygiene rules in [`crate::log_hygiene`]).
    pub message: String,
}

/// Classification of a definitive failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefinitiveErrorKind {
    /// The server replied with an error (e.g. `WRONGTYPE`, `ERR`, `NOPERM`).
    ServerError,
    /// The command was recognized as invalid before any bytes were sent.
    InvalidCommand,
    /// Authentication/ACL failure.
    Auth,
    /// The connection was already down before the command was sent.
    NotConnected,
    /// Anything else the caller classifies as terminal.
    Other,
}

impl DefinitiveError {
    /// Construct a [`DefinitiveError`] from a kind and message.
    pub fn new(kind: DefinitiveErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for DefinitiveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.kind, self.message)
    }
}

impl std::error::Error for DefinitiveError {}

/// Context recorded when a write's outcome became unknown.
///
/// Deliberately contains **no key or value bytes** — a copy of the in-flight
/// command is kept (as [`Command`], not a rendered string) only so the UI can
/// show *what kind* of command is in limbo and re-issue it on explicit user
/// request. [`Command`]'s own `Debug` is byte-safe; never log its contents.
#[derive(Debug, Clone)]
pub struct DisconnectContext {
    /// The command whose reply never arrived (kept verbatim, never auto-sent).
    pub in_flight: Command,
    /// Non-sensitive hint about why the link went down, if known.
    pub reason: Option<String>,
}

/// Tri-state result of issuing a write command.
///
/// Invariants (enforced by convention + tests, and by the connection layer):
///
/// - `Acknowledged` — the server replied. For writes that can fail
///   semantically, the reply itself (e.g. `OK`, or a server error) decides
///   between `Acknowledged` and `Failed`.
/// - `Failed` — definitive rejection. Nothing about the write's effect is
///   uncertain (it either never executed or the server told us it failed).
/// - `OutcomeUnknown` — the ack never arrived. **Never auto-retried.**
#[derive(Debug, Clone)]
pub enum WriteOutcome {
    /// The server acknowledged the write.
    Acknowledged,
    /// The write definitively failed; the error is safe to show and log.
    Failed(DefinitiveError),
    /// The write's fate is unknown (lost ack around a disconnect).
    /// Render the dedicated "check before retrying" state; never resend.
    OutcomeUnknown(DisconnectContext),
}

impl WriteOutcome {
    /// Map a [`crate::connection::ConnError`] from a `Connection` into a
    /// write outcome. This is the call-site mapping every write path must
    /// use — it is the single place that turns `Disconnected` into
    /// [`WriteOutcome::OutcomeUnknown`].
    pub fn from_conn_error(cmd: Command, err: crate::connection::ConnError) -> WriteOutcome {
        match err {
            crate::connection::ConnError::Definitive(e) => WriteOutcome::Failed(e),
            crate::connection::ConnError::Disconnected { reason, in_flight } => {
                // The connection layer already recorded the command; prefer
                // its record (it is authoritative for *what was sent*), but
                // keep the caller's copy if the layer had none.
                let in_flight = in_flight.unwrap_or(cmd);
                WriteOutcome::OutcomeUnknown(DisconnectContext { in_flight, reason })
            }
        }
    }

    /// `true` only for [`WriteOutcome::Acknowledged`].
    pub fn is_acknowledged(&self) -> bool {
        matches!(self, WriteOutcome::Acknowledged)
    }

    /// `true` only for [`WriteOutcome::OutcomeUnknown`] — the state that
    /// must gate any "retry?" UI and must never trigger an automatic resend.
    pub fn is_outcome_unknown(&self) -> bool {
        matches!(self, WriteOutcome::OutcomeUnknown(_))
    }
}

impl From<Result<(), DefinitiveError>> for WriteOutcome {
    fn from(res: Result<(), DefinitiveError>) -> Self {
        match res {
            Ok(()) => WriteOutcome::Acknowledged,
            Err(e) => WriteOutcome::Failed(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::connection::ConnError;
    use crate::outcome::DefinitiveErrorKind;

    fn cmd_set(v: u8) -> Command {
        Command::new("SET", vec![b"key".to_vec(), vec![v]])
    }

    #[test]
    fn acknowledged_and_failed() {
        assert!(WriteOutcome::Acknowledged.is_acknowledged());
        assert!(!WriteOutcome::Acknowledged.is_outcome_unknown());

        let failed = WriteOutcome::Failed(DefinitiveError::new(
            DefinitiveErrorKind::ServerError,
            "WRONGTYPE",
        ));
        assert!(!failed.is_acknowledged());
        assert!(!failed.is_outcome_unknown());
        assert!(format!("{failed:?}").contains("WRONGTYPE"));
    }

    #[test]
    fn disconnect_maps_to_outcome_unknown() {
        let cmd = cmd_set(1);
        let out = WriteOutcome::from_conn_error(
            cmd.clone(),
            ConnError::Disconnected {
                reason: Some("server closed link".into()),
                in_flight: Some(cmd.clone()),
            },
        );
        assert!(out.is_outcome_unknown());
        match out {
            WriteOutcome::OutcomeUnknown(ctx) => {
                assert_eq!(ctx.in_flight, cmd);
                assert_eq!(ctx.reason.as_deref(), Some("server closed link"));
            }
            _ => panic!("expected OutcomeUnknown"),
        }
    }

    #[test]
    fn disconnect_without_layer_recorded_command_falls_back_to_caller_copy() {
        let cmd = cmd_set(2);
        let out = WriteOutcome::from_conn_error(
            cmd.clone(),
            ConnError::Disconnected {
                reason: None,
                in_flight: None,
            },
        );
        match out {
            WriteOutcome::OutcomeUnknown(ctx) => assert_eq!(ctx.in_flight, cmd),
            _ => panic!("expected OutcomeUnknown"),
        }
    }

    #[test]
    fn definitive_error_maps_to_failed() {
        let out = WriteOutcome::from_conn_error(
            cmd_set(3),
            ConnError::Definitive(DefinitiveError::new(
                DefinitiveErrorKind::ServerError,
                "ERR unknown command",
            )),
        );
        match out {
            WriteOutcome::Failed(e) => {
                assert_eq!(e.kind, DefinitiveErrorKind::ServerError)
            }
            _ => panic!("expected Failed"),
        }
    }
}
