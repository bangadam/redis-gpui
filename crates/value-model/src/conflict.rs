//! Honest conflict model for concurrent modification (PRD story 30,
//! acceptance 9, PLAN.md decision F5).
//!
//! This module is **data-only**: it says what the save path *can* promise
//! per type and what the UI must disclose. The actual `WATCH/MULTI` dance
//! happens in `redis-io`; this crate never performs I/O.
//!
//! The split follows Redis's real capabilities:
//!
//! - **Strings** can be saved atomically with compare-and-swap semantics
//!   (`WATCH key` + `GET`-compare + `SET` inside `MULTI/EXEC`), so the save
//!   path can detect that someone else changed the value since we read it.
//! - **Containers** (hash/list/set/zset) have no server-side facility that
//!   detects a whole-container concurrent modification: `WATCH` on the key
//!   detects *any* write to the key, but the client cannot atomically
//!   compare-and-set the full container contents without a race window it
//!   cannot close client-side. Rather than overpromise, the UI discloses
//!   the limit and saves are last-writer-wins with a warning.

use crate::raw::TypeKind;

/// What the save path can honestly promise for one value type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConflictModel {
    /// The save path will use `WATCH/MULTI` (redis-io) and surface a
    /// conflict instead of silently overwriting when the key changed since
    /// it was read. Strings (and JSON-as-string) support this.
    CasSupported,
    /// Redis cannot detect concurrent modification of the whole container
    /// for this type. Saves are last-writer-wins; the UI must show the
    /// disclosure text ([`ConflictModel::disclosure_text`]) so the user
    /// knows a save might stomp someone else's change.
    NoCasDisclosed,
}

impl ConflictModel {
    /// The conflict model for a type.
    pub fn for_type(kind: &TypeKind) -> Self {
        match kind {
            // Strings compare byte-for-byte under WATCH; the JSON module
            // type is also a single string slot server-side, so the same
            // CAS works (JSON.SET on a WATCHed key aborts if changed).
            TypeKind::String | TypeKind::Json => ConflictModel::CasSupported,
            // Containers: no honest whole-value CAS exists.
            TypeKind::Hash | TypeKind::List | TypeKind::Set | TypeKind::ZSet | TypeKind::Stream => {
                ConflictModel::NoCasDisclosed
            }
            // Unsupported types are read-only; there is no save path to
            // conflict about.
            TypeKind::Unsupported(_) => ConflictModel::NoCasDisclosed,
        }
    }

    /// The user-facing disclosure shown before saving a type without
    /// conflict detection. Honest by construction: it names the limit
    /// instead of promising protection.
    pub fn disclosure_text(&self) -> Option<&'static str> {
        match self {
            ConflictModel::CasSupported => None,
            ConflictModel::NoCasDisclosed => Some(
                "Redis cannot detect concurrent modification for this value type. \
                 If another client changed this key since you opened it, saving \
                 will overwrite their change (last writer wins).",
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_support_cas() {
        assert_eq!(
            ConflictModel::for_type(&TypeKind::String),
            ConflictModel::CasSupported
        );
        assert_eq!(
            ConflictModel::for_type(&TypeKind::Json),
            ConflictModel::CasSupported
        );
        assert!(ConflictModel::for_type(&TypeKind::String)
            .disclosure_text()
            .is_none());
    }

    #[test]
    fn containers_disclose_no_cas() {
        for kind in [
            TypeKind::Hash,
            TypeKind::List,
            TypeKind::Set,
            TypeKind::ZSet,
            TypeKind::Stream,
        ] {
            let model = ConflictModel::for_type(&kind);
            assert_eq!(model, ConflictModel::NoCasDisclosed);
            let text = model.disclosure_text().expect("disclosure text required");
            assert!(
                text.contains("cannot detect"),
                "disclosure must name the limit"
            );
        }
    }

    #[test]
    fn unsupported_types_are_not_editable_so_not_cas() {
        let model = ConflictModel::for_type(&TypeKind::Unsupported("graph".into()));
        assert_eq!(model, ConflictModel::NoCasDisclosed);
        assert!(model.disclosure_text().is_some());
    }
}
