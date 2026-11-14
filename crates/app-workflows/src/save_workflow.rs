//! The SAVE workflow: the headless mirror of the app session's save
//! discipline (acceptance 3 / PRD stories 26–29, 31; PLAN.md §5).
//!
//! # What this mirrors (the honest seam note)
//!
//! `src/session.rs` (app crate) holds a **concrete** `RedisConnection`
//! inside a GPUI entity, so it cannot be driven without a window. This
//! struct re-implements its save discipline step by step, using the same
//! production crates. Line references below are `src/session.rs` unless
//! stated.
//!
//! | Workflow step | session.rs mirror |
//! |---|---|
//! | Gate check (write-safety `classify`) before any write | `gate: WriteGate` field (line 216); `issue_console_command` refuses unless `classify` said Run (lines 1406-1423); gate reset on every `ConnEvent::Disconnected` (lines 1591, 1613, 1664) |
//! | Issue through the `Connection` trait, one command at a time | `run_io` + `conn.lock_owned()` serial discipline (lines 29-37 module docs, 1419-1423) |
//! | `WriteOutcome::from_conn_error` mapping — the ONE tri-state rule | `issue_console_command` lines 1436-1449 |
//! | `OutcomeUnknown` → honest "check before retrying" state, never a resend | same lines + `format_write_outcome`; the never-resend half is the redis-io sticky-broken contract (`redis_backend.rs` lines 24-28) |
//!
//! The edit/save state machine (`EditorState::save_encoding`,
//! `save_acknowledged`, `apply_refresh`) is value-model's production
//! code, used verbatim — not mirrored.

use redis_io::connection::{Command, ConnError, Connection, Reply};
use redis_io::outcome::{DefinitiveError, DefinitiveErrorKind, WriteOutcome};
use value_model::editor::EditorState;
use value_model::raw::RawValue;
use write_safety::gate::{GateEvent, GateState, WriteGate};
use write_safety::{classify, BlockedReason, Decision, ParsedCommand};

use crate::mock::ScriptedConnection;

/// Why a save refused before issuing anything.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SaveRefusal {
    /// The gate is read-only (PRD stories 26-27): refuses with the
    /// write-safety wording, zero commands sent.
    GateReadOnly,
    /// The CAS precheck found the server's value changed under the edit
    /// (PRD acceptance 9 / PLAN.md F5): refuses, never overwrites
    /// silently.
    Conflict,
    /// Nothing to save: the editor is clean.
    NotDirty,
}

impl SaveRefusal {
    /// The user-facing message. For `GateReadOnly` this is the VERBATIM
    /// write-safety refusal wording (`BlockedReason::message`) — one
    /// brain for console and edit/save paths (PLAN.md §4).
    pub fn message(&self) -> &'static str {
        match self {
            SaveRefusal::GateReadOnly => BlockedReason::WriteBlockedByReadOnly.message(),
            SaveRefusal::Conflict => {
                "conflict: the value changed on the server after you opened it — \
                 reload and reconcile before saving"
            }
            SaveRefusal::NotDirty => "nothing to save: no unsaved edits",
        }
    }
}

/// The error half of [`SaveFlow::Failed`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SaveFlowError {
    /// A definitive server failure (`Failed` tri-state arm). The editor
    /// is untouched; retry is a user action, never automatic.
    Definitive(String),
}

impl SaveFlowError {
    /// The user-facing error string.
    pub fn to_error_string(&self) -> &str {
        match self {
            SaveFlowError::Definitive(msg) => msg,
        }
    }
}

/// The outcome of driving one SAVE flow end-to-end — what the Phase 5
/// UI renders from (mirrors the session's tri-state handling).
#[derive(Clone, Debug)]
pub enum SaveFlow {
    /// The write(s) were issued and acknowledged: the editor is clean
    /// (the workflow already called `EditorState::save_acknowledged`).
    Acknowledged,
    /// The write definitively failed. The editor is untouched (still
    /// dirty); retry is a user action.
    Failed(SaveFlowError),
    /// The write's fate is unknown (dropped ack at a disconnect). The
    /// editor is UNTOUCHED — deliberately NOT `save_acknowledged` —
    /// because the write may have applied; the original bytes are
    /// preserved so a later re-read can compare. NOTHING is re-sent.
    OutcomeUnknown {
        /// The honest "check before retrying" message for the UI.
        check_before_retrying: String,
    },
    /// The save refused before ANY bytes were sent.
    Refused(SaveRefusal),
}

impl SaveFlow {
    /// `true` only for the `OutcomeUnknown` arm (the acceptance-3 state).
    pub fn is_outcome_unknown(&self) -> bool {
        matches!(self, SaveFlow::OutcomeUnknown { .. })
    }
}

/// The SAVE workflow harness: gate + editor + connection, driven exactly
/// as the app session would.
///
/// Not `Clone`: the connection is a single serial resource (mirrors the
/// `Arc<Mutex<...>>` + one-command-at-a-time discipline, session.rs
/// lines 29-37). Drive it from one async task.
pub struct SaveWorkflow<C: Connection> {
    gate: WriteGate,
    editor: EditorState,
    key: Vec<u8>,
    conn: C,
}

impl<C: Connection> SaveWorkflow<C> {
    /// Open a workflow over a fetched value. The gate starts ReadOnly
    /// (PRD story 26: every connection starts read-only; session.rs
    /// `gate: WriteGate::new()`, line 359).
    pub fn open(key: Vec<u8>, original: RawValue, conn: C) -> Self {
        Self {
            gate: WriteGate::new(),
            editor: EditorState::open(original),
            key,
            conn,
        }
    }

    /// The user explicitly enables editing (the ONLY path into
    /// `WriteEnabled`; PRD story 27, mirrors session.rs gate usage).
    pub fn enable_editing(&mut self) {
        let _ = self.gate.apply(GateEvent::EnableEditing);
    }

    /// Feed a disconnect event: the gate forces ReadOnly (PRD story 27 —
    /// "mode resets on disconnect"; session.rs lines 1591, 1613, 1664).
    pub fn on_disconnected(&mut self) {
        let _ = self.gate.apply(GateEvent::Disconnected);
    }

    /// The current gate state.
    pub fn gate_state(&self) -> GateState {
        self.gate.state()
    }

    /// Read-only access to the editor (assertions on dirty / diverged /
    /// original bytes).
    pub fn editor(&self) -> &EditorState {
        &self.editor
    }

    /// Mutate the editor (edit one field before saving). Delegates to
    /// `EditorState`'s validated setters — lossy display strings are a
    /// type error away from entering (value-model invariant 2).
    pub fn edit_text(&mut self, path: &value_model::editor::FieldPath, text: &str) {
        self.editor
            .edit_text(path, text)
            .expect("harness edits are valid by construction");
    }

    /// The save payloads: `SET` for scalar strings, one `HSET` per
    /// **edited** hash field. Encoded from `save_encoding()` — the only
    /// save-path encoder (value-model invariant 3: untouched entries
    /// keep their original bytes and are simply not written).
    fn save_commands(&self) -> Vec<Command> {
        let encoded = self.editor.save_encoding();
        match &encoded {
            RawValue::Hash(pairs) => {
                // One HSET per slot whose working value is Edited; the
                // field name bytes come from the original side.
                use value_model::editor::{EditedValue, Slot};
                if let EditedValue::Hash(working) = &self.editor.working {
                    working
                        .iter()
                        .enumerate()
                        .filter_map(|(i, (_, slot))| match slot {
                            Slot::Edited(_) => pairs.get(i).map(|(orig_field, value)| {
                                Command::new(
                                    "HSET",
                                    vec![
                                        self.key.clone(),
                                        orig_field.as_bytes().to_vec(),
                                        value.as_bytes().to_vec(),
                                    ],
                                )
                            }),
                            Slot::Original(_) => None,
                        })
                        .collect()
                } else {
                    Vec::new()
                }
            }
            RawValue::String(bytes) => {
                vec![Command::new(
                    "SET",
                    vec![self.key.clone(), bytes.as_bytes().to_vec()],
                )]
            }
            // This harness drives string and hash editors; the Phase 5
            // UI extends the table (RPUSH/SET-per-item, etc.).
            _ => Vec::new(),
        }
    }

    /// One complete SAVE flow: gate check → issue → tri-state mapping →
    /// post-state. Exactly the discipline `session.rs::
    /// issue_console_command` (lines 1406-1462) applies to writes,
    /// extended with the editor post-state the editing UI will own.
    ///
    /// Post-state rules (the acceptance-3 contract):
    /// - `Acknowledged` → `editor.save_acknowledged()` (clean).
    /// - `Failed` → editor untouched (still dirty); error surfaced.
    /// - `OutcomeUnknown` → editor UNTOUCHED — NOT `save_acknowledged`;
    ///   the honest state is surfaced and NOTHING is re-sent, ever (the
    ///   sticky-broken connection contract makes a resend structurally
    ///   impossible).
    pub async fn save(&mut self) -> SaveFlow {
        let commands = self.save_commands();
        if commands.is_empty() {
            return SaveFlow::Refused(SaveRefusal::NotDirty);
        }

        // 1. Gate check BEFORE any issue (session.rs consults the gate
        //    via classify before issuing; SET/HSET are `Write`, so a
        //    read-only gate refuses with the write-safety wording).
        let parsed = ParsedCommand {
            name: commands[0].name.clone(),
            args: commands[0].args.clone(),
        };
        match classify(&parsed, &self.gate) {
            Decision::Run => {}
            Decision::Blocked(_) | Decision::Confirm(_) => {
                // SET/HSET are Write (never Confirm): Blocked can only be
                // the read-only refusal here.
                return SaveFlow::Refused(SaveRefusal::GateReadOnly);
            }
        }

        // 2. Issue serially (the one-command-at-a-time discipline).
        for cmd in commands {
            let result = self.conn.issue(cmd.clone()).await;
            let outcome = map_reply(result).map_err(|e| WriteOutcome::from_conn_error(cmd, e));
            match outcome {
                Ok(()) | Err(WriteOutcome::Acknowledged) => continue,
                Err(WriteOutcome::Failed(e)) => {
                    // Definitive failure: editor untouched. (For a
                    // multi-field hash an earlier HSET may have landed;
                    // the editor is still NOT acknowledged — honest.)
                    return SaveFlow::Failed(SaveFlowError::Definitive(e.message));
                }
                Err(WriteOutcome::OutcomeUnknown(_)) => {
                    // THE acceptance-3 rule: editor UNTOUCHED (the write
                    // may have applied — acknowledging would falsify the
                    // original bytes a later re-read compares against).
                    // No resend: the connection is sticky-broken, and the
                    // workflow never re-issues. The gate also drops to
                    // ReadOnly (session.rs on_connection_error, lines
                    // 1447-1449).
                    self.on_disconnected();
                    return SaveFlow::OutcomeUnknown {
                        check_before_retrying: "outcome unknown: the server's reply was lost \
                            around a disconnect — check the key's current value before retrying"
                            .into(),
                    };
                }
            }
        }

        // 3. All acknowledged → clean editor (value-model contract).
        self.editor.save_acknowledged();
        SaveFlow::Acknowledged
    }

    /// Refresh mid-edit: new server bytes routed per `apply_refresh`
    /// (value-model invariant 1 — the working buffer survives).
    pub fn apply_refresh(&mut self, new_original: RawValue) {
        self.editor.apply_refresh(new_original);
    }

    /// Swap in a fresh connection after a disconnect — the harness mirror
    /// of `RedisConnection::reconnect` (redis_backend.rs lines 421-423):
    /// a fresh link, same session state, **re-sends nothing**. The gate
    /// is NOT re-enabled implicitly (the OutcomeUnknown path already
    /// forced it ReadOnly; PRD story 27).
    ///
    /// Type-changing (`Into<SaveWorkflow<C>>`), because a fresh link may
    /// be a different `Connection` implementor than the severed one (the
    /// fault-injecting wrapper is gone after the boundary, like the real
    /// backend's dead socket is gone).
    pub fn replace_connection<C2: Connection>(self, conn: C2) -> SaveWorkflow<C2> {
        SaveWorkflow {
            gate: self.gate,
            editor: self.editor,
            key: self.key,
            conn,
        }
    }

    /// SAVE with the CAS-style conflict precheck (PLAN.md F5): GET the
    /// key first; if the server's bytes differ from the editor's
    /// original, refuse with a conflict — NEVER overwrite silently.
    /// Otherwise proceed to [`save`](Self::save).
    pub async fn save_with_conflict_precheck(&mut self) -> SaveFlow {
        match self.conflict_precheck().await {
            Ok(()) => self.save().await,
            Err(refusal) => SaveFlow::Refused(refusal),
        }
    }

    /// CAS-style conflict precheck for scalars (PLAN.md F5 / PRD
    /// acceptance 9): re-GET the key; if the server's bytes differ from
    /// the editor's ORIGINAL, refuse the save with a conflict (never
    /// overwrite silently).
    ///
    /// This models the check half of WATCH/MULTI optimistic save at the
    /// workflow boundary; the transaction encoding itself lands with the
    /// Phase 5 UI. Honest limit: it is a precheck, not an atomic guard.
    pub async fn conflict_precheck(&mut self) -> Result<(), SaveRefusal> {
        let get = Command::new("GET", vec![self.key.clone()]);
        let server_bytes = match self.conn.issue(get).await {
            Ok(reply) => reply.as_bytes().map(|b| b.to_vec()),
            Err(_) => return Err(SaveRefusal::Conflict),
        };
        let original_bytes = match &self.editor.original {
            RawValue::String(b) => Some(b.as_bytes().to_vec()),
            _ => None,
        };
        match (server_bytes, original_bytes) {
            (Some(server), Some(original)) if server == original => Ok(()),
            _ => Err(SaveRefusal::Conflict),
        }
    }

    /// Read the key's current bytes (the "check" of check-before-retry).
    /// Through a severed (sticky-broken) connection this sends nothing
    /// and returns `None`.
    pub async fn read_current(&mut self) -> Option<Vec<u8>> {
        let get = Command::new("GET", vec![self.key.clone()]);
        match self.conn.issue(get).await {
            Ok(reply) => reply.as_bytes().map(|b| b.to_vec()),
            Err(_) => None,
        }
    }
}

/// Map a `Connection::issue` result into the tri-state pre-mapping:
/// `Reply::Okay` is the ack; any other reply is a definitive failure;
/// `ConnError` (Disconnected or Definitive) passes through for
/// `WriteOutcome::from_conn_error`.
fn map_reply(result: Result<Reply, ConnError>) -> Result<(), ConnError> {
    match result {
        Ok(Reply::Okay) => Ok(()),
        Ok(other) => Err(ConnError::Definitive(DefinitiveError::new(
            DefinitiveErrorKind::ServerError,
            format!("unexpected reply: {other:?}"),
        ))),
        Err(e) => Err(e),
    }
}

/// Connections that support the explicit reconnect operation (the mock
/// and `RedisConnection` both do). Separate trait so the workflow stays
/// generic over plain `Connection` implementors too.
#[async_trait::async_trait]
pub trait Reconnectable: Connection {
    /// Explicit reconnect; re-sends no application commands.
    async fn reconnect(&mut self) -> Result<(), ConnError>;
}

#[async_trait::async_trait]
impl Reconnectable for ScriptedConnection {
    async fn reconnect(&mut self) -> Result<(), ConnError> {
        // The mock's own reconnect: resets the severed flag only.
        ScriptedConnection::reconnect(self);
        Ok(())
    }
}
