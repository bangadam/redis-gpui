//! Cancellation groundwork: a cooperative stop token plus a generation
//! guard (PLAN.md §5, acceptance 2's backbone).
//!
//! **The two halves and why both:**
//!
//! - **The token** stops *work*: the SCAN loop checks
//!   [`CancellationToken::is_cancelled`] between batches, so a cancel takes
//!   effect at the next batch boundary (worst case one in-flight round
//!   trip) and no further `SCAN` commands are issued. That makes
//!   "delivery stops promptly" (acceptance 2) true at the engine level.
//! - **The generation** stops *deliveries*: even after the loop exits, the
//!   caller may already hold a completed in-flight batch (or it may still
//!   resolve a moment later). Every [`crate::scan::ScanPage`] carries the
//!   generation it was produced under; the app drops any page whose
//!   generation is not *current*. A stale batch can therefore never be
//!   rendered even if it races the cancel.
//!
//! **How the app should use this (Phase 2 wiring):**
//!
//! 1. When a refresh/search starts, `next_generation()` the session's
//!    [`SearchGeneration`] counter and create a
//!    [`CancellationToken`] from it.
//! 2. Spawn the scan future on GPUI's background executor, delivering
//!    each page via the event channel tagged with its generation.
//! 3. On cancel (user typed a new pattern, closed the tab, disconnect):
//!    call `token.cancel()`.
//! 4. On every incoming page, check
//!    `generation.is_current(page.generation)` before applying it.
//!
//! Semantics are proven by unit tests in this module.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

/// The shared stop flag + generation identity of one scan run.
#[derive(Clone)]
pub struct CancellationToken {
    inner: Arc<Inner>,
}

struct Inner {
    cancelled: AtomicBool,
    generation: u64,
}

impl CancellationToken {
    /// Create a fresh token for `generation`.
    pub fn new(generation: u64) -> Self {
        Self {
            inner: Arc::new(Inner {
                cancelled: AtomicBool::new(false),
                generation,
            }),
        }
    }

    /// Request cancellation. Takes effect at the next batch boundary;
    /// idempotent.
    pub fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::SeqCst);
    }

    /// The generation this token belongs to.
    pub fn generation(&self) -> u64 {
        self.inner.generation
    }

    /// Whether [`CancellationToken::cancel`] has been called.
    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }
}

impl std::fmt::Debug for CancellationToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CancellationToken")
            .field("generation", &self.inner.generation)
            .field("cancelled", &self.is_cancelled())
            .finish()
    }
}

/// Monotonic generation counter owned by the caller (e.g. the app's
/// connection session entity).
///
/// Each new scan run gets a strictly larger generation than every previous
/// run, so "older than current" is a total order and the guard is a single
/// integer comparison.
#[derive(Debug)]
pub struct SearchGeneration {
    counter: Arc<AtomicU64>,
}

impl SearchGeneration {
    /// A counter starting at generation 0.
    pub fn new() -> Self {
        Self {
            counter: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Advance and return the next generation.
    ///
    /// Call once per new scan run; the returned value is the current
    /// generation thereafter.
    pub fn next_generation(&self) -> u64 {
        self.counter.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// The current generation (without advancing).
    pub fn current(&self) -> u64 {
        self.counter.load(Ordering::SeqCst)
    }

    /// The guard: is `g` the current generation?
    ///
    /// Rejects results from any earlier run. `g > current` cannot occur
    /// under the single-counter protocol but is rejected anyway (defensive:
    /// a misbehaving producer).
    pub fn is_current(&self, g: u64) -> bool {
        g == self.current()
    }
}

impl Default for SearchGeneration {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_starts_uncancelled_and_cancel_is_idempotent() {
        let token = CancellationToken::new(7);
        assert_eq!(token.generation(), 7);
        assert!(!token.is_cancelled());
        token.cancel();
        token.cancel(); // idempotent
        assert!(token.is_cancelled());
    }

    #[test]
    fn clones_share_the_cancel_flag() {
        let token = CancellationToken::new(1);
        let twin = token.clone();
        twin.cancel();
        assert!(token.is_cancelled());
        assert!(twin.is_cancelled());
    }

    #[test]
    fn generation_advances_monotonically() {
        let gen = SearchGeneration::new();
        assert_eq!(gen.current(), 0);
        assert_eq!(gen.next_generation(), 1);
        assert_eq!(gen.current(), 1);
        assert_eq!(gen.next_generation(), 2);
        assert!(gen.next_generation() > 2);
    }

    #[test]
    fn guard_rejects_stale_and_unknown_generations() {
        let gen = SearchGeneration::new();
        let g1 = gen.next_generation();
        let g2 = gen.next_generation();
        assert!(gen.is_current(g2)); // current run: accepted
        assert!(!gen.is_current(g1)); // one run ago: stale, rejected
        assert!(!gen.is_current(0)); // never issued: rejected
        assert!(!gen.is_current(99)); // not yet issued: rejected (defensive)
    }
}
