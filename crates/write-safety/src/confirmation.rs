//! Typed confirmation model for destructive commands (PRD story 38, AC8).
//!
//! A destructive command never runs on the spot: the classifier returns a
//! [`NeedsConfirmation`] value that names **what will be lost** and the exact
//! phrase the user must type to proceed. The UI renders that; nothing here
//! touches the network.

use crate::command_policy::{CommandKind, DESTRUCTIVE, READ_ONLY, SCRIPT_SERVER_SIDE, WRITE};

/// A confirmation the UI must collect before a destructive command runs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NeedsConfirmation {
    /// What the user is about to do, e.g. `FLUSHDB`.
    pub action: String,
    /// Plain-language description of the data at stake, e.g.
    /// "all keys in DB 0". D3 (PLAN.md section 9): every multi-key delete
    /// confirms, naming the count.
    pub scope_description: String,
    /// The exact phrase the user must type to confirm.
    pub confirm_phrase: String,
}

impl NeedsConfirmation {
    /// Builds a confirmation for a command that destroys a named scope.
    ///
    /// The confirm phrase is deliberately tied to the action (`FLUSHDB`,
    /// not a generic "yes") so a typed confirmation can't be muscle-memoryed
    /// across different destructive commands.
    pub fn for_action(action: &str, scope_description: &str) -> Self {
        Self {
            action: action.to_ascii_uppercase(),
            scope_description: scope_description.to_string(),
            confirm_phrase: action.to_ascii_uppercase(),
        }
    }

    /// Builds the confirmation for a multi-key delete (D3: always confirms,
    /// count named in the scope).
    pub fn for_multi_key_delete(count: usize) -> Self {
        Self::for_action("DEL", &format!("{count} keys"))
    }

    /// True when `typed` matches the required confirm phrase
    /// (case-insensitive, surrounding whitespace trimmed).
    pub fn is_satisfied_by(&self, typed: &str) -> bool {
        typed.trim().eq_ignore_ascii_case(&self.confirm_phrase)
    }
}

/// Describes the scope a destructive command would destroy, based on its
/// arguments. Used by [`crate::classify`].
///
/// Argument shapes follow the Redis command reference:
/// - `FLUSHDB` — no key args; the scope is the whole current DB. We say
///   "the current database" because the DB index belongs to the connection
///   session, which this pure crate does not know.
/// - `FLUSHALL` — no key args; every key on the server.
/// - `SWAPDB a b` — both DBs named by the user.
/// - `DEL`/`UNLINK` with more than one key — "N keys" (D3).
pub(crate) fn scope_for(command_name: &str, args: &[&str]) -> Option<String> {
    match command_name.to_ascii_uppercase().as_str() {
        "FLUSHDB" => Some("all keys in the current database".to_string()),
        "FLUSHALL" => Some("all keys in every database on this server".to_string()),
        "SWAPDB" => {
            let a = args.first().copied().unwrap_or("?");
            let b = args.get(1).copied().unwrap_or("?");
            Some(format!("swaps databases {a} and {b} (all keys in both)"))
        }
        "SCRIPT FLUSH" => Some("all cached Lua scripts on this server".to_string()),
        _ => None,
    }
}

/// The number of keys a delete-family command touches, used to escalate
/// multi-key deletes per D3.
pub(crate) fn delete_key_count(command_name: &str, args: &[&str]) -> Option<usize> {
    match command_name.to_ascii_uppercase().as_str() {
        "DEL" | "UNLINK" => Some(args.len()),
        _ => None,
    }
}

/// Whether a table contains a name (case-insensitive); mirrors the private
/// helper in [`crate::command_policy`] for this module's use.
pub(crate) fn in_table(name: &str, table: &[&str]) -> bool {
    table
        .iter()
        .any(|candidate| name.eq_ignore_ascii_case(candidate))
}

/// Classifies a delete-family command for confirmation purposes: single-key
/// is an ordinary write, multi-key escalates to a confirming destructive.
pub(crate) fn delete_needs_confirmation(command_name: &str, args: &[&str]) -> bool {
    delete_key_count(command_name, args).is_some_and(|count| count > 1)
}

/// The kind the policy table assigns, adjusted for argument-driven
/// escalation (multi-key DEL → [`CommandKind::Destructive`]).
pub(crate) fn effective_kind(command_name: &str, args: &[&str]) -> CommandKind {
    if in_table(command_name, DESTRUCTIVE) {
        return CommandKind::Destructive;
    }
    if delete_needs_confirmation(command_name, args) {
        return CommandKind::Destructive;
    }
    if in_table(command_name, SCRIPT_SERVER_SIDE) {
        return CommandKind::ScriptServerSide;
    }
    if in_table(command_name, READ_ONLY) {
        return CommandKind::ReadOnly;
    }
    if in_table(command_name, WRITE) {
        return CommandKind::Write;
    }
    CommandKind::Unknown
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flushdb_names_the_current_database() {
        let scope = scope_for("FLUSHDB", &[]).unwrap();
        assert_eq!(scope, "all keys in the current database");
    }

    #[test]
    fn flushall_names_the_whole_server() {
        let scope = scope_for("FLUSHALL", &[]).unwrap();
        assert_eq!(scope, "all keys in every database on this server");
    }

    #[test]
    fn swapdb_names_both_databases() {
        let scope = scope_for("SWAPDB", &["0", "1"]).unwrap();
        assert_eq!(scope, "swaps databases 0 and 1 (all keys in both)");
    }

    #[test]
    fn script_flush_names_cached_scripts() {
        let scope = scope_for("SCRIPT FLUSH", &[]).unwrap();
        assert!(scope.contains("Lua scripts"));
    }

    #[test]
    fn multi_key_delete_names_the_count() {
        let c = NeedsConfirmation::for_multi_key_delete(3);
        assert_eq!(c.action, "DEL");
        assert_eq!(c.scope_description, "3 keys");
        assert_eq!(c.confirm_phrase, "DEL");
    }

    #[test]
    fn confirmation_phrase_matches_typed_text() {
        let c = NeedsConfirmation::for_action("FLUSHDB", "all keys in the current database");
        assert!(c.is_satisfied_by("FLUSHDB"));
        assert!(c.is_satisfied_by("  flushdb  "));
        assert!(!c.is_satisfied_by("FLUSHALL"));
        assert!(!c.is_satisfied_by("yes"));
        assert!(!c.is_satisfied_by(""));
    }

    #[test]
    fn single_key_delete_does_not_confirm() {
        assert!(!delete_needs_confirmation("DEL", &["mykey"]));
        assert!(!delete_needs_confirmation("UNLINK", &["mykey"]));
        assert!(!delete_needs_confirmation("SET", &["k", "v"]));
        assert!(!delete_needs_confirmation("GET", &["k"]));
    }

    #[test]
    fn multi_key_delete_confirms() {
        assert!(delete_needs_confirmation("DEL", &["a", "b"]));
        assert!(delete_needs_confirmation("UNLINK", &["a", "b", "c"]));
        assert!(delete_needs_confirmation("del", &["a", "b"]));
    }

    #[test]
    fn zero_key_del_is_not_multi() {
        // `DEL` with no keys is a client-side arity error the console will
        // report, not a destructive sweep.
        assert!(!delete_needs_confirmation("DEL", &[]));
    }

    #[test]
    fn effective_kind_multi_key_del_escalates() {
        assert_eq!(
            effective_kind("DEL", &["a"]),
            CommandKind::Write,
            "single-key DEL stays an ordinary write"
        );
        assert_eq!(
            effective_kind("DEL", &["a", "b", "c"]),
            CommandKind::Destructive,
            "multi-key DEL escalates per D3"
        );
        assert_eq!(
            effective_kind("UNLINK", &["a", "b"]),
            CommandKind::Destructive
        );
        assert_eq!(effective_kind("FLUSHDB", &[]), CommandKind::Destructive);
        assert_eq!(effective_kind("GET", &["k"]), CommandKind::ReadOnly);
        assert_eq!(
            effective_kind("EVAL", &["1", "0"]),
            CommandKind::ScriptServerSide
        );
        assert_eq!(effective_kind("SET", &["k", "v"]), CommandKind::Write);
        assert_eq!(effective_kind("WAT", &[]), CommandKind::Unknown);
    }
}
