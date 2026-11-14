//! Batched `SCAN` pagination: the engine that enumerates keys without ever
//! materializing the whole keyspace (PLAN.md §3, PRD stories 16, 17, 18).
//!
//! # Design choices (documented, as required)
//!
//! - **Trait object, not generics**: the engine drives `&mut dyn
//!   Connection`, matching how the app holds connections (one per tab,
//!   behind a handle). Generic `C: Connection` would monomorphize the
//!   engine into every backend and make `KeyScan` non-storable in a
//!   session struct; the object costs one vtable dispatch per *batch*
//!   (amortized over hundreds of keys — negligible).
//! - **Async without a runtime**: every engine method is a plain
//!   `async fn` taking `&mut dyn Connection`. It never spawns, never
//!   sleeps, and depends on no executor. The app drives it (see
//!   [Driving from GPUI](#driving-from-gpui)) so the same code runs under
//!   tokio in tests and GPUI's background executor in production.
//! - **Pull, don't push**: the engine is a cursor machine. The caller
//!   calls [`KeyScan::next_page`] when it wants another page (user
//!   scrolled, or the progressive loader wants more). Nothing runs until
//!   asked, which *is* the "never tries to fetch everything at once"
//!   guarantee of story 16.
//! - **One page = enough batches to fill `page_size`** (or exhaust the
//!   cursor). Each `SCAN` round trip uses `COUNT = batch_hint` (default
//!   1000). The distinction matters: COUNT is a per-call *hint* the server
//!   may ignore in either direction, so a page may need 1, 3, or N calls.
//! - **Never materializes all keys** (memory proof): the engine holds only
//!   the accumulated keys of the pages *already delivered* plus a cursor.
//!   Memory is O(keys kept), not O(keyspace). A million-key server costs
//!   the engine exactly the keys the UI chose to keep.
//! - **Dedup is part of honesty**: Redis SCAN may return the same element
//!   multiple times (guarantee is completeness, not uniqueness). Keys are
//!   deduplicated on arrival against every key the view has ever kept, so
//!   [counts](KeyCount) are true *distinct-key* counts.
//! - **DBSIZE is never consulted**: counts are exactly what the current
//!   view has physically seen (story 18 — no fabricated totals).
//!
//! # Driving from GPUI
//!
//! The app crate owns a `KeyScan` + a connection handle per tab. On a
//! background executor it calls `next_page(&mut conn, token)` and delivers
//! the returned `ScanPage` back to the session entity via the event
//! channel (PLAN §5: `ScanBatch`), tagging the page with its generation.
//! `next_page` is one `await` chain over `scan_batch` calls, so it parks
//! exactly like any other future on that executor. On
//! [`PageOutcome::Cancelled`] the caller simply stops asking.

use std::collections::HashSet;

use redis_io::connection::{Command, ConnError, Connection, Reply, ScanResult};
use redis_io::outcome::{DefinitiveError, DefinitiveErrorKind};

use crate::cancellation::CancellationToken;

/// Default `COUNT` hint per `SCAN` call (PRD story 16's "small batches").
pub const DEFAULT_BATCH_HINT: u64 = 1000;

/// Bound on consecutive `SCAN` batches that return no *new* (post-dedup)
/// keys before the engine stops asking. See the anti-stall note in
/// [`KeyScan::next_page`].
pub const MAX_STALLED_BATCHES: u32 = 1000;

/// Honest key count for the browse UI (PRD story 18).
///
/// The UI must derive its text from this, never from DBSIZE:
///
/// | Variant | UI text shape |
/// |---|---|
/// | `AtLeast(n)` | "n+ keys" / "scanning… n so far" |
/// | `Exact(n)` | "n keys" (only after full cursor exhaustion) |
/// | `Unknown` | nothing counted yet |
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyCount {
    /// The scan of the current view has seen `n` distinct keys and is not
    /// known to be finished.
    AtLeast(u64),
    /// The scan's cursor is exhausted: exactly `n` distinct keys match.
    Exact(u64),
    /// No scan of this view has produced anything yet.
    Unknown,
}

impl KeyCount {
    /// The number that is safe to display (the lower bound).
    pub fn n(&self) -> u64 {
        match *self {
            KeyCount::AtLeast(n) | KeyCount::Exact(n) => n,
            KeyCount::Unknown => 0,
        }
    }

    /// True only for [`KeyCount::Exact`].
    pub fn is_exact(&self) -> bool {
        matches!(self, KeyCount::Exact(_))
    }
}

impl std::ops::Add for KeyCount {
    type Output = KeyCount;
    /// Adding counts preserves honesty: an exact only stays exact if both
    /// sides are exact.
    fn add(self, rhs: KeyCount) -> KeyCount {
        use KeyCount::*;
        match (self, rhs) {
            (Unknown, other) | (other, Unknown) => other,
            (Exact(a), Exact(b)) => Exact(a + b),
            (AtLeast(a) | Exact(a), AtLeast(b) | Exact(b)) => AtLeast(a + b),
        }
    }
}

/// One page of keys delivered by [`KeyScan::next_page`].
#[derive(Clone, Debug, PartialEq)]
pub struct ScanPage {
    /// Keys newly discovered in this page (deduplicated, raw bytes,
    /// **scan arrival order** — sorting is a flat-view concern, see
    /// [`crate::flat`]).
    pub new_keys: Vec<Vec<u8>>,
    /// The generation this page was produced under; check
    /// [`crate::cancellation::SearchGeneration::is_current`] before
    /// applying it.
    pub generation: u64,
}

/// Result of asking the engine for one more page.
#[derive(Debug)]
pub enum PageOutcome {
    /// A page was produced; the count snapshot moved with it.
    Page(ScanPage),
    /// No page: the cursor is exhausted. Further calls are no-ops.
    Exhausted,
    /// The token fired between batches; no further `SCAN` was issued.
    /// The engine's cursor still points where the last completed batch
    /// left it, so a *new* engine may resume from the same prefix.
    Cancelled,
    /// The connection failed. The batch that failed was *not* applied; the
    /// cursor is unchanged, so the caller may retry `next_page` (SCAN is
    /// safe to retry: it is a read, and revisits are deduplicated).
    ConnectionError(redis_io::connection::ConnError),
}

impl PageOutcome {
    /// Whether the engine is finished (cursor exhausted).
    pub fn is_exhausted(&self) -> bool {
        matches!(self, PageOutcome::Exhausted)
    }
}

/// The pagination engine state machine.
///
/// Holds: the live cursor, the dedup set over keys kept so far, the
/// MATCH pattern, and the page geometry. Holds **no keys themselves
/// beyond the dedup set** — key *contents* travel to the caller via
/// [`ScanPage`]s; the engine keeps only a set of the key bytes for
/// dedup (they are the view's kept keys, so memory is O(keys kept),
/// never O(keyspace)).
#[derive(Debug)]
pub struct KeyScan {
    cursor: u64,
    exhausted: bool,
    /// Set when the anti-stall bound fired: no more pages will be
    /// produced, but the count stays `AtLeast` (cursor never hit 0).
    stalled_out: bool,
    batch_hint: u64,
    page_size: usize,
    pattern: Option<Vec<u8>>,
    /// Optional server-side `TYPE` argument (Redis ≥ 6): when set, every
    /// SCAN call carries `TYPE <t>` so the server skips non-matching
    /// keys. Additive Phase 3 field — `None` by default, so all Phase 2
    /// behavior is unchanged.
    key_type: Option<Vec<u8>>,
    seen: HashSet<Vec<u8>>,
    generation: u64,
    /// Keys accumulated within the page currently being built — the only
    /// transient storage, bounded by `page_size` + one batch.
    pending: Vec<Vec<u8>>,
}

impl KeyScan {
    /// Create a scan of the whole keyspace (`MATCH None`) at the given
    /// generation.
    pub fn new(generation: u64, page_size: usize, batch_hint: u64) -> Self {
        Self::with_pattern(generation, page_size, batch_hint, None)
    }

    /// Create a scan with a raw `MATCH` glob (`*pattern*` etc. is the
    /// caller's choice; Phase 2 passes it through untouched — Phase 3
    /// builds progressive search on top by re-creating scans with new
    /// patterns and generations).
    ///
    /// `pattern` is raw bytes: key globs may be non-UTF-8.
    pub fn with_pattern(
        generation: u64,
        page_size: usize,
        batch_hint: u64,
        pattern: Option<Vec<u8>>,
    ) -> Self {
        // page_size 0 would loop forever "trying to fill" a zero-sized
        // page; treat it as "deliver whatever one batch returns".
        Self {
            cursor: 0,
            exhausted: false,
            stalled_out: false,
            batch_hint: batch_hint.max(1),
            page_size: page_size.max(1),
            pattern,
            key_type: None,
            seen: HashSet::new(),
            generation,
            pending: Vec::new(),
        }
    }

    /// Attach a server-side `TYPE` filter (Redis ≥ 6): every SCAN call
    /// this engine issues carries `TYPE <t>`, so the server only ships
    /// keys of that type. Additive Phase 3 builder — pure mutation of a
    /// freshly-built scan (the app creates the scan, then attaches).
    ///
    /// Behavior on servers without SCAN TYPE support (Redis < 6 or a
    /// proxy that rejects it): the connection layer surfaces a
    /// *definitive* server error from the first SCAN, which the caller
    /// should treat as "retry without TYPE / fall back to the
    /// client-side filter" (see [`crate::type_filter`]). A disconnect is
    /// not affected by this argument (still retryable, still never
    /// re-sent by the layer).
    pub fn with_type(mut self, key_type: Option<Vec<u8>>) -> Self {
        self.key_type = key_type;
        self
    }

    /// The MATCH pattern this scan was created with.
    pub fn pattern(&self) -> Option<&[u8]> {
        self.pattern.as_deref()
    }

    /// The `TYPE` argument this scan filters by server-side, if any.
    /// Additive Phase 3 accessor (see [`KeyScan::with_type`]).
    pub fn key_type(&self) -> Option<&[u8]> {
        self.key_type.as_deref()
    }

    /// The generation this scan runs under.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Honest count of the keys this view has kept so far.
    ///
    /// `Exact` only when the scan's cursor reached 0 (full exhaustion of
    /// the current view) — including the exhausted-but-empty case, which
    /// is the one cheaply verifiable exact total (story 18). Cancelled
    /// or stalled scans stay `AtLeast`; a never-started scan is
    /// `Unknown`.
    pub fn key_count(&self) -> KeyCount {
        let kept = self.seen.len() as u64 + self.pending.len() as u64;
        if self.exhausted {
            // Cursor reached 0: this view is complete, even if it is
            // complete at zero keys (empty database).
            KeyCount::Exact(kept)
        } else if kept == 0 {
            KeyCount::Unknown
        } else {
            KeyCount::AtLeast(kept)
        }
    }

    /// Whether the engine will produce no more pages (cursor exhausted
    /// or anti-stall bound hit).
    pub fn is_exhausted(&self) -> bool {
        self.exhausted || self.stalled_out
    }

    /// Ask for one more page: run `SCAN` batches until `page_size` new
    /// keys are found or the cursor ends.
    ///
    /// Cancellation is checked *before each batch*, so a cancel takes
    /// effect at the next batch boundary and the loop never issues
    /// another `SCAN` after that. Keys accumulated in a page that gets
    /// cancelled mid-build are **discarded** (the caller asked to stop;
    /// the generation guard would drop them anyway) — the count drops
    /// to what was actually delivered, which stays honest.
    pub async fn next_page(
        &mut self,
        conn: &mut dyn Connection,
        token: &CancellationToken,
    ) -> PageOutcome {
        if self.exhausted || self.stalled_out {
            return PageOutcome::Exhausted;
        }

        // Fill the page: accumulate deduped keys until page_size or
        // cursor exhaustion. Because COUNT is a hint, the server may
        // return 0 new keys per call, hence "or cursor ends" is the only
        // guaranteed terminator — that is why exhaustion is tracked by
        // cursor, never by "batch came back empty".
        let mut page_new = std::mem::take(&mut self.pending);
        let mut stalled: u32 = 0;
        while page_new.len() < self.page_size {
            if token.is_cancelled() {
                // Discard the partial page: stopped means stopped.
                drop(page_new);
                return PageOutcome::Cancelled;
            }
            let batch = match self.scan_call(conn).await {
                Ok(b) => b,
                Err(e) => {
                    // Nothing applied; the in-flight partial page is kept
                    // in `pending` so a retry continues where it stopped.
                    self.pending = page_new;
                    return PageOutcome::ConnectionError(e);
                }
            };
            let had_new = self.absorb(batch.keys.into_iter(), &mut page_new);
            self.cursor = batch.next_cursor;
            if self.cursor == 0 {
                self.exhausted = true;
                break;
            }
            // Anti-stall bound (a MATCH-filtered scan over an unrelated
            // keyspace can return empty batches with a live cursor — a
            // server may legitimately require many iterations; Redis's
            // own docs note SCAN may need O(N) iterations for sparse
            // matches). We bound consecutive *fruitless* batches at
            // MAX_STALLED_BATCHES and report the page we have. Counting
            // stays honest: whatever we kept is still AtLeast.
            stalled = if had_new { 0 } else { stalled + 1 };
            if stalled >= MAX_STALLED_BATCHES {
                break;
            }
        }

        self.seen.extend(page_new.iter().cloned());
        if page_new.is_empty() {
            // Cursor exhausted with zero distinct keys, or the anti-stall
            // bound hit with nothing to show. Either way this scan has
            // produced everything it usefully can; only cursor exhaustion
            // earns `Exact` (and only it returns Exhausted *finally*).
            if self.exhausted {
                self.pending = Vec::new();
                return PageOutcome::Exhausted;
            }
            // Stall with an empty page: stop asking this engine; a
            // stalled scan is treated as exhausted for paging purposes
            // but stays `AtLeast` (honest: cursor never reached 0).
            self.stalled_out = true;
            self.pending = Vec::new();
            return PageOutcome::Exhausted;
        }
        self.pending = Vec::new();
        PageOutcome::Page(ScanPage {
            new_keys: page_new,
            generation: self.generation,
        })
    }

    /// Issue one SCAN round trip, routed by whether a server-side TYPE
    /// filter is attached:
    ///
    /// - No TYPE (the default, and all Phase 2 paths): the dedicated
    ///   [`Connection::scan_batch`] seam — unchanged behavior.
    /// - TYPE set: a `SCAN cursor [MATCH p] COUNT n TYPE t` command
    ///   through the generic [`Connection::issue`] seam (the trait's
    ///   `scan_batch` has no TYPE argument; extending the trait would
    ///   break every implementor, and `issue` is the same wire path the
    ///   backend uses internally — no new seam, just a different
    ///   front door to it). The reply is the standard SCAN array; a
    ///   non-array reply is a definitive error (the server/proxy did
    ///   not understand the command shape — the documented
    ///   "fall back to client-side filter" signal for the app).
    async fn scan_call(&mut self, conn: &mut dyn Connection) -> Result<ScanResult, ConnError> {
        let Some(key_type) = self.key_type.clone() else {
            return conn
                .scan_batch(self.cursor, self.pattern.clone(), self.batch_hint)
                .await;
        };
        let mut args: Vec<Vec<u8>> = Vec::with_capacity(7);
        args.push(self.cursor.to_string().into_bytes());
        if let Some(p) = &self.pattern {
            args.push(b"MATCH".to_vec());
            args.push(p.clone());
        }
        args.push(b"COUNT".to_vec());
        args.push(self.batch_hint.to_string().into_bytes());
        args.push(b"TYPE".to_vec());
        args.push(key_type);
        let reply = conn.issue(Command::new("SCAN", args)).await?;
        parse_scan_reply(&reply)
    }

    /// Merge a batch into the page, deduplicating against all keys ever
    /// kept (SCAN may revisit). Returns whether anything new arrived.
    fn absorb<I: Iterator<Item = Vec<u8>>>(
        &mut self,
        batch: I,
        page_new: &mut Vec<Vec<u8>>,
    ) -> bool {
        let mut had_new = false;
        for key in batch {
            if self.seen.contains(&key) || page_new.iter().any(|k| k == &key) {
                continue; // SCAN revisit: count once, honestly
            }
            page_new.push(key);
            had_new = true;
        }
        had_new
    }
}

/// Parse a raw `SCAN` reply (RESP array `[cursor, [key, ...]]`) into a
/// [`ScanResult`]. Used for the TYPE-filtered path, which goes through
/// the generic `issue` seam instead of `scan_batch`.
///
/// A malformed reply is a definitive error (protocol violation), never
/// a disconnect: the caller must not retry-send (nothing uncertain).
fn parse_scan_reply(reply: &Reply) -> Result<ScanResult, ConnError> {
    let Reply::Array(items) = reply else {
        return Err(ConnError::Definitive(DefinitiveError::new(
            DefinitiveErrorKind::ServerError,
            format!("unexpected reply to SCAN (TYPE-filtered): {reply:?}"),
        )));
    };
    if items.len() != 2 {
        return Err(ConnError::Definitive(DefinitiveError::new(
            DefinitiveErrorKind::ServerError,
            "malformed SCAN reply: expected 2-element array",
        )));
    }
    // Cursor: RESP2 bulk ("17") or RESP3 integer.
    let cursor = match &items[0] {
        Reply::Bulk(b) => std::str::from_utf8(b)
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .ok_or_else(malformed_cursor),
        Reply::Int(i) => u64::try_from(*i).map_err(|_| malformed_cursor()),
        _ => Err(malformed_cursor()),
    }?;
    // Keys: second element must be an array of bulks.
    let Reply::Array(keys) = &items[1] else {
        return Err(ConnError::Definitive(DefinitiveError::new(
            DefinitiveErrorKind::ServerError,
            "malformed SCAN reply: keys element is not an array",
        )));
    };
    let keys = keys
        .iter()
        .map(|k| match k {
            Reply::Bulk(b) => Ok(b.clone()),
            other => Err(ConnError::Definitive(DefinitiveError::new(
                DefinitiveErrorKind::ServerError,
                format!("malformed SCAN reply: key is not bulk ({other:?})"),
            ))),
        })
        .collect::<Result<Vec<Vec<u8>>, _>>()?;
    Ok(ScanResult {
        next_cursor: cursor,
        keys,
    })
}

/// The definitive error for an unparseable SCAN cursor.
fn malformed_cursor() -> ConnError {
    ConnError::Definitive(DefinitiveError::new(
        DefinitiveErrorKind::ServerError,
        "malformed SCAN reply: cursor is not an integer",
    ))
}
