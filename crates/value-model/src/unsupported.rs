//! Marker for value types this client displays but cannot edit (PRD
//! story 22, acceptance 10).
//!
//! An unsupported type never becomes a [`RawValue`](crate::raw::RawValue)
//! variant — by construction it cannot masquerade as editable data. The
//! browse layer wraps it in this marker so the UI shows it read-only with a
//! clear type name.

/// A value whose Redis type this client does not model: displayed
/// read-only, never editable, never silently converted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadonlyUnsupported {
    /// The module type name exactly as the server reported it (`TYPE`
    /// reply), e.g. `graph`, `timeseries`.
    pub type_name: String,
}

impl ReadonlyUnsupported {
    /// Wraps a server-reported type name.
    pub fn new(type_name: impl Into<String>) -> Self {
        Self {
            type_name: type_name.into(),
        }
    }

    /// The user-facing marker text, e.g. `graph — read-only (unsupported
    /// type)`.
    pub fn marker_text(&self) -> String {
        format!("{} — read-only (unsupported type)", self.type_name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn marker_names_the_type_and_readonly() {
        let marker = ReadonlyUnsupported::new("graph");
        assert_eq!(marker.type_name, "graph");
        let text = marker.marker_text();
        assert!(text.contains("graph"));
        assert!(text.contains("read-only"));
    }
}
