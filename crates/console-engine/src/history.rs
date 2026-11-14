//! In-memory per-connection command history (PRD story 39).
//!
//! **Nothing here ever touches a disk.** The history is a plain `Vec<String>`
//! owned by the caller (one per connection tab); when the app exits or the
//! tab closes, it simply ceases to exist. There is no persistence path in
//! this crate at all — that is the guarantee.
//!
//! Bounds and behavior (documented, per PRD story 39):
//! - **Cap: 500 entries.** Pushing beyond 500 evicts the *oldest* entry.
//!   The cap exists so a long session cannot grow memory without bound,
//!   and it never trims what the user just typed.
//! - **Duplicate-consecutive collapse**: submitting the same line twice in
//!   a row (including after up-arrow recall) does not add a second copy.
//!   Non-consecutive repeats stay (normal shell behavior).
//! - **Navigation** ([`HistoryNav`]): up-arrow walks backward (older), each
//!   step returning the entry at the new position; down-arrow walks forward
//!   and `None` at the end means "back at the empty prompt". Reset the nav
//!   when the user submits or edits the recalled line.

/// The per-connection history buffer.
///
/// `MAX_ENTRIES` = 500: roomy enough for a session, bounded enough that a
/// multi-hour session cannot grow memory unbounded. Entries are kept
/// verbatim (the raw input line), in memory only.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct History {
    entries: Vec<String>,
}

/// Maximum retained entries; older entries are evicted on overflow.
pub const MAX_ENTRIES: usize = 500;

impl History {
    /// An empty history.
    pub fn new() -> Self {
        Self::default()
    }

    /// Number of retained entries.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when no entries are retained.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The retained entries, oldest first.
    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    /// Records one submitted input line.
    ///
    /// - Empty or whitespace-only lines are ignored (the console treats them
    ///   as no-ops; they are not history).
    /// - A line identical to the most recent entry is collapsed (no second
    ///   copy), matching redis-cli behavior.
    /// - At the cap, the oldest entry is evicted first.
    pub fn push(&mut self, line: &str) {
        if line.trim().is_empty() {
            return;
        }
        if self.entries.last() == Some(&line.to_string()) {
            return;
        }
        if self.entries.len() >= MAX_ENTRIES {
            // Evict oldest; drain_front in one step.
            self.entries.remove(0);
        }
        self.entries.push(line.to_string());
    }

    /// Starts up-arrow navigation over this history.
    ///
    /// The first `older()` on the returned nav yields the most recent entry.
    pub fn nav(&self) -> HistoryNav {
        HistoryNav {
            len: self.entries.len(),
            position: self.entries.len(),
        }
    }
}

/// Up-arrow / down-arrow navigation state over a [`History`] snapshot.
///
/// The position is an index into the history (`len` = "at the prompt, nothing
/// recalled"; `len - 1` = the newest entry). Navigation returns entry
/// **indices**, and the caller reads the text from the same `History` it
/// navigates — so the nav never holds a second copy of any entry and never
/// outlives a history change (the UI re-creates the nav via
/// [`History::nav`] when the history changes or the user submits).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HistoryNav {
    len: usize,
    position: usize,
}

impl HistoryNav {
    /// Up-arrow: the index of one entry older than the current position,
    /// or `None` when already at the oldest entry (keep showing it).
    pub fn older(&mut self) -> Option<usize> {
        if self.position == 0 {
            return None;
        }
        self.position -= 1;
        Some(self.position)
    }

    /// Down-arrow: the index of one entry newer than the current position,
    /// or `None` when returning to the empty prompt.
    pub fn newer(&mut self) -> Option<usize> {
        if self.position >= self.len {
            return None;
        }
        self.position += 1;
        if self.position >= self.len {
            return None;
        }
        Some(self.position)
    }

    /// The current position (`history.len()` = "at the prompt").
    pub fn position(&self) -> usize {
        self.position
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_and_entries() {
        let mut h = History::new();
        h.push("GET foo");
        h.push("SET foo bar");
        assert_eq!(
            h.entries(),
            &["GET foo".to_string(), "SET foo bar".to_string()]
        );
        assert_eq!(h.len(), 2);
        assert!(!h.is_empty());
    }

    #[test]
    fn empty_and_whitespace_lines_are_ignored() {
        let mut h = History::new();
        h.push("");
        h.push("   ");
        h.push("\t");
        assert!(h.is_empty());
    }

    #[test]
    fn consecutive_duplicates_collapse() {
        let mut h = History::new();
        h.push("GET foo");
        h.push("GET foo");
        assert_eq!(h.len(), 1);
        // Non-consecutive repeat stays.
        h.push("GET bar");
        h.push("GET foo");
        assert_eq!(h.len(), 3);
    }

    #[test]
    fn cap_evicts_oldest() {
        let mut h = History::new();
        for i in 0..(MAX_ENTRIES + 10) {
            h.push(&format!("cmd {i}"));
        }
        assert_eq!(h.len(), MAX_ENTRIES);
        assert_eq!(h.entries().first().unwrap(), "cmd 10");
        assert_eq!(
            h.entries().last().unwrap(),
            &format!("cmd {}", MAX_ENTRIES + 9)
        );
    }

    #[test]
    fn nav_up_then_down_round_trip() {
        let mut h = History::new();
        for cmd in ["one", "two", "three"] {
            h.push(cmd);
        }
        let mut nav = h.nav();
        assert_eq!(
            nav.older(),
            Some(2),
            "first up-arrow recalls the newest entry"
        );
        assert_eq!(h.entries()[2], "three");
        assert_eq!(nav.older(), Some(1));
        assert_eq!(h.entries()[1], "two");
        assert_eq!(nav.older(), Some(0));
        assert_eq!(h.entries()[0], "one");
        assert_eq!(nav.older(), None, "already at the oldest entry");
        assert_eq!(nav.position(), 0);
        assert_eq!(nav.newer(), Some(1));
        assert_eq!(nav.newer(), Some(2));
        assert_eq!(
            nav.newer(),
            None,
            "down-arrow past the newest returns to the prompt"
        );
        assert_eq!(nav.newer(), None, "and stays there");
        assert_eq!(nav.position(), 3, "position == len means the empty prompt");
    }

    #[test]
    fn nav_on_empty_history_yields_nothing() {
        let mut nav = History::new().nav();
        assert_eq!(nav.older(), None);
        assert_eq!(nav.newer(), None);
        assert_eq!(nav.position(), 0);
    }

    #[test]
    fn history_and_nav_are_plain_memory() {
        // Type-level guarantee documentation: History owns its strings and
        // exposes no serialization, no file path, no write-to-disk API.
        let h = History::new();
        let _: Vec<String> = h.entries().to_vec();
    }
}
