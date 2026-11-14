//! Command safety for a write-safe Redis GUI.
//!
//! This crate holds the product's defining guarantee: **never damage data by
//! accident**. It is pure logic with no UI and no network — exhaustively
//! unit-testable (PLAN.md section 3).
//!
//! Responsibilities:
//! - The **command policy table** ([`command_policy`]): a const
//!   classification of Redis commands into `ReadOnly` / `Write` /
//!   `Destructive` / `ScriptServerSide` / `Unknown`.
//! - The **read-only gate** ([`gate`]): a per-connection state machine
//!   `ReadOnly ⇄ WriteEnabled`, forced reset to `ReadOnly` on disconnect or
//!   app restart. The reset lives here, not in the UI, so it cannot be
//!   forgotten.
//! - **Confirmation classification** ([`confirmation`]): typed
//!   confirmations for destructive commands, naming what is lost.
//!
//! One entry point, [`classify(command, gate) → Decision`], is shared by the
//! console and the edit/save paths, so both have one brain.

pub mod command_policy;
pub mod confirmation;
pub mod gate;

pub use command_policy::CommandKind;
pub use confirmation::NeedsConfirmation;
pub use gate::{GateEvent, GateState, Rejection, WriteGate};

/// A pre-tokenized Redis command, as produced by the console parser or the
/// edit/save paths.
///
/// Design choice (documented for `console-engine` / `redis-io`):
/// - `name` is a `String` because Redis command names are ASCII; the caller
///   sets the **complete** command name, including the subcommand word where
///   Redis defines one (`"CONFIG GET"`, `"SCRIPT FLUSH"`, `"MEMORY USAGE"`).
///   [`classify`] additionally tries `name + " " + args[0]` when the bare
///   name is unrecognized, so a tokenizer that splits `SCRIPT FLUSH` into
///   name `"SCRIPT"` and arg `"FLUSH"` still classifies correctly.
/// - `args` are raw bytes (`Vec<Vec<u8>>`) because Redis keys and values are
///   arbitrary bytes; only their *count* feeds the policy (multi-key delete
///   escalation per D3), and the rare arg that reaches a scope description
///   (SWAPDB indexes) is rendered with `from_utf8_lossy`. Byte faithfulness
///   itself is `value-model`'s job.
///
/// Tokenization lives in `console-engine`; this crate only consumes the
/// result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParsedCommand {
    /// The complete command name, e.g. `"GET"`, `"CONFIG GET"`,
    /// `"SCRIPT FLUSH"`. Case-insensitive.
    pub name: String,
    /// The command's arguments as raw bytes, one entry per argument.
    pub args: Vec<Vec<u8>>,
}

impl ParsedCommand {
    /// Builds a command from `&str` pieces (tests and ASCII-only callers).
    pub fn from_strs(name: &str, args: &[&str]) -> Self {
        Self {
            name: name.to_string(),
            args: args.iter().map(|a| a.as_bytes().to_vec()).collect(),
        }
    }

    /// The args rendered as `&str` (lossy for non-UTF-8) for scope text.
    fn args_lossy(&self) -> Vec<String> {
        self.args
            .iter()
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .collect()
    }
}

/// Why a command was blocked. Carries the user-facing message the console
/// prints (PRD AC8: refusals are proven client-side, before any bytes are
/// sent).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum BlockedReason {
    /// A write/destructive command while the connection is read-only.
    WriteBlockedByReadOnly,
    /// A server-side script (EVAL family) while read-only (PRD story 37).
    ScriptsBlockedByReadOnly,
    /// A command not in the policy table while read-only (PRD story 37).
    UnknownCommand,
}

impl BlockedReason {
    /// The user-facing explanation (verbatim contract with the console UI).
    pub fn message(&self) -> &'static str {
        match self {
            Self::WriteBlockedByReadOnly => {
                "blocked: the connection is read-only; enable editing to run write commands"
            }
            Self::ScriptsBlockedByReadOnly => {
                "blocked: server-side scripts cannot be verified as read-only, so they are \
                 blocked in read-only mode"
            }
            Self::UnknownCommand => {
                "blocked: this command is not recognized, so read-only mode refuses to run it"
            }
        }
    }
}

/// The decision for one command against one gate state. This is the single
/// output both the console and the edit/save paths act on.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Send it to the server.
    Run,
    /// Do not send anything; show [`BlockedReason::message`].
    Blocked(BlockedReason),
    /// Destructive in write mode: collect the typed confirmation first
    /// ([`NeedsConfirmation::is_satisfied_by`]); declining means nothing is
    /// sent (PRD AC8).
    Confirm(NeedsConfirmation),
}

/// Resolves the effective command name: the stored name if it is recognized,
/// else `name + " " + first_arg` (covers tokenizers that split
/// `SCRIPT FLUSH` / `CONFIG GET` into name + first arg).
fn resolve_name(command: &ParsedCommand) -> String {
    if command_policy::lookup(&command.name) != CommandKind::Unknown {
        return command.name.clone();
    }
    if let Some(first) = command.args.first() {
        let joined = format!("{} {}", command.name, String::from_utf8_lossy(first).trim());
        if command_policy::lookup(&joined) != CommandKind::Unknown {
            return joined;
        }
    }
    command.name.clone()
}

/// The single entry point (PLAN.md section 4): one brain for the console and
/// the edit/save paths.
///
/// Rules:
/// - Read-only gate: `ReadOnly` commands **Run**; `Write`, `Destructive`
///   (including escalated multi-key DEL), `ScriptServerSide`, and `Unknown`
///   are **Blocked** with distinct explanatory messages.
/// - Write-enabled gate: `ReadOnly` and `Write` commands **Run**;
///   `Destructive` → **Confirm** (scope names what is lost); `Unknown` →
///   **Run** (guardrail on the user's own server); `ScriptServerSide` →
///   **Run** (the PRD blocks scripts in read-only mode only).
/// - MULTI/EXEC per D2: classified `Write` — blocked in read-only, plain Run
///   in write mode. The commands inside a transaction are classified
///   separately when the console sends them.
///
/// Pure function: no I/O, no side effects, no clock.
pub fn classify(command: &ParsedCommand, gate: &WriteGate) -> Decision {
    let name = resolve_name(command);
    let args = command.args_lossy();
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let kind = confirmation::effective_kind(&name, &arg_refs);

    match (gate.state(), kind) {
        // ---- Read-only gate (PRD story 37, AC8) ----
        (GateState::ReadOnly, CommandKind::ReadOnly) => Decision::Run,
        (GateState::ReadOnly, CommandKind::Write)
        | (GateState::ReadOnly, CommandKind::Destructive) => {
            Decision::Blocked(BlockedReason::WriteBlockedByReadOnly)
        }
        (GateState::ReadOnly, CommandKind::ScriptServerSide) => {
            Decision::Blocked(BlockedReason::ScriptsBlockedByReadOnly)
        }
        (GateState::ReadOnly, CommandKind::Unknown) => {
            Decision::Blocked(BlockedReason::UnknownCommand)
        }

        // ---- Write-enabled gate (PRD story 38) ----
        (GateState::WriteEnabled, CommandKind::ReadOnly)
        | (GateState::WriteEnabled, CommandKind::Write) => Decision::Run,
        (GateState::WriteEnabled, CommandKind::Destructive) => {
            let confirmation = confirmation::scope_for(&name, &arg_refs)
                .map(|scope| NeedsConfirmation::for_action(&name, &scope))
                .unwrap_or_else(|| {
                    // Multi-key DEL/UNLINK escalation (D3): always confirm,
                    // naming the count.
                    let count = command.args.len().max(1);
                    NeedsConfirmation::for_multi_key_delete(count)
                });
            Decision::Confirm(confirmation)
        }
        (GateState::WriteEnabled, CommandKind::ScriptServerSide) => Decision::Run,
        (GateState::WriteEnabled, CommandKind::Unknown) => Decision::Run,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ro() -> WriteGate {
        WriteGate::new()
    }

    fn we() -> WriteGate {
        let mut gate = WriteGate::new();
        gate.apply(GateEvent::EnableEditing).unwrap();
        gate
    }

    // ---- Read-only mode: the allowlist (PRD story 37 / AC8) ----

    #[test]
    fn read_only_allowlist_runs_in_read_only_mode() {
        for name in [
            "GET",
            "MGET",
            "EXISTS",
            "TYPE",
            "TTL",
            "SCAN",
            "KEYS",
            "HGETALL",
            "LRANGE",
            "SMEMBERS",
            "ZRANGE",
            "XRANGE",
            "INFO",
            "DBSIZE",
            "PING",
            "ECHO",
            "CONFIG GET",
            "CLIENT LIST",
            "TIME",
            "STRLEN",
            "OBJECT",
            "MEMORY USAGE",
        ] {
            let cmd = ParsedCommand::from_strs(name, &["somekey"]);
            assert_eq!(
                classify(&cmd, &ro()),
                Decision::Run,
                "{name} must run in read-only mode"
            );
        }
    }

    #[test]
    fn write_commands_blocked_in_read_only_mode() {
        for name in [
            "SET", "DEL", "HSET", "LPUSH", "EXPIRE", "RENAME", "INCR", "SELECT", "MULTI",
        ] {
            let cmd = ParsedCommand::from_strs(name, &["somekey"]);
            assert_eq!(
                classify(&cmd, &ro()),
                Decision::Blocked(BlockedReason::WriteBlockedByReadOnly),
                "{name} must be blocked in read-only mode"
            );
        }
    }

    #[test]
    fn write_block_message_is_explanatory() {
        let cmd = ParsedCommand::from_strs("SET", &["k", "v"]);
        let Decision::Blocked(reason) = classify(&cmd, &ro()) else {
            panic!("SET must be blocked in read-only mode");
        };
        let msg = reason.message();
        assert!(
            msg.contains("read-only"),
            "message explains the gate: {msg}"
        );
        assert!(
            msg.contains("enable editing"),
            "message says how to proceed: {msg}"
        );
    }

    #[test]
    fn destructive_blocked_in_read_only_mode() {
        for name in ["FLUSHALL", "FLUSHDB", "SWAPDB", "SCRIPT FLUSH"] {
            let cmd = ParsedCommand::from_strs(name, &[]);
            assert_eq!(
                classify(&cmd, &ro()),
                Decision::Blocked(BlockedReason::WriteBlockedByReadOnly),
                "{name} must be blocked in read-only mode"
            );
        }
    }

    #[test]
    fn scripts_blocked_in_read_only_mode() {
        for name in [
            "EVAL",
            "EVALSHA",
            "EVAL_RO",
            "EVALSHA_RO",
            "SCRIPT LOAD",
            "FCALL",
            "FCALL_RO",
        ] {
            let cmd = ParsedCommand::from_strs(name, &["return 1", "0"]);
            assert_eq!(
                classify(&cmd, &ro()),
                Decision::Blocked(BlockedReason::ScriptsBlockedByReadOnly),
                "{name} must be blocked in read-only mode (D1)"
            );
        }
    }

    #[test]
    fn script_block_message_explains_why() {
        let cmd = ParsedCommand::from_strs("EVAL", &["return 1", "0"]);
        let Decision::Blocked(reason) = classify(&cmd, &ro()) else {
            panic!("EVAL must be blocked in read-only mode");
        };
        assert!(reason.message().contains("server-side script"));
    }

    #[test]
    fn unknown_command_blocked_in_read_only_mode() {
        let cmd = ParsedCommand::from_strs("WOBBLE", &["x"]);
        assert_eq!(
            classify(&cmd, &ro()),
            Decision::Blocked(BlockedReason::UnknownCommand)
        );
    }

    #[test]
    fn unknown_block_message_says_not_recognized() {
        let cmd = ParsedCommand::from_strs("WOBBLE", &["x"]);
        let Decision::Blocked(reason) = classify(&cmd, &ro()) else {
            panic!("unknown command must be blocked in read-only mode");
        };
        assert!(reason.message().contains("not recognized"));
    }

    // ---- Write-enabled mode (PRD story 38) ----

    #[test]
    fn read_only_and_write_commands_run_in_write_mode() {
        for (name, args) in [
            ("GET", vec!["k"]),
            ("SET", vec!["k", "v"]),
            ("HSET", vec!["h", "f", "v"]),
            ("EXPIRE", vec!["k", "60"]),
            ("MULTI", vec![]),
            ("EXEC", vec![]),
        ] {
            let cmd = ParsedCommand::from_strs(name, &args);
            assert_eq!(classify(&cmd, &we()), Decision::Run, "{name}");
        }
    }

    #[test]
    fn unknown_command_runs_in_write_mode() {
        // Guardrail, not a wall: in write mode the user's own server decides.
        let cmd = ParsedCommand::from_strs("WOBBLE", &["x"]);
        assert_eq!(classify(&cmd, &we()), Decision::Run);
    }

    #[test]
    fn scripts_run_in_write_mode() {
        // The PRD blocks server-side scripts in read-only mode only.
        for name in ["EVAL", "EVALSHA", "EVAL_RO", "SCRIPT LOAD"] {
            let cmd = ParsedCommand::from_strs(name, &["return 1", "0"]);
            assert_eq!(classify(&cmd, &we()), Decision::Run, "{name}");
        }
    }

    #[test]
    fn flushall_needs_confirmation_naming_the_server() {
        let cmd = ParsedCommand::from_strs("FLUSHALL", &[]);
        let Decision::Confirm(c) = classify(&cmd, &we()) else {
            panic!("FLUSHALL must require confirmation in write mode");
        };
        assert_eq!(c.action, "FLUSHALL");
        assert_eq!(
            c.scope_description,
            "all keys in every database on this server"
        );
        assert_eq!(c.confirm_phrase, "FLUSHALL");
    }

    #[test]
    fn flushdb_needs_confirmation_naming_the_current_database() {
        let cmd = ParsedCommand::from_strs("FLUSHDB", &[]);
        let Decision::Confirm(c) = classify(&cmd, &we()) else {
            panic!("FLUSHDB must require confirmation in write mode");
        };
        assert_eq!(c.action, "FLUSHDB");
        assert_eq!(c.scope_description, "all keys in the current database");
    }

    #[test]
    fn flush_never_confirms_in_read_only_it_blocks_instead() {
        // Confirmation only exists in write mode; read-only just blocks.
        let cmd = ParsedCommand::from_strs("FLUSHDB", &[]);
        assert_eq!(
            classify(&cmd, &ro()),
            Decision::Blocked(BlockedReason::WriteBlockedByReadOnly)
        );
    }

    #[test]
    fn swapdb_confirms_naming_both_databases() {
        let cmd = ParsedCommand::from_strs("SWAPDB", &["0", "1"]);
        let Decision::Confirm(c) = classify(&cmd, &we()) else {
            panic!("SWAPDB must require confirmation in write mode");
        };
        assert_eq!(c.action, "SWAPDB");
        assert_eq!(
            c.scope_description,
            "swaps databases 0 and 1 (all keys in both)"
        );
    }

    #[test]
    fn script_flush_confirms_naming_cached_scripts() {
        let cmd = ParsedCommand::from_strs("SCRIPT FLUSH", &[]);
        let Decision::Confirm(c) = classify(&cmd, &we()) else {
            panic!("SCRIPT FLUSH must require confirmation in write mode");
        };
        assert!(c.scope_description.contains("Lua scripts"));
    }

    #[test]
    fn multi_key_del_escalates_to_confirmation_with_count() {
        let cmd = ParsedCommand::from_strs("DEL", &["a", "b", "c"]);
        let Decision::Confirm(c) = classify(&cmd, &we()) else {
            panic!("multi-key DEL must require confirmation (D3)");
        };
        assert_eq!(c.action, "DEL");
        assert_eq!(c.scope_description, "3 keys");
    }

    #[test]
    fn multi_key_unlink_escalates_to_confirmation_with_count() {
        let cmd = ParsedCommand::from_strs("UNLINK", &["a", "b"]);
        let Decision::Confirm(c) = classify(&cmd, &we()) else {
            panic!("multi-key UNLINK must require confirmation (D3)");
        };
        assert_eq!(c.scope_description, "2 keys");
    }

    #[test]
    fn multi_key_del_blocked_in_read_only_mode() {
        // Escalation is independent of gate: in read-only it is simply
        // blocked as a write.
        let cmd = ParsedCommand::from_strs("DEL", &["a", "b", "c"]);
        assert_eq!(
            classify(&cmd, &ro()),
            Decision::Blocked(BlockedReason::WriteBlockedByReadOnly)
        );
    }

    #[test]
    fn single_key_del_in_write_mode_is_a_plain_run() {
        let cmd = ParsedCommand::from_strs("DEL", &["a"]);
        assert_eq!(classify(&cmd, &we()), Decision::Run);
        let cmd = ParsedCommand::from_strs("UNLINK", &["a"]);
        assert_eq!(classify(&cmd, &we()), Decision::Run);
    }

    // ---- Case-insensitivity and aliases ----

    #[test]
    fn classification_is_case_insensitive() {
        for (name, expected) in [
            (
                "flushdb",
                Decision::Confirm(NeedsConfirmation::for_action(
                    "FLUSHDB",
                    "all keys in the current database",
                )),
            ),
            (
                "FlushAll",
                Decision::Confirm(NeedsConfirmation::for_action(
                    "FLUSHALL",
                    "all keys in every database on this server",
                )),
            ),
            ("get", Decision::Run),
            ("sEt", Decision::Run),
        ] {
            let cmd = ParsedCommand::from_strs(name, &["k"]);
            assert_eq!(classify(&cmd, &we()), expected, "{name}");
        }
    }

    #[test]
    fn read_only_case_insensitive() {
        let cmd = ParsedCommand::from_strs("ping", &[]);
        assert_eq!(classify(&cmd, &ro()), Decision::Run);
        let cmd = ParsedCommand::from_strs("dbsize", &[]);
        assert_eq!(classify(&cmd, &ro()), Decision::Run);
        let cmd = ParsedCommand::from_strs("eval_ro", &["return 1", "0"]);
        assert_eq!(
            classify(&cmd, &ro()),
            Decision::Blocked(BlockedReason::ScriptsBlockedByReadOnly)
        );
    }

    #[test]
    fn getex_classified_as_write() {
        // GETEX mutates TTL state, so it is a write even though it reads.
        let cmd = ParsedCommand::from_strs("GETEX", &["k", "EX", "60"]);
        assert_eq!(
            classify(&cmd, &ro()),
            Decision::Blocked(BlockedReason::WriteBlockedByReadOnly)
        );
        assert_eq!(classify(&cmd, &we()), Decision::Run);
    }

    // ---- Subcommand name resolution (tokenizer tolerance) ----

    #[test]
    fn split_subcommands_still_classify() {
        // A tokenizer that splits `SCRIPT FLUSH` into name + first arg.
        let cmd = ParsedCommand::from_strs("SCRIPT", &["FLUSH"]);
        assert_eq!(
            classify(&cmd, &ro()),
            Decision::Blocked(BlockedReason::WriteBlockedByReadOnly)
        );
        let Decision::Confirm(c) = classify(&cmd, &we()) else {
            panic!("SCRIPT + FLUSH arg must classify as SCRIPT FLUSH");
        };
        assert_eq!(c.action, "SCRIPT FLUSH");
        assert!(c.scope_description.contains("Lua scripts"));

        let cmd = ParsedCommand::from_strs("CONFIG", &["GET", "maxmemory"]);
        assert_eq!(classify(&cmd, &ro()), Decision::Run);
        let cmd = ParsedCommand::from_strs("MEMORY", &["USAGE", "k"]);
        assert_eq!(classify(&cmd, &ro()), Decision::Run);
        let cmd = ParsedCommand::from_strs("SCRIPT", &["LOAD", "return 1"]);
        assert_eq!(
            classify(&cmd, &ro()),
            Decision::Blocked(BlockedReason::ScriptsBlockedByReadOnly)
        );
    }

    // ---- Gate × classify integration: reset-on-disconnect ----

    #[test]
    fn disconnect_resets_the_gate_and_write_commands_block_again() {
        let mut gate = WriteGate::new();
        let set = ParsedCommand::from_strs("SET", &["k", "v"]);
        assert_eq!(
            classify(&set, &gate),
            Decision::Blocked(BlockedReason::WriteBlockedByReadOnly)
        );

        gate.apply(GateEvent::EnableEditing).unwrap();
        assert_eq!(classify(&set, &gate), Decision::Run);

        gate.apply(GateEvent::Disconnected).unwrap();
        assert_eq!(
            classify(&set, &gate),
            Decision::Blocked(BlockedReason::WriteBlockedByReadOnly),
            "a network drop must re-arm read-only (PRD story 27)"
        );
    }

    #[test]
    fn app_restart_resets_the_gate_from_write_enabled() {
        let mut gate = WriteGate::new();
        gate.apply(GateEvent::EnableEditing).unwrap();
        gate.apply(GateEvent::AppRestart).unwrap();
        assert!(gate.is_read_only());
        let del = ParsedCommand::from_strs("DEL", &["a"]);
        assert_eq!(
            classify(&del, &gate),
            Decision::Blocked(BlockedReason::WriteBlockedByReadOnly)
        );
    }

    #[test]
    fn declining_enable_keeps_read_only() {
        let mut gate = WriteGate::new();
        // User declines: the enable prompt answers DisableEditing.
        let _ = gate.apply(GateEvent::DisableEditing);
        let cmd = ParsedCommand::from_strs("FLUSHDB", &[]);
        assert_eq!(
            classify(&cmd, &gate),
            Decision::Blocked(BlockedReason::WriteBlockedByReadOnly)
        );
    }

    #[test]
    fn confirmation_phrase_check_is_part_of_the_contract() {
        let cmd = ParsedCommand::from_strs("FLUSHDB", &[]);
        let Decision::Confirm(c) = classify(&cmd, &we()) else {
            panic!()
        };
        assert!(c.is_satisfied_by("flushdb"));
        assert!(!c.is_satisfied_by("FLUSHALL"));
        assert!(!c.is_satisfied_by("yes"));
    }

    // ---- Byte-faithful args ----

    #[test]
    fn args_are_raw_bytes_and_only_counted() {
        // A non-UTF-8 key arg must not break classification; DEL counts it.
        let cmd = ParsedCommand {
            name: "DEL".to_string(),
            args: vec![vec![0xFF, 0xFE], vec![0x00, 0x01]],
        };
        let Decision::Confirm(c) = classify(&cmd, &we()) else {
            panic!("two-key DEL must confirm even with binary keys");
        };
        assert_eq!(c.scope_description, "2 keys");
    }

    // ---- Exhaustiveness guard ----

    #[test]
    fn every_command_kind_meets_every_gate_state() {
        // 5 kinds x 2 states, from a representative of each table.
        for (name, args, kind) in [
            ("GET", vec!["k"], CommandKind::ReadOnly),
            ("SET", vec!["k", "v"], CommandKind::Write),
            ("FLUSHDB", vec![], CommandKind::Destructive),
            ("EVAL", vec!["return 1", "0"], CommandKind::ScriptServerSide),
            ("WOBBLE", vec!["x"], CommandKind::Unknown),
        ] {
            let cmd = ParsedCommand::from_strs(name, &args);
            for gate in [ro(), we()] {
                let decision = classify(&cmd, &gate);
                match (gate.state(), kind) {
                    (GateState::ReadOnly, CommandKind::ReadOnly) => {
                        assert_eq!(decision, Decision::Run, "{name}");
                    }
                    (GateState::ReadOnly, CommandKind::Write)
                    | (GateState::ReadOnly, CommandKind::Destructive) => {
                        assert_eq!(
                            decision,
                            Decision::Blocked(BlockedReason::WriteBlockedByReadOnly),
                            "{name}"
                        );
                    }
                    (GateState::ReadOnly, CommandKind::ScriptServerSide) => {
                        assert_eq!(
                            decision,
                            Decision::Blocked(BlockedReason::ScriptsBlockedByReadOnly),
                            "{name}"
                        );
                    }
                    (GateState::ReadOnly, CommandKind::Unknown) => {
                        assert_eq!(
                            decision,
                            Decision::Blocked(BlockedReason::UnknownCommand),
                            "{name}"
                        );
                    }
                    (GateState::WriteEnabled, CommandKind::ReadOnly)
                    | (GateState::WriteEnabled, CommandKind::Write)
                    | (GateState::WriteEnabled, CommandKind::ScriptServerSide)
                    | (GateState::WriteEnabled, CommandKind::Unknown) => {
                        assert_eq!(decision, Decision::Run, "{name}");
                    }
                    (GateState::WriteEnabled, CommandKind::Destructive) => {
                        assert!(matches!(decision, Decision::Confirm(_)), "{name}");
                    }
                }
            }
        }
    }
}
