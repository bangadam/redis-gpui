//! Log hygiene: credentials, key names, and values never enter logs.
//!
//! PRD story 42: "no credentials, no key data, and no command results in
//! application logs — log files are safe to share."
//!
//! This module provides the enforcement primitives:
//! - [`Secret`]: a newtype whose `Debug`/`Display` print `[REDACTED]`.
//! - [`redacted_event()`]: build a non-sensitive event string for logging,
//!   with lengths instead of contents.
//!
//! Companion conventions enforced crate-wide (by `Command`'s and `Reply`'s
//! byte-safe `Debug` impls): bulk payloads and command args never render.
//!
//! # Zeroize
//!
//! The `zeroize` crate is deliberately NOT pulled in at Phase 1: the
//! `Secret` values here are short-lived (a password passed from the
//! keyring/prompt to the connect call) and the redaction guarantee we
//! actually promise — nothing reaches logs — doesn't need heap scrubbing
//! to hold. Revisit if secrets start living longer than a connect (e.g. a
//! cached credential pool); then `zeroize` earns its dependency slot.

use std::fmt;

/// A credential value that must never render into logs or debug output.
#[derive(Clone, Default)]
pub struct Secret {
    inner: String,
}

impl Secret {
    /// Wrap a credential string.
    pub fn new(value: String) -> Self {
        Self { inner: value }
    }

    /// Reveal the secret for use (auth handshake). Call sites should pass
    /// it straight through and never log it.
    pub fn reveal(&self) -> &str {
        &self.inner
    }

    /// Length in bytes — usable for logs ("password: 14 bytes").
    pub fn len(&self) -> usize {
        self.inner.len()
    }

    /// Whether the secret is empty.
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("[REDACTED]")
    }
}

impl PartialEq for Secret {
    /// Compare by content (test convenience); equality does not leak.
    fn eq(&self, other: &Self) -> bool {
        self.inner == other.inner
    }
}

impl Eq for Secret {}

/// Build a redacted, log-safe event string.
///
/// `kind` names the event (e.g. `"conn.connect"`); `cmd_name` is the
/// uppercase command name when relevant; `payload_len` is a byte length,
/// not content. Result never contains key or value bytes.
pub fn redacted_event(kind: &str, cmd_name: Option<&str>, payload_len: Option<usize>) -> String {
    let mut s = String::with_capacity(48);
    s.push_str(kind);
    if let Some(c) = cmd_name {
        s.push(' ');
        s.push_str(c);
    }
    if let Some(n) = payload_len {
        s.push_str(&format!(" ({} bytes)", n));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secret_debug_and_display_are_redacted() {
        let s = Secret::new("hunter2-super-secret".into());
        assert_eq!(format!("{s:?}"), "[REDACTED]");
        assert_eq!(format!("{s}"), "[REDACTED]");
        // Nested in structs:
        #[derive(Debug)]
        struct Holder {
            #[allow(dead_code)] // the point is Debug, not reading it
            pw: Secret,
        }
        let h = Holder { pw: s };
        let out = format!("{h:?}");
        assert!(!out.contains("hunter2"));
        assert!(out.contains("[REDACTED]"));
    }

    #[test]
    fn secret_bytes_do_not_appear_in_any_common_formatting() {
        let s = Secret::new("\u{1F9EA}nul\0pass".into());
        for out in [format!("{s:?}"), format!("{s}"), format!("{s:#?}")] {
            assert!(!out.contains('\u{1F9EA}'));
            assert!(!out.contains("pass"));
        }
    }

    #[test]
    fn redacted_event_contains_no_payload() {
        let ev = redacted_event("conn.cmd_done", Some("SET"), Some(512));
        assert_eq!(ev, "conn.cmd_done SET (512 bytes)");
    }

    #[test]
    fn equality_by_content() {
        assert_eq!(Secret::new("a".into()), Secret::new("a".into()));
        assert_ne!(Secret::new("a".into()), Secret::new("b".into()));
    }
}
