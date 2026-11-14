//! The per-connection read-only gate — the state machine that makes
//! "editing is a deliberate, temporary state" structural (PRD stories 26–27).
//!
//! ```text
//!                 EnableEditing (explicit user action)
//!   ReadOnly ─────────────────────────────────────────▶ WriteEnabled
//!       ◀─ ─ ─ ─ ─ DisableEditing / user declines ─ ─ ─ ─
//!       │                                                 │
//!       └────────── Disconnected / AppRestart ────────────┘
//!                     (from BOTH states)
//! ```
//!
//! Invariants (exhaustively tested):
//! - The only path into [`GateState::WriteEnabled`] is an explicit
//!   [`GateEvent::EnableEditing`] from [`GateState::ReadOnly`].
//! - [`GateEvent::Disconnected`] and [`GateEvent::AppRestart`] force
//!   [`GateState::ReadOnly`] from **any** state, so an accidental restart can
//!   never leave write mode silently armed (PRD "Read-only by default").
//! - Enabling editing *while already enabled*, disabling while read-only, and
//!   every other non-explicit event are rejected, so the UI can surface "you
//!   asked for something that didn't happen" instead of silently no-op-ing.

/// The two gate states, one per connection.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum GateState {
    /// Every connection starts here. Reads only.
    #[default]
    ReadOnly,
    /// Explicitly enabled editing for this connection. Resets to
    /// [`GateState::ReadOnly`] on disconnect or app restart.
    WriteEnabled,
}

/// Events that drive the gate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum GateEvent {
    /// The user explicitly enabled editing for this connection.
    EnableEditing,
    /// The user disabled editing, or declined the enable prompt.
    DisableEditing,
    /// The connection dropped (network error, server gone, user disconnect).
    Disconnected,
    /// The application restarted.
    AppRestart,
}

/// Why a transition was rejected.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Rejection {
    /// Editing is already enabled; nothing to do.
    AlreadyEnabled,
    /// The gate is already read-only; nothing to disable.
    AlreadyReadOnly,
    /// Unreachable by construction; kept so callers match exhaustively.
    Impossible,
}

/// The gate state machine. Construct one per connection; drive it with
/// [`Gate::apply`]; read it with [`Gate::state`]. It is deliberately dumb —
/// no timers, no side effects — so the reset-on-disconnect rule lives here
/// and cannot be forgotten by any UI code that merely consumes events.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct WriteGate {
    state: GateState,
}

impl WriteGate {
    /// A fresh gate, read-only (PRD story 26: every connection starts
    /// read-only).
    pub const fn new() -> Self {
        Self {
            state: GateState::ReadOnly,
        }
    }

    /// The current state.
    pub const fn state(&self) -> GateState {
        self.state
    }

    /// True when the gate is read-only.
    pub const fn is_read_only(&self) -> bool {
        matches!(self.state, GateState::ReadOnly)
    }

    /// Applies an event, returning the new state on success.
    ///
    /// Returns [`Err(Rejection)`] when the event is a no-op in the current
    /// state (e.g. enabling while already enabled). The forced resets
    /// ([`GateEvent::Disconnected`], [`GateEvent::AppRestart`]) never fail.
    pub fn apply(&mut self, event: GateEvent) -> Result<GateState, Rejection> {
        match (self.state, event) {
            // The one and only entry into WriteEnabled: explicit user action.
            (GateState::ReadOnly, GateEvent::EnableEditing) => {
                self.state = GateState::WriteEnabled;
                Ok(self.state)
            }
            (GateState::ReadOnly, GateEvent::DisableEditing) => Err(Rejection::AlreadyReadOnly),

            (GateState::WriteEnabled, GateEvent::DisableEditing) => {
                self.state = GateState::ReadOnly;
                Ok(self.state)
            }
            (GateState::WriteEnabled, GateEvent::EnableEditing) => Err(Rejection::AlreadyEnabled),

            // Forced reset from BOTH states: never fails, never negotiates.
            (_, GateEvent::Disconnected) | (_, GateEvent::AppRestart) => {
                self.state = GateState::ReadOnly;
                Ok(self.state)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_read_only() {
        assert_eq!(WriteGate::new().state(), GateState::ReadOnly);
        assert!(WriteGate::new().is_read_only());
        assert_eq!(WriteGate::default().state(), GateState::ReadOnly);
    }

    #[test]
    fn explicit_enable_and_disable_round_trip() {
        let mut gate = WriteGate::new();
        assert_eq!(
            gate.apply(GateEvent::EnableEditing),
            Ok(GateState::WriteEnabled)
        );
        assert!(!gate.is_read_only());
        assert_eq!(
            gate.apply(GateEvent::DisableEditing),
            Ok(GateState::ReadOnly)
        );
        assert!(gate.is_read_only());
    }

    #[test]
    fn enable_is_the_only_path_into_write_enabled() {
        // From ReadOnly, only EnableEditing lands in WriteEnabled.
        let mut gate = WriteGate::new();
        for event in [
            GateEvent::DisableEditing,
            GateEvent::Disconnected,
            GateEvent::AppRestart,
        ] {
            let _ = gate.apply(event);
            assert_eq!(
                gate.state(),
                GateState::ReadOnly,
                "{event:?} must not enable"
            );
        }
        assert_eq!(
            gate.apply(GateEvent::EnableEditing),
            Ok(GateState::WriteEnabled)
        );
    }

    #[test]
    fn disconnect_forces_read_only_from_write_enabled() {
        let mut gate = WriteGate::new();
        gate.apply(GateEvent::EnableEditing).unwrap();
        assert_eq!(gate.apply(GateEvent::Disconnected), Ok(GateState::ReadOnly));
        assert!(gate.is_read_only());
    }

    #[test]
    fn app_restart_forces_read_only_from_write_enabled() {
        let mut gate = WriteGate::new();
        gate.apply(GateEvent::EnableEditing).unwrap();
        assert_eq!(gate.apply(GateEvent::AppRestart), Ok(GateState::ReadOnly));
        assert!(gate.is_read_only());
    }

    #[test]
    fn disconnect_forces_read_only_from_read_only() {
        let mut gate = WriteGate::new();
        assert_eq!(gate.apply(GateEvent::Disconnected), Ok(GateState::ReadOnly));
        assert_eq!(gate.apply(GateEvent::AppRestart), Ok(GateState::ReadOnly));
    }

    #[test]
    fn re_enable_after_reset_requires_explicit_event() {
        // The reset leaves the gate ReadOnly; WriteEnabled does not survive
        // the reconnect implicitly.
        let mut gate = WriteGate::new();
        gate.apply(GateEvent::EnableEditing).unwrap();
        gate.apply(GateEvent::Disconnected).unwrap();
        assert_eq!(gate.state(), GateState::ReadOnly);
        // A second disconnect in read-only is fine (idempotent reset).
        assert_eq!(gate.apply(GateEvent::Disconnected), Ok(GateState::ReadOnly));
        // And enabling again is an explicit act.
        assert_eq!(
            gate.apply(GateEvent::EnableEditing),
            Ok(GateState::WriteEnabled)
        );
    }

    #[test]
    fn illegal_transitions_are_rejected() {
        // Enable while already enabled.
        let mut gate = WriteGate::new();
        gate.apply(GateEvent::EnableEditing).unwrap();
        assert_eq!(
            gate.apply(GateEvent::EnableEditing),
            Err(Rejection::AlreadyEnabled)
        );
        assert_eq!(
            gate.state(),
            GateState::WriteEnabled,
            "state unchanged on rejection"
        );

        // Disable while already read-only.
        let mut gate = WriteGate::new();
        assert_eq!(
            gate.apply(GateEvent::DisableEditing),
            Err(Rejection::AlreadyReadOnly)
        );
        assert_eq!(
            gate.state(),
            GateState::ReadOnly,
            "state unchanged on rejection"
        );
    }

    #[test]
    fn every_state_event_pair_is_covered() {
        // Exhaustive: all 2 states x 4 events. Forced resets succeed from
        // both states; everything else follows the invariant above.
        for state in [GateState::ReadOnly, GateState::WriteEnabled] {
            for event in [
                GateEvent::EnableEditing,
                GateEvent::DisableEditing,
                GateEvent::Disconnected,
                GateEvent::AppRestart,
            ] {
                let mut gate = WriteGate { state };
                let result = gate.apply(event);
                match (state, event) {
                    (GateState::ReadOnly, GateEvent::EnableEditing) => {
                        assert_eq!(result, Ok(GateState::WriteEnabled));
                    }
                    (GateState::ReadOnly, GateEvent::DisableEditing) => {
                        assert_eq!(result, Err(Rejection::AlreadyReadOnly));
                    }
                    (GateState::WriteEnabled, GateEvent::DisableEditing) => {
                        assert_eq!(result, Ok(GateState::ReadOnly));
                    }
                    (GateState::WriteEnabled, GateEvent::EnableEditing) => {
                        assert_eq!(result, Err(Rejection::AlreadyEnabled));
                    }
                    // Forced resets: Ok(ReadOnly) from both states.
                    (_, GateEvent::Disconnected) | (_, GateEvent::AppRestart) => {
                        assert_eq!(result, Ok(GateState::ReadOnly));
                    }
                }
                // The Impossible rejection is never produced.
                assert_ne!(result, Err(Rejection::Impossible));
            }
        }
    }

    #[test]
    fn gate_is_copy_and_value_semantics() {
        // Cloning a gate snapshots it; mutating the clone must not leak into
        // the original (per-connection isolation is the caller's job, but
        // value semantics make accidental sharing harmless).
        let mut original = WriteGate::new();
        original.apply(GateEvent::EnableEditing).unwrap();
        let mut clone = original;
        clone.apply(GateEvent::Disconnected).unwrap();
        assert_eq!(original.state(), GateState::WriteEnabled);
        assert_eq!(clone.state(), GateState::ReadOnly);
    }
}
