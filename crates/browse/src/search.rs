//! Progressive, cancellable key search over the scan engine (PRD story
//! 14, PLAN phase 3).
//!
//! # Design choices (documented, as required)
//!
//! - **The app owns the loop; this crate owns the state machine.** The
//!   scan engine is pull-based by design (see [`crate::scan`]), and the
//!   driver loop must live where the executor is (GPUI's background
//!   executor, as `src/session.rs` already does for plain browsing).
//!   What this module provides: the pattern semantics (user text →
//!   MATCH glob), the per-keystroke run creation, and the count/state
//!   that keep the search's totals honest (`AtLeast` while searching,
//!   `Exact` only at cursor exhaustion — the same [`KeyCount`] rules).
//!
//!   Concretely, the app drives one run like this:
//!
//!   ```text
//!   let run = search.start(text, &gen);
//!   run.scan.next_page(&mut conn, &run.token).await
//!   - deliver ScanPage through the event channel (flat/tree apply_page
//!     drop it if the generation moved) until Exhausted / Cancelled /
//!     ConnectionError.
//!   ```
//!
//! - **Matching semantics (both modes explicit, never mixed):**
//!   - [`MatchMode::Prefix`] — user types `user:1`, the glob is
//!     `user:1*`: every key starting with the text. This is the
//!     default; Redis key naming is prefix-structured (namespaces),
//!     so prefix is the cheapest useful narrowing.
//!   - [`MatchMode::Contains`] — the glob is `*user:1*`: substring
//!     match anywhere in the key name. Strictly more results, strictly
//!     more server work (Redis matches against the whole glob; a
//!     leading `*` forfeits prefix shortcuts) — offered as an explicit
//!     UI choice, not a silent fallback.
//!   Whatever the user typed that is already a glob (`*`, `?`, `[`,
//!   `]`) is escaped so it matches **literally** — the text box is a
//!   text search, not a glob editor. (Use the raw-pattern scan directly
//!   for glob power-use.)
//! - **Debounce is app policy; restart is the mechanism.** There is no
//!   timer here. The app debounces keystrokes and, per accepted query,
//!   calls [`ProgressiveSearch::start`]: that cancels the previous
//!   token, advances the generation (stale pages die in the guard at
//!   the flat/tree seam) and hands back a fresh engine. A late page
//!   from the older run is dropped by exactly the same two-layer guard
//!   as plain browsing — acceptance 2's machinery, reused.
//! - **`start` never errors on input:** the empty text returns a whole-
//!   keyspace scan (mode is irrelevant; prefix and contains of "" are
//!   both "everything"), because that is what a cleared search box
//!   means: browse all keys, progressively.
//! - **State mirrors the engine:** [`SearchRun::key_count`] /
//!   [`SearchRun::is_exhausted`] / [`SearchRun::pattern`] delegate to
//!   the engine; the app's footer composes them exactly like
//!   `ConnectionSession::count_text` does for browsing.

use crate::cancellation::CancellationToken;
use crate::scan::{KeyCount, KeyScan};
use crate::search::MatchMode::{Contains, Prefix};

/// How user text maps to a MATCH glob.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum MatchMode {
    /// `text*` — keys starting with the text (default).
    #[default]
    Prefix,
    /// `*text*` — keys containing the text anywhere.
    Contains,
}

/// Escape a raw byte string into a MATCH glob that matches it
/// **literally**: `*`, `?`, `[`, `]`, `\` are backslash-escaped.
///
/// Public because [`crate::tree::KeyTree::prefix_pattern`] builds
/// scoped patterns with it — search text and node paths need the same
/// "no accidental globbing" guarantee.
pub fn escape_glob(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    for &b in raw {
        match b {
            b'*' | b'?' | b'[' | b']' | b'\\' => {
                out.push(b'\\');
                out.push(b);
            }
            _ => out.push(b),
        }
    }
    out
}

/// Build the MATCH glob for `text` under `mode`.
///
/// Empty text → `*` (everything) regardless of mode: a cleared search
/// box means "browse all keys".
pub fn pattern_for(text: &[u8], mode: MatchMode) -> Vec<u8> {
    let escaped = escape_glob(text);
    if escaped.is_empty() {
        return b"*".to_vec();
    }
    match mode {
        Prefix => [escaped, b"*".to_vec()].concat(),
        Contains => [b"*".to_vec(), escaped, b"*".to_vec()].concat(),
    }
}

/// One armed search run: the fresh engine + its cancel token, created
/// together so they can never disagree about which generation they run
/// under.
#[derive(Debug)]
pub struct SearchRun {
    /// The scan engine. The app drives `next_page` on its background
    /// executor and delivers pages through the event channel.
    pub scan: KeyScan,
    /// The token for this run; cancelling it stops the engine at the
    /// next batch boundary. The next `start` cancels it automatically.
    pub token: CancellationToken,
}

impl SearchRun {
    /// The MATCH glob this run searches with.
    pub fn pattern(&self) -> Option<&[u8]> {
        self.scan.pattern()
    }

    /// The generation this run runs under.
    pub fn generation(&self) -> u64 {
        self.scan.generation()
    }

    /// Honest count under the run's engine (`AtLeast` while pages may
    /// still come, `Exact` at cursor exhaustion — never DBSIZE).
    pub fn key_count(&self) -> KeyCount {
        self.scan.key_count()
    }

    /// Whether the run's engine can deliver more pages.
    pub fn is_exhausted(&self) -> bool {
        self.scan.is_exhausted()
    }
}

/// The progressive search driver state machine.
///
/// Holds the previous run's token so each new query retires it (the
/// generation counter itself lives with the caller's
/// [`crate::cancellation::SearchGeneration`], matching how
/// `ConnectionSession` owns it for browsing).
#[derive(Debug)]
pub struct ProgressiveSearch {
    mode: MatchMode,
    page_size: usize,
    batch_hint: u64,
    /// The retired-but-not-yet-cancelled token of the previous run, if
    /// any. Kept only to guarantee retirement; the current run's token
    /// lives in the returned [`SearchRun`].
    retired: Option<CancellationToken>,
}

impl ProgressiveSearch {
    /// Create the driver with a matching mode and page geometry.
    pub fn new(mode: MatchMode, page_size: usize, batch_hint: u64) -> Self {
        Self {
            mode,
            page_size: page_size.max(1),
            batch_hint: batch_hint.max(1),
            retired: None,
        }
    }

    /// The active matching mode.
    pub fn mode(&self) -> MatchMode {
        self.mode
    }

    /// Begin a search for `text`: retires any previous run (cancels its
    /// token; its in-flight pages die at the generation guard), takes
    /// the next generation, and returns the fresh engine + token pair.
    ///
    /// The generation counter comes from the caller (`gen`) — the same
    /// shared [`crate::cancellation::SearchGeneration`] the session
    /// uses for browsing, so one guard protects both flows.
    pub fn start(&mut self, text: &[u8], gen: &crate::cancellation::SearchGeneration) -> SearchRun {
        self.retire();
        let generation = gen.next_generation();
        let run = SearchRun {
            scan: KeyScan::with_pattern(
                generation,
                self.page_size,
                self.batch_hint,
                Some(pattern_for(text, self.mode)),
            ),
            token: CancellationToken::new(generation),
        };
        // Track the live run's token so the next start (or an explicit
        // cancel) retires exactly it.
        self.retired = Some(run.token.clone());
        run
    }

    /// Cancel the current run's work (app-initiated stop: user hit
    /// escape, closed the tab, disconnected). Idempotent; the engine
    /// stops issuing SCAN at the next batch boundary. Does not advance
    /// the generation — late pages from the cancelled run are dropped
    /// by the engine's discard-on-cancel + the token's generation
    /// staying current-but-cancelled.
    pub fn cancel(&mut self) {
        self.retire();
    }

    /// Cancel the tracked run's token, if any (shared by `start` and
    /// `cancel`).
    fn retire(&mut self) {
        if let Some(old) = self.retired.take() {
            old.cancel(); // retirement takes effect at a batch boundary
        }
    }

    /// The MATCH glob the next `start(text)` would build (diagnostics;
    /// also handy for showing the user what is being searched).
    pub fn preview_pattern(&self, text: &[u8]) -> Vec<u8> {
        pattern_for(text, self.mode)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefix_mode_builds_prefix_glob() {
        assert_eq!(pattern_for(b"user:1", Prefix), b"user:1*");
        assert_eq!(pattern_for(b"", Prefix), b"*"); // cleared box = all
    }

    #[test]
    fn contains_mode_builds_surrounding_glob() {
        assert_eq!(pattern_for(b"42:cart", Contains), b"*42:cart*");
        assert_eq!(pattern_for(b"", Contains), b"*");
    }

    #[test]
    fn glob_metacharacters_in_text_are_escaped() {
        // The text box searches text, not globs.
        assert_eq!(pattern_for(b"a*", Prefix), b"a\\**");
        assert_eq!(pattern_for(b"100%?", Contains), b"*100%\\?*");
        assert_eq!(pattern_for(b"[a]", Prefix), b"\\[a\\]*");
        assert_eq!(pattern_for(b"back\\", Prefix), b"back\\\\*");
        // escape_glob itself round-trips literals.
        assert_eq!(escape_glob(b"plain:key"), b"plain:key");
    }

    #[test]
    fn start_retires_the_previous_run() {
        let mut search = ProgressiveSearch::new(Prefix, 100, 1000);
        let gen = crate::cancellation::SearchGeneration::new();

        let run1 = search.start(b"user", &gen);
        assert!(!run1.token.is_cancelled());
        assert_eq!(run1.generation(), 1);
        assert_eq!(run1.pattern(), Some(b"user*".to_vec().as_slice()));

        let run2 = search.start(b"session", &gen);
        // The previous run's token was cancelled on start.
        assert!(run1.token.is_cancelled());
        assert!(!run2.token.is_cancelled());
        assert_eq!(run2.generation(), 2);
        assert!(run2.generation() > run1.generation());
        // Generation guard: run1 is now stale.
        assert!(!gen.is_current(run1.generation()));
        assert!(gen.is_current(run2.generation()));
    }

    #[test]
    fn cancel_stops_the_current_run() {
        let mut search = ProgressiveSearch::new(Contains, 10, 10);
        let gen = crate::cancellation::SearchGeneration::new();
        let run = search.start(b"x", &gen);
        assert!(!run.token.is_cancelled());
        search.cancel();
        assert!(run.token.is_cancelled());
        // Generation not advanced: the guard alone retires pages only
        // on a new start; the cancelled engine discards partial pages.
        assert!(gen.is_current(run.generation()));
        search.cancel(); // idempotent
        assert!(run.token.is_cancelled());
    }

    #[test]
    fn empty_text_scans_everything_progressively() {
        let mut search = ProgressiveSearch::new(Prefix, 50, 100);
        let gen = crate::cancellation::SearchGeneration::new();
        let run = search.start(b"", &gen);
        assert_eq!(run.pattern(), Some(b"*".to_vec().as_slice()));
        assert_eq!(run.key_count(), KeyCount::Unknown);
        assert!(!run.is_exhausted());
    }

    #[test]
    fn preview_pattern_reflects_mode() {
        let prefix = ProgressiveSearch::new(Prefix, 1, 1);
        let contains = ProgressiveSearch::new(Contains, 1, 1);
        assert_eq!(prefix.preview_pattern(b"k"), b"k*");
        assert_eq!(contains.preview_pattern(b"k"), b"*k*");
        assert_eq!(prefix.mode(), Prefix);
        assert_eq!(contains.mode(), Contains);
    }
}
