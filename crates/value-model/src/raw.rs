//! Byte-faithful raw value types.
//!
//! Everything in this module treats value bytes as **opaque**: no code path
//! here converts, re-encodes, or normalizes them. The one lossy conversion in
//! the whole crate ([`RawBytes::to_lossy_display`]) is display-only and
//! documented loudly. This is the foundation of PLAN.md section 4 ("values
//! are bytes, views are views") and PRD acceptance 10.

use std::borrow::Cow;
use std::fmt;
use std::str::Utf8Error;

/// An opaque, byte-faithful blob: the exact bytes Redis sent (or will
/// receive) for a value, key, field name, or list member.
///
/// The wrap exists so that byte-preserving vs. lossy operations are visible
/// in the type system: there is no `Display`/`ToString` and no `Deref` to
/// `str`, so the only ways out are the explicit methods below.
#[derive(Clone, Default, PartialEq, Eq, Hash)]
pub struct RawBytes(Vec<u8>);

impl RawBytes {
    /// Wraps a byte vector without inspecting it.
    pub fn new(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }

    /// The exact bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    /// Consumes the wrapper, returning the exact bytes.
    pub fn into_vec(self) -> Vec<u8> {
        self.0
    }

    /// Number of bytes.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// True when there are zero bytes.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// True iff the bytes are valid UTF-8.
    pub fn is_valid_utf8(&self) -> bool {
        std::str::from_utf8(&self.0).is_ok()
    }

    /// Strict borrow as `&str` — succeeds only when the bytes are valid
    /// UTF-8. This is the *only* lossless text access.
    pub fn to_str(&self) -> Result<&str, Utf8Error> {
        std::str::from_utf8(&self.0)
    }

    /// **DISPLAY ONLY — NEVER a save path.**
    ///
    /// Replaces invalid UTF-8 sequences with U+FFFD, so the result can
    /// *differ from the original bytes*. It exists so the UI can show
    /// something human-readable next to the hex view. Feeding the output
    /// back into any edit or save is a data-corruption bug; the editor API
    /// ([`crate::editor`]) is structured so this is impossible: it only
    /// accepts validated UTF-8 edits or explicit byte edits, never
    /// lossy-rendered strings.
    pub fn to_lossy_display(&self) -> Cow<'_, str> {
        String::from_utf8_lossy(&self.0)
    }
}

impl fmt::Debug for RawBytes {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Ok(s) = self.to_str() {
            let preview: String = s.chars().take(24).collect();
            write!(f, "RawBytes({} bytes, {:?})", self.0.len(), preview)
        } else {
            let preview = self
                .0
                .iter()
                .take(8)
                .map(|b| format!("{b:02x}"))
                .collect::<Vec<_>>()
                .join(" ");
            write!(f, "RawBytes({} bytes, hex {} ..)", self.0.len(), preview)
        }
    }
}

impl From<Vec<u8>> for RawBytes {
    fn from(bytes: Vec<u8>) -> Self {
        Self(bytes)
    }
}

impl From<&[u8]> for RawBytes {
    fn from(bytes: &[u8]) -> Self {
        Self(bytes.to_vec())
    }
}

impl From<&str> for RawBytes {
    fn from(s: &str) -> Self {
        Self(s.as_bytes().to_vec())
    }
}

impl From<String> for RawBytes {
    fn from(s: String) -> Self {
        Self(s.into_bytes())
    }
}

/// The Redis type of a value, as reported by `TYPE` (plus the module type
/// name for anything this client does not model).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TypeKind {
    /// `string` — a plain byte string (GET/SET).
    String,
    /// `hash` — field/value pairs.
    Hash,
    /// `list` — ordered byte-string elements.
    List,
    /// `set` — unordered unique byte-string members.
    Set,
    /// `zset` — members with server-parsed scores.
    ZSet,
    /// `stream` — append-only entries with IDs.
    Stream,
    /// `ReJSON-RL` (RedisJSON) module type.
    Json,
    /// Any other type: the module type name as reported by the server.
    Unsupported(String),
}

impl TypeKind {
    /// The human-facing type name (what the UI shows).
    pub fn display_name(&self) -> &str {
        match self {
            TypeKind::String => "string",
            TypeKind::Hash => "hash",
            TypeKind::List => "list",
            TypeKind::Set => "set",
            TypeKind::ZSet => "zset",
            TypeKind::Stream => "stream",
            TypeKind::Json => "JSON",
            TypeKind::Unsupported(name) => name,
        }
    }
}

/// One sorted-set entry: the member's original bytes plus the
/// server-reported score.
///
/// Scores are server-*parsed* numbers, not raw bytes: Redis sends them as
/// ASCII and parses them canonically, so the score is kept as `f64` and
/// re-formatted on save. Byte fidelity applies to members; scores are
/// numeric by Redis's own semantics.
#[derive(Debug, Clone, PartialEq)]
pub struct ZSetEntry {
    /// The member bytes, preserved exactly.
    pub member: RawBytes,
    /// The score as parsed from the server's reply.
    pub score: f64,
}

/// One stream entry: the entry ID bytes plus its flat field/value pairs.
#[derive(Debug, Clone, PartialEq)]
pub struct StreamEntry {
    /// The entry ID (e.g. `1690000000000-1`), preserved as bytes.
    pub id: RawBytes,
    /// Field/value pairs in server order.
    pub fields: Vec<(RawBytes, RawBytes)>,
}

/// A value exactly as Redis delivered it. Every leaf — string bodies, hash
/// field names and values, list items, set members, stream IDs and field
/// names — is [`RawBytes`]; nothing here is decoded, normalized, or
/// re-ordered (container order is the server's reply order).
///
/// There is deliberately no `Unsupported` variant: types this client cannot
/// model are represented by [`crate::unsupported::ReadonlyUnsupported`]
/// instead, so an unmodeled type can never masquerade as editable data.
#[derive(Debug, Clone, PartialEq)]
pub enum RawValue {
    /// A string value: the raw bytes, no encoding assumed.
    String(RawBytes),
    /// A hash: field/value pairs in `HGETALL` reply order.
    Hash(Vec<(RawBytes, RawBytes)>),
    /// A list: items in list order.
    List(Vec<RawBytes>),
    /// A set: members in `SMEMBERS` reply order.
    Set(Vec<RawBytes>),
    /// A sorted set: entries in `ZRANGE ... WITHSCORES` order.
    ZSet(Vec<ZSetEntry>),
    /// A stream: entries in `XRANGE` order.
    Stream(Vec<StreamEntry>),
    /// A RedisJSON document, kept as the serialized bytes the server sent
    /// (`JSON.GET`). The document is never re-serialized into storage.
    Json(RawBytes),
}

impl RawValue {
    /// The type of this value.
    pub fn kind(&self) -> TypeKind {
        match self {
            RawValue::String(_) => TypeKind::String,
            RawValue::Hash(_) => TypeKind::Hash,
            RawValue::List(_) => TypeKind::List,
            RawValue::Set(_) => TypeKind::Set,
            RawValue::ZSet(_) => TypeKind::ZSet,
            RawValue::Stream(_) => TypeKind::Stream,
            RawValue::Json(_) => TypeKind::Json,
        }
    }

    /// True iff **every** byte leaf of the value is valid UTF-8. This is the
    /// gate for the text view; if any leaf is binary, the hex view is
    /// required.
    pub fn is_valid_utf8(&self) -> bool {
        self.leaf_bytes().iter().all(|b| b.is_valid_utf8())
    }

    /// All byte leaves of the value, in deterministic traversal order
    /// (container order, then within-entry order).
    pub(crate) fn leaf_bytes(&self) -> Vec<&RawBytes> {
        let mut leaves = Vec::new();
        match self {
            RawValue::String(bytes) | RawValue::Json(bytes) => leaves.push(bytes),
            RawValue::Hash(pairs) => {
                for (field, value) in pairs {
                    leaves.push(field);
                    leaves.push(value);
                }
            }
            RawValue::List(items) | RawValue::Set(items) => {
                leaves.extend(items.iter());
            }
            RawValue::ZSet(entries) => {
                for entry in entries {
                    leaves.push(&entry.member);
                }
            }
            RawValue::Stream(entries) => {
                for entry in entries {
                    leaves.push(&entry.id);
                    for (field, value) in &entry.fields {
                        leaves.push(field);
                        leaves.push(value);
                    }
                }
            }
        }
        leaves
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_bytes_roundtrip_identity() {
        let bytes = vec![0xC3, 0x28, 0xF5, 0x00, 0xEF, 0xBB, 0xBF];
        let raw = RawBytes::new(bytes.clone());
        assert_eq!(raw.as_bytes(), bytes.as_slice());
        assert_eq!(raw.clone().into_vec(), bytes);
    }

    #[test]
    fn raw_bytes_utf8_access_is_strict() {
        let valid = RawBytes::from("héllo 🦀");
        assert!(valid.is_valid_utf8());
        assert_eq!(valid.to_str().unwrap(), "héllo 🦀");

        // 0xC3 0x28 is invalid UTF-8 (continuation without lead).
        let invalid = RawBytes::from(vec![0xC3, 0x28]);
        assert!(!invalid.is_valid_utf8());
        assert!(invalid.to_str().is_err());
    }

    #[test]
    fn lossy_display_differs_but_original_survives() {
        let invalid = RawBytes::from(vec![0xC3, 0x28, b'a']);
        // The lossy display replaces the bad sequence with U+FFFD ...
        let display = invalid.to_lossy_display();
        assert!(display.contains('\u{FFFD}'));
        // ... while the original bytes are untouched.
        assert_eq!(invalid.as_bytes(), &[0xC3, 0x28, b'a']);
    }

    #[test]
    fn embedded_nul_is_utf8_but_not_text_reliable() {
        // NUL is valid UTF-8; byte preservation must keep it.
        let raw = RawBytes::from(vec![b'a', 0x00, b'b']);
        assert!(raw.is_valid_utf8());
        assert_eq!(raw.to_str().unwrap(), "a\0b");
        assert_eq!(raw.as_bytes(), &[b'a', 0x00, b'b']);
    }

    #[test]
    fn kind_matches_shape() {
        assert_eq!(
            RawValue::String(RawBytes::from("x")).kind(),
            TypeKind::String
        );
        assert_eq!(
            RawValue::ZSet(vec![ZSetEntry {
                member: RawBytes::from("m"),
                score: 1.5
            }])
            .kind(),
            TypeKind::ZSet
        );
        assert_eq!(
            TypeKind::Unsupported("graph".into()).display_name(),
            "graph"
        );
    }

    #[test]
    fn container_utf8_check_covers_all_leaves() {
        let hash = RawValue::Hash(vec![
            (RawBytes::from("k"), RawBytes::from("v")),
            (RawBytes::from("bin"), RawBytes::from(vec![0xF5])),
        ]);
        assert!(!hash.is_valid_utf8());

        let clean = RawValue::Hash(vec![
            (RawBytes::from("k"), RawBytes::from("v")),
            (RawBytes::from("é"), RawBytes::from("🦀")),
        ]);
        assert!(clean.is_valid_utf8());
    }
}
