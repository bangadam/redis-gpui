//! Console result → display model.
//!
//! Takes a `redis_io::connection::Reply` and produces a plain-text
//! [`ConsoleOutput`] the console panel renders line-by-line. Byte safety
//! (PRD story 42: no key/value bytes in logs) is preserved here: bulk
//! payloads that are not valid UTF-8 render as a **hex preview with a
//! label**, never as lossy replacement characters.
//!
//! Rendering rules (documented, matching redis-cli's console feel):
//! - `Bulk` — the bytes as-is when valid UTF-8; otherwise
//!   `(non-UTF-8, N bytes: 41 42 …)` — hex preview of the first bytes.
//! - `Array` — numbered lines `1) item`, `2) item`, recursively.
//! - `Int` / `Double` / `Boolean` / `Okay` — plain text (`OK`, `true`).
//! - `Nil` — `(nil)`.
//! - `Map` — `key: value` lines.
//! - `Other` — the label as-is.
//! - **Truncation cap: 200 lines.** An array (or map, or nested structure)
//!   that renders to more than 200 lines is cut at 200 and followed by
//!   `… N more lines`. The cap keeps the panel responsive on huge replies
//!   (e.g. `KEYS *` on the million-key fixture); it caps *display*, not
//!   data — the browse pane is the tool for full inspection.
//!
//! [`format_write_outcome`] renders the tri-state write result; for
//! `OutcomeUnknown` the message is the honest PRD-acceptance-3 wording and
//! deliberately does **not** suggest retrying.

use redis_io::connection::Reply;
use redis_io::outcome::WriteOutcome;

/// How the console styles a rendered result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputKind {
    /// Successful command result (green-ish in the UI).
    Success,
    /// Server-side / client-side error (red-ish).
    Error,
    /// Informational output: refusals, declines, unknown outcomes.
    Info,
}

/// A rendered console result: kind + ready-to-display text lines.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConsoleOutput {
    /// Which style the panel applies.
    pub kind: OutputKind,
    /// The rendered lines (no trailing newline).
    pub lines: Vec<String>,
}

/// Maximum rendered lines per result before truncation (see module docs).
pub const MAX_LINES: usize = 200;

/// Renders a server [`Reply`] into console display lines.
pub fn render_reply(reply: &Reply) -> ConsoleOutput {
    let mut lines = Vec::new();
    let mut hidden = 0usize;
    render_into(reply, &mut lines, &mut hidden, 0);
    let kind = if reply_is_error_shaped(reply) {
        OutputKind::Error
    } else {
        OutputKind::Success
    };
    finish(lines, hidden, kind)
}

/// Renders a refusal/decline/notice as an Info output.
pub fn render_info(message: &str) -> ConsoleOutput {
    ConsoleOutput {
        kind: OutputKind::Info,
        lines: message.lines().map(str::to_string).collect(),
    }
}

/// Renders a write command's tri-state outcome (redis-io's
/// [`WriteOutcome`]).
///
/// - `Acknowledged` → `OK` (kind Success).
/// - `Failed` → the definitive error message (kind Error).
/// - `OutcomeUnknown` → the honest "check before retrying" wording (kind
///   Info) — PRD acceptance 3: never claim the write failed, never claim it
///   worked, never resend.
pub fn format_write_outcome(outcome: &WriteOutcome) -> ConsoleOutput {
    match outcome {
        WriteOutcome::Acknowledged => ConsoleOutput {
            kind: OutputKind::Success,
            lines: vec!["OK".to_string()],
        },
        WriteOutcome::Failed(err) => ConsoleOutput {
            kind: OutputKind::Error,
            lines: vec![err.to_string()],
        },
        WriteOutcome::OutcomeUnknown(_) => ConsoleOutput {
            kind: OutputKind::Info,
            lines: vec![
                "outcome unknown — the write may or may not have been applied; check before \
                 retrying"
                    .to_string(),
            ],
        },
    }
}

/// Appends the truncation marker when needed and builds the output.
/// `hidden` counts lines dropped to the budget during rendering.
fn finish(mut lines: Vec<String>, hidden: usize, kind: OutputKind) -> ConsoleOutput {
    if hidden > 0 {
        lines.push(format!("… {hidden} more lines"));
    }
    ConsoleOutput { kind, lines }
}

/// Errors arrive as `Reply::Other` labels from the adapter when the server
/// replies with a RESP error; the definitive-failure path goes through
/// [`WriteOutcome::Failed`] instead. A reply output is Success unless it is
/// an `Other` carrying a server-error label.
fn reply_is_error_shaped(reply: &Reply) -> bool {
    match reply {
        Reply::Other(label) => {
            label.starts_with("ERR")
                || label.starts_with("error")
                || label.starts_with("WRONGTYPE")
                || label.starts_with("MOVED")
                || label.starts_with("ASK")
                || label.starts_with("NOAUTH")
                || label.starts_with("NOPERM")
        }
        _ => false,
    }
}

/// Recursive renderer; `depth` bounds pathological nesting. `hidden`
/// counts lines that did not fit the budget (reported by the truncation
/// marker).
fn render_into(reply: &Reply, lines: &mut Vec<String>, hidden: &mut usize, depth: usize) {
    if depth > 8 {
        *hidden += 1;
        return;
    }
    let push = |lines: &mut Vec<String>, hidden: &mut usize, line: String| {
        if lines.len() < MAX_LINES {
            lines.push(line);
        } else {
            *hidden += 1;
        }
    };
    match reply {
        Reply::Nil => push(lines, hidden, "(nil)".to_string()),
        Reply::Int(i) => push(lines, hidden, format!("(integer) {i}")),
        Reply::Double(d) => push(lines, hidden, format!("(double) {d}")),
        Reply::Boolean(b) => push(lines, hidden, b.to_string()),
        Reply::Okay => push(lines, hidden, "OK".to_string()),
        Reply::Bulk(bytes) => push(lines, hidden, render_bulk(bytes)),
        Reply::Array(items) => {
            for item in items {
                // Render the item into its own line set so numbering
                // applies to top-level entries.
                let mut sub = Vec::new();
                render_into(item, &mut sub, hidden, depth + 1);
                let n = lines.len() + 1;
                match sub.first() {
                    Some(first) => {
                        push(lines, hidden, format!("{n}) {first}"));
                        for extra in sub.iter().skip(1) {
                            push(lines, hidden, format!("   {extra}"));
                        }
                    }
                    None => push(lines, hidden, format!("{n}) (nil)")),
                }
            }
        }
        Reply::Map(entries) => {
            for (key, value) in entries {
                let key_text = match key {
                    Reply::Bulk(bytes) => render_bulk(bytes),
                    other => {
                        let mut sub = Vec::new();
                        render_into(other, &mut sub, hidden, depth + 1);
                        sub.first().cloned().unwrap_or_else(|| "(nil)".to_string())
                    }
                };
                let mut sub = Vec::new();
                render_into(value, &mut sub, hidden, depth + 1);
                let value_text = sub.first().cloned().unwrap_or_else(|| "(nil)".to_string());
                push(lines, hidden, format!("{key_text}: {value_text}"));
            }
        }
        Reply::Other(label) => push(lines, hidden, label.clone()),
    }
}

/// One bulk payload as display text: valid UTF-8 verbatim, else a hex
/// preview with byte count (first 16 bytes as hex pairs).
fn render_bulk(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(text) => {
            if text.is_empty() {
                "(empty string)".to_string()
            } else {
                text.to_string()
            }
        }
        Err(_) => {
            let preview: Vec<String> = bytes.iter().take(16).map(|b| format!("{b:02X}")).collect();
            format!("(non-UTF-8, {} bytes: {})", bytes.len(), preview.join(" "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use redis_io::connection::Command;
    use redis_io::outcome::{DefinitiveError, DefinitiveErrorKind, DisconnectContext};

    fn lines_of(reply: &Reply) -> Vec<String> {
        render_reply(reply).lines
    }

    #[test]
    fn bulk_valid_utf8_renders_verbatim() {
        assert_eq!(lines_of(&Reply::Bulk(b"hello".to_vec())), vec!["hello"]);
    }

    #[test]
    fn bulk_invalid_utf8_renders_hex_preview() {
        let out = lines_of(&Reply::Bulk(vec![0xFF, 0xFE, 0x00]));
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("non-UTF-8"), "{}", out[0]);
        assert!(out[0].contains("3 bytes"), "{}", out[0]);
        assert!(out[0].contains("FF FE 00"), "{}", out[0]);
    }

    #[test]
    fn bulk_hex_preview_caps_at_sixteen_bytes() {
        let bytes: Vec<u8> = (0u8..32).collect();
        let out = lines_of(&Reply::Bulk(bytes));
        // 16 hex pairs in the preview, not 32.
        let hex_count = out[0].split_whitespace().count();
        assert!(hex_count <= 4 + 16 + 1, "hex tokens bounded: {hex_count}");
    }

    #[test]
    fn empty_bulk_renders_empty_string_marker() {
        assert_eq!(lines_of(&Reply::Bulk(Vec::new())), vec!["(empty string)"]);
    }

    #[test]
    fn nil_renders_nil_marker() {
        assert_eq!(lines_of(&Reply::Nil), vec!["(nil)"]);
    }

    #[test]
    fn int_renders_with_integer_marker() {
        assert_eq!(lines_of(&Reply::Int(42)), vec!["(integer) 42"]);
    }

    #[test]
    fn double_and_boolean_render() {
        assert_eq!(lines_of(&Reply::Double(1.5)), vec!["(double) 1.5"]);
        assert_eq!(lines_of(&Reply::Boolean(true)), vec!["true"]);
        assert_eq!(lines_of(&Reply::Boolean(false)), vec!["false"]);
    }

    #[test]
    fn okay_renders_ok() {
        assert_eq!(lines_of(&Reply::Okay), vec!["OK"]);
    }

    #[test]
    fn array_renders_numbered_lines() {
        let reply = Reply::Array(vec![
            Reply::Bulk(b"one".to_vec()),
            Reply::Bulk(b"two".to_vec()),
            Reply::Int(3),
        ]);
        assert_eq!(
            lines_of(&reply),
            vec![
                "1) one".to_string(),
                "2) two".to_string(),
                "3) (integer) 3".to_string()
            ]
        );
    }

    #[test]
    fn nested_array_indents_continuation_lines() {
        // redis-cli renders nested arrays with the nested item's own
        // numbering on the continuation lines, aligned under the parent.
        let reply = Reply::Array(vec![Reply::Array(vec![
            Reply::Bulk(b"a".to_vec()),
            Reply::Bulk(b"b".to_vec()),
        ])]);
        assert_eq!(
            lines_of(&reply),
            vec!["1) 1) a".to_string(), "   2) b".to_string()]
        );
    }

    #[test]
    fn map_renders_key_value_lines() {
        let reply = Reply::Map(vec![(
            Reply::Bulk(b"maxmemory".to_vec()),
            Reply::Bulk(b"0".to_vec()),
        )]);
        assert_eq!(lines_of(&reply), vec!["maxmemory: 0"]);
    }

    #[test]
    fn other_renders_label_verbatim() {
        assert_eq!(
            lines_of(&Reply::Other("verbatim-type".to_string())),
            vec!["verbatim-type"]
        );
    }

    #[test]
    fn error_shaped_reply_is_error_kind() {
        let out = render_reply(&Reply::Other("ERR unknown command".to_string()));
        assert_eq!(out.kind, OutputKind::Error);
        assert_eq!(out.lines, vec!["ERR unknown command"]);
    }

    #[test]
    fn big_array_truncates_with_more_lines_marker() {
        let items: Vec<Reply> = (0..(MAX_LINES + 50))
            .map(|i| Reply::Bulk(format!("key{i}").into_bytes()))
            .collect();
        let out = render_reply(&Reply::Array(items));
        assert_eq!(out.lines.len(), MAX_LINES + 1);
        assert_eq!(out.lines.last().unwrap(), "… 50 more lines");
        assert_eq!(out.lines[0], "1) key0");
    }

    #[test]
    fn exactly_at_cap_does_not_truncate() {
        let items: Vec<Reply> = (0..MAX_LINES)
            .map(|i| Reply::Bulk(format!("k{i}").into_bytes()))
            .collect();
        let out = render_reply(&Reply::Array(items));
        assert_eq!(out.lines.len(), MAX_LINES);
        assert!(!out.lines.last().unwrap().starts_with('…'));
    }

    #[test]
    fn render_info_splits_lines() {
        let out = render_info("line one\nline two");
        assert_eq!(out.kind, OutputKind::Info);
        assert_eq!(out.lines, vec!["line one", "line two"]);
    }

    #[test]
    fn write_outcome_acknowledged_is_ok() {
        let out = format_write_outcome(&WriteOutcome::Acknowledged);
        assert_eq!(out.kind, OutputKind::Success);
        assert_eq!(out.lines, vec!["OK"]);
    }

    #[test]
    fn write_outcome_failed_is_error_with_message() {
        let err = DefinitiveError::new(DefinitiveErrorKind::ServerError, "WRONGTYPE foo");
        let out = format_write_outcome(&WriteOutcome::Failed(err));
        assert_eq!(out.kind, OutputKind::Error);
        assert_eq!(out.lines, vec!["ServerError: WRONGTYPE foo"]);
    }

    #[test]
    fn write_outcome_unknown_is_the_honest_message() {
        let ctx = DisconnectContext {
            in_flight: Command::new("SET", vec![b"k".to_vec(), b"v".to_vec()]),
            reason: None,
        };
        let out = format_write_outcome(&WriteOutcome::OutcomeUnknown(ctx));
        assert_eq!(out.kind, OutputKind::Info);
        assert_eq!(
            out.lines,
            vec![
                "outcome unknown — the write may or may not have been applied; check before \
                 retrying"
                    .to_string()
            ]
        );
        // The message must not claim failure or success, and must not
        // suggest an automatic retry.
        let msg = &out.lines[0];
        assert!(!msg.contains("failed"));
        assert!(!msg.contains("succeeded"));
        assert!(!msg.contains("retrying automatically"));
    }
}
