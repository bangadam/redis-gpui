//! Key TYPE filtering: client-side fallback + server-side passthrough.
//!
//! # Design choices (documented, as required)
//!
//! - **Two layers, one enum.** [`TypeFilter`] describes *what* the user
//!   wants; the two mechanisms that satisfy it are explicitly different:
//!   - **Server-side** (`SCAN ... TYPE <t>`, Redis ≥ 6): the app
//!     attaches it to the scan engine via
//!     [`crate::scan::KeyScan::with_type`], and the connection layer
//!     passes the TYPE argument through to SCAN. Cheaper (the server
//!     never ships non-matching keys) but needs a version/behavior
//!     probe (see `version_supported`).
//!   - **Client-side fallback** ([`matches`]): the app filters rows it
//!     already holds, after per-key `TYPE` probes (it already probes
//!     on selection; a filtered view probes the keys it lists). Works
//!     everywhere, costs one TYPE round trip per listed key.
//! - **The client filter is trivial on purpose:** raw string equality
//!   against Redis's lowercase type names (`string`, `list`, `set`,
//!   `zset`, `hash`, `stream`). The server never returns mixed case,
//!   and being strict (no case folding, no synonyms like "SortedSet")
//!   means a surprising type string from a module (e.g. RedisJSON's
//!   `ReJSON-RL`) just doesn't match any preset filter — it stays
//!   visible under `All` and under `Other` if the app chooses to probe
//!   for it.
//! - **Server-side TYPE is a hint, not a guarantee** in the same way
//!   COUNT is: Redis's SCAN may return keys whose type was *deleted and
//!   recreated between* the scan and the type check. The client filter
//!   remains the source of truth for what is rendered; server-side TYPE
//!   is an optimization that shrinks traffic.

/// The preset type filters the UI offers (PRD story 15). Redis's own
/// lowercase TYPE names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum TypeFilter {
    /// No filtering: every key.
    #[default]
    All,
    /// `string` keys.
    String,
    /// `list` keys.
    List,
    /// `set` keys.
    Set,
    /// `zset` (sorted set) keys.
    ZSet,
    /// `hash` keys.
    Hash,
    /// `stream` keys.
    Stream,
    /// Not one of the presets (client-side: matches only unknown type
    /// strings, e.g. module types like `ReJSON-RL`).
    Other,
}

impl TypeFilter {
    /// The server-side `TYPE` argument for `SCAN ... TYPE`:
    /// `None` for [`TypeFilter::All`] (do not filter), the type name
    /// otherwise.
    ///
    /// [`TypeFilter::Other`] has no server-side spelling — it *means*
    /// "none of the above"; a SCAN cannot ask for that. It returns
    /// `None` (no server narrowing) so the app must apply the
    /// client-side filter for it. Documented, not guessed.
    pub fn scan_type_arg(&self) -> Option<&'static [u8]> {
        match self {
            TypeFilter::All | TypeFilter::Other => None,
            TypeFilter::String => Some(b"string"),
            TypeFilter::List => Some(b"list"),
            TypeFilter::Set => Some(b"set"),
            TypeFilter::ZSet => Some(b"zset"),
            TypeFilter::Hash => Some(b"hash"),
            TypeFilter::Stream => Some(b"stream"),
        }
    }
}

/// Client-side fallback: does `key_type` (the raw server TYPE reply
/// string) pass `filter`?
///
/// - [`TypeFilter::All`] passes everything.
/// - Presets match exactly (case-sensitive: the server replies in
///   lowercase; anything else is treated as not-this-type).
/// - [`TypeFilter::Other`] passes exactly the strings that are not one
///   of the presets (module types).
pub fn matches(key_type: &str, filter: TypeFilter) -> bool {
    match filter {
        TypeFilter::All => true,
        TypeFilter::Other => !is_preset(key_type),
        TypeFilter::String => key_type == "string",
        TypeFilter::List => key_type == "list",
        TypeFilter::Set => key_type == "set",
        TypeFilter::ZSet => key_type == "zset",
        TypeFilter::Hash => key_type == "hash",
        TypeFilter::Stream => key_type == "stream",
    }
}

/// Whether the type string is one of the six core Redis types.
fn is_preset(key_type: &str) -> bool {
    matches!(
        key_type,
        "string" | "list" | "set" | "zset" | "hash" | "stream"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_match_their_type_exactly() {
        assert!(matches("string", TypeFilter::String));
        assert!(matches("hash", TypeFilter::Hash));
        assert!(matches("zset", TypeFilter::ZSet));
        assert!(matches("stream", TypeFilter::Stream));
        // Cross-type: no match.
        assert!(!matches("string", TypeFilter::Hash));
        assert!(!matches("hash", TypeFilter::String));
        // A preset filter rejects module and unknown types.
        assert!(!matches("ReJSON-RL", TypeFilter::String));
        assert!(!matches("none", TypeFilter::String));
    }

    #[test]
    fn all_passes_everything() {
        for t in ["string", "hash", "ReJSON-RL", "none", "weird"] {
            assert!(matches(t, TypeFilter::All), "{t} must pass All");
        }
    }

    #[test]
    fn other_matches_non_preset_types_only() {
        assert!(matches("ReJSON-RL", TypeFilter::Other));
        assert!(matches("graph", TypeFilter::Other));
        assert!(!matches("string", TypeFilter::Other));
        assert!(!matches("zset", TypeFilter::Other));
    }

    #[test]
    fn matching_is_case_sensitive_by_design() {
        // The server replies lowercase; case-folded or synonym spellings
        // are NOT matches (documented decision — strictness surfaces
        // module types instead of silently absorbing them).
        assert!(!matches("String", TypeFilter::String));
        assert!(!matches("sortedset", TypeFilter::ZSet));
    }

    #[test]
    fn scan_type_arg_passes_the_server_name() {
        assert_eq!(TypeFilter::All.scan_type_arg(), None);
        assert_eq!(
            TypeFilter::String.scan_type_arg(),
            Some(b"string".as_slice())
        );
        assert_eq!(TypeFilter::ZSet.scan_type_arg(), Some(b"zset".as_slice()));
        // Other has no server-side spelling: no narrowing.
        assert_eq!(TypeFilter::Other.scan_type_arg(), None);
    }
}
