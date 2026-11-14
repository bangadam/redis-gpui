//! Mock-connection tests for the Phase 2 scan engine (no Redis, no
//! Docker). The mock is scripted with SCAN batches and records every
//! call so tests can prove:
//!
//! - pagination walks the cursor in COUNT-sized batches (never `KEYS`),
//! - MATCH patterns pass through untouched,
//! - SCAN revisits are deduplicated,
//! - counts are honest (AtLeast while scanning, Exact at exhaustion),
//! - cancellation stops at a batch boundary,
//! - a connection error mid-page retries safely from the same cursor.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use browse::cancellation::{CancellationToken, SearchGeneration};
use browse::scan::{KeyCount, KeyScan, PageOutcome, DEFAULT_BATCH_HINT};
use redis_io::connection::{Command, ConnError, Connection, Reply, ScanResult};

/// Recorded SCAN call: (cursor, pattern, count).
type ScanCall = (u64, Option<Vec<u8>>, u64);

/// Scriptable SCAN mock: pops one scripted batch per `scan_batch` call,
/// records every call, and can disconnect on demand.
struct MockScanConnection {
    /// Batches in call order (pop from front).
    batches: Vec<ScanResult>,
    calls: Arc<Mutex<Vec<ScanCall>>>,
    /// 0-based index of the `scan_batch` call that should fail with
    /// Disconnected (scripted batches are untouched — a failed call
    /// consumes none, modeling a reply that never arrived).
    fail_on_call: Option<usize>,
}

impl MockScanConnection {
    fn new(batches: Vec<ScanResult>) -> (Self, Arc<Mutex<Vec<ScanCall>>>) {
        let calls = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                batches,
                calls: calls.clone(),
                fail_on_call: None,
            },
            calls,
        )
    }

    fn calls(&self) -> Vec<ScanCall> {
        self.calls.lock().unwrap().clone()
    }

    fn call_count(&self) -> usize {
        self.calls.lock().unwrap().len()
    }
}

#[async_trait]
impl Connection for MockScanConnection {
    async fn issue(&mut self, _cmd: Command) -> Result<Reply, ConnError> {
        // The engine must never fall back to KEYS/DBSIZE; `issue` is the
        // only path those could take. Panic = loud test failure.
        panic!(
            "scan engine must not issue raw commands; got {:?}",
            _cmd.name
        )
    }

    async fn scan_batch(
        &mut self,
        cursor: u64,
        pattern: Option<Vec<u8>>,
        count: u64,
    ) -> Result<ScanResult, ConnError> {
        let index = self.calls.lock().unwrap().len();
        self.calls.lock().unwrap().push((cursor, pattern, count));
        if self.fail_on_call == Some(index) {
            return Err(ConnError::Disconnected {
                reason: Some("mock: mid-scan drop".into()),
                in_flight: None,
            });
        }
        Ok(self.batches.remove(0))
    }

    async fn ping(&mut self) -> Result<(), ConnError> {
        Ok(())
    }
}

fn batch(cursor: u64, keys: &[&str]) -> ScanResult {
    ScanResult {
        next_cursor: cursor,
        keys: keys.iter().map(|k| k.as_bytes().to_vec()).collect(),
    }
}

fn live_token() -> CancellationToken {
    CancellationToken::new(1)
}

// ---------- Pagination (story 16) ----------

/// 9 keys over 3 batches, page size 5, COUNT 1000:
/// page 1 needs batches 1+2 (2+3 keys = 5), page 2 takes batch 3 (4 keys)
/// and exhausts the cursor. Total SCAN calls: 3 — no KEYS, no DBSIZE
/// (the mock panics in `issue` if either is attempted).
#[tokio::test]
async fn paginates_in_batches_without_materializing_everything() {
    let (mut conn, _calls) = MockScanConnection::new(vec![
        batch(17, &["k1", "k2"]),
        batch(42, &["k3", "k4", "k5"]),
        batch(0, &["k6", "k7", "k8", "k9"]),
    ]);
    let mut scan = KeyScan::new(1, 5, DEFAULT_BATCH_HINT);

    let out = scan.next_page(&mut conn, &live_token()).await;
    let page = match out {
        PageOutcome::Page(p) => p,
        other => panic!("expected page, got {other:?}"),
    };
    assert_eq!(page.generation, 1);
    assert_eq!(
        page.new_keys
            .iter()
            .map(|k| k.as_slice())
            .collect::<Vec<_>>(),
        [b"k1".as_slice(), b"k2", b"k3", b"k4", b"k5"]
    );
    // While scanning: honest lower bound, not exact.
    assert_eq!(scan.key_count(), KeyCount::AtLeast(5));

    // Page 1 consumed exactly two SCAN calls with COUNT=1000, cursor 0.
    let c = conn.calls();
    assert_eq!(c.len(), 2);
    assert_eq!(c[0], (0, None, DEFAULT_BATCH_HINT));
    assert_eq!(c[1], (17, None, DEFAULT_BATCH_HINT));

    // The mock panics if the engine ever issues KEYS or DBSIZE (see
    // `issue`), so passing here IS the no-materialization proof.

    let out2 = scan.next_page(&mut conn, &live_token()).await;
    match out2 {
        PageOutcome::Page(p) => assert_eq!(p.new_keys.len(), 4),
        other => panic!("expected second page, got {other:?}"),
    }
    // Cursor hit 0: now the count is Exact.
    assert_eq!(scan.key_count(), KeyCount::Exact(9));

    // Third call took the final batch; a fourth ask is a no-op.
    assert_eq!(conn.call_count(), 3);
    assert!(scan
        .next_page(&mut conn, &live_token())
        .await
        .is_exhausted());
    assert_eq!(conn.call_count(), 3); // no extra SCAN after exhaustion
}

/// A single page that ends exactly at cursor 0 with the page already
/// full: Exhausted is still reported on the NEXT ask (this ask returns
/// the full page), and the count goes Exact immediately.
#[tokio::test]
async fn exact_count_arrives_with_cursor_zero() {
    let (mut conn, _) = MockScanConnection::new(vec![
        batch(0, &["a", "b", "c"]),
        // second ask never issues a SCAN; provide none
    ]);
    let mut scan = KeyScan::new(1, 3, DEFAULT_BATCH_HINT);
    let out = scan.next_page(&mut conn, &live_token()).await;
    assert!(matches!(out, PageOutcome::Page(_)));
    assert_eq!(scan.key_count(), KeyCount::Exact(3));
    assert!(scan
        .next_page(&mut conn, &live_token())
        .await
        .is_exhausted());
    assert_eq!(conn.call_count(), 1);
}

// ---------- MATCH passthrough ----------

#[tokio::test]
async fn match_pattern_is_passed_through_untouched() {
    let (mut conn, _) = MockScanConnection::new(vec![batch(0, &["user:1"])]);
    let pattern = Some(b"user:*".to_vec());
    let mut scan = KeyScan::with_pattern(3, 100, 10, pattern.clone());
    let out = scan.next_page(&mut conn, &live_token()).await;
    assert!(matches!(out, PageOutcome::Page(_)));
    let c = conn.calls();
    assert_eq!(c[0], (0, pattern, 10));
    // non-UTF-8 patterns survive byte-identical
    let (mut conn, _) = MockScanConnection::new(vec![batch(0, &["ok"])]);
    let weird = Some(vec![0xff, b'*']);
    let mut scan = KeyScan::with_pattern(3, 100, 10, weird.clone());
    let _ = scan.next_page(&mut conn, &live_token()).await;
    assert_eq!(conn.calls()[0].1, weird);
}

// ---------- Dedup under SCAN revisit (honesty under dedup) ----------

/// Redis SCAN guarantees elements may be returned more than once. The
/// mock revisits k1/k2 in batch 2; they must be counted and delivered
/// exactly once.
#[tokio::test]
async fn scan_revisits_are_deduplicated() {
    let (mut conn, _) = MockScanConnection::new(vec![
        batch(5, &["k1", "k2"]),
        batch(0, &["k1", "k2", "k3", "k1"]), // revisit + duplicate in-batch
    ]);
    let mut scan = KeyScan::new(1, 10, DEFAULT_BATCH_HINT);
    let out = scan.next_page(&mut conn, &live_token()).await;
    match out {
        PageOutcome::Page(p) => {
            assert_eq!(
                p.new_keys.iter().map(|k| k.as_slice()).collect::<Vec<_>>(),
                [b"k1".as_slice(), b"k2", b"k3"]
            );
        }
        other => panic!("expected page, got {other:?}"),
    }
    // Exact because the cursor ended at 0 — deduped: 3, not 5.
    assert_eq!(scan.key_count(), KeyCount::Exact(3));
}

// ---------- Cancellation (acceptance 2) ----------

/// Cancel fires between batches: the loop exits at the next boundary
/// and no further SCAN is issued. Two sub-cases: token cancelled before
/// the scan starts (zero calls), and token cancelled after one delivered
/// page (no calls for the next page).
#[tokio::test]
async fn cancel_stops_at_batch_boundary() {
    // Case A: token already cancelled before the first batch — the
    // check happens BEFORE each batch, so zero SCAN calls occur.
    let (mut conn, _) = MockScanConnection::new(vec![batch(10, &["a", "b"]), batch(0, &["c"])]);
    let token = CancellationToken::new(1);
    token.cancel();
    let mut scan = KeyScan::new(1, 5, DEFAULT_BATCH_HINT);
    let out = scan.next_page(&mut conn, &token).await;
    assert!(matches!(out, PageOutcome::Cancelled));
    assert_eq!(conn.call_count(), 0);
    assert_eq!(scan.key_count(), KeyCount::Unknown);

    // Case B: one batch completed a page, then cancel before the next
    // page's first batch — no SCAN for the cancelled page.
    let (mut conn, _) = MockScanConnection::new(vec![batch(10, &["a", "b"]), batch(0, &["c"])]);
    let token = CancellationToken::new(2);
    let mut scan = KeyScan::new(2, 2, DEFAULT_BATCH_HINT);
    let out = scan.next_page(&mut conn, &token).await;
    match out {
        PageOutcome::Page(p) => assert_eq!(p.new_keys.len(), 2),
        other => panic!("expected page, got {other:?}"),
    }
    assert_eq!(conn.call_count(), 1);

    token.cancel();
    let out2 = scan.next_page(&mut conn, &token).await;
    assert!(matches!(out2, PageOutcome::Cancelled));
    // No SCAN was issued for the cancelled page (checked before batch).
    assert_eq!(conn.call_count(), 1);
    // The delivered page's keys are kept; count is AtLeast, not Exact.
    assert_eq!(scan.key_count(), KeyCount::AtLeast(2));
}

/// The generation guard: a late batch produced under an old generation
/// is rejected by the flat view (the Phase 2 seam for acceptance 2).
#[tokio::test]
async fn generation_guard_rejects_late_batch() {
    let (mut conn, _) = MockScanConnection::new(vec![batch(0, &["stale:key"])]);
    let gen = SearchGeneration::new();
    let g1 = gen.next_generation();
    let mut old_scan = KeyScan::new(g1, 10, DEFAULT_BATCH_HINT);
    let late = match old_scan.next_page(&mut conn, &live_token()).await {
        PageOutcome::Page(p) => p,
        other => panic!("expected page, got {other:?}"),
    };

    // A new run starts (user retyped the pattern): generation moves.
    let g2 = gen.next_generation();
    assert!(g2 > g1);
    assert!(!gen.is_current(late.generation)); // the guard itself
    assert_eq!(late.generation, g1);

    // The flat projection drops the late page.
    let mut list = browse::flat::FlatKeyList::new();
    assert!(list.is_empty());
    assert!(!list.apply_page(&late, &gen));
    assert!(list.is_empty());
}

// ---------- Connection failure mid-page ----------

/// A disconnect mid-page leaves the cursor unchanged; retrying
/// `next_page` re-issues SCAN from the SAME cursor (SCAN is a read;
/// revisits are deduplicated, so retry is safe — unlike writes, which
/// have the OutcomeUnknown rule).
#[tokio::test]
async fn connection_error_mid_page_is_retryable_from_same_cursor() {
    let (mut conn, _) = MockScanConnection::new(vec![batch(7, &["k1", "k2"]), batch(0, &["k3"])]);
    let mut scan = KeyScan::new(1, 5, DEFAULT_BATCH_HINT);

    // Page size 5 needs both batches: call #1 succeeds (k1/k2, cursor
    // → 7), call #2 drops mid-flight (reply never arrives). The failed
    // call consumed no scripted batch — the model of a dropped reply.
    conn.fail_on_call = Some(1);
    let out = scan.next_page(&mut conn, &live_token()).await;
    assert!(matches!(out, PageOutcome::ConnectionError(_)));
    // The failed call WAS a round trip (recorded), but nothing from it
    // was applied: cursor still 7, k1/k2 still pending in the engine.
    let c = conn.calls();
    assert_eq!(c.len(), 2);
    assert_eq!(c[1].0, 7); // engine was at cursor 7 when the drop hit

    // Retry: SCAN is a read — safe to re-issue (revisits dedup). The
    // engine continues from cursor 7 with its pending keys.
    let out2 = scan.next_page(&mut conn, &live_token()).await;
    match out2 {
        PageOutcome::Page(p) => {
            assert_eq!(
                p.new_keys.iter().map(|k| k.as_slice()).collect::<Vec<_>>(),
                [b"k1".as_slice(), b"k2", b"k3"]
            );
        }
        other => panic!("expected page, got {other:?}"),
    }
    let c = conn.calls();
    assert_eq!(c.len(), 3);
    assert_eq!(c[2].0, 7); // retry resumed from the pre-failure cursor
    assert_eq!(scan.key_count(), KeyCount::Exact(3));
}

// ---------- Edge cases ----------

#[tokio::test]
async fn empty_database_is_exact_zero() {
    let (mut conn, _) = MockScanConnection::new(vec![batch(0, &[])]);
    let mut scan = KeyScan::new(1, 100, DEFAULT_BATCH_HINT);
    // Before any scan: Unknown (never fabricated).
    assert_eq!(scan.key_count(), KeyCount::Unknown);
    let out = scan.next_page(&mut conn, &live_token()).await;
    assert!(out.is_exhausted());
    // Empty DB, cursor exhausted: Exact(0) — the one case where we can
    // cheaply verify an exact total (one SCAN round trip).
    assert_eq!(scan.key_count(), KeyCount::Exact(0));
}

#[tokio::test]
async fn zero_count_and_zero_page_size_are_sanitized() {
    // COUNT hint 0 and page size 0 are degenerate; constructor floors
    // them to 1 so the engine can never spin on a zero page.
    let (mut conn, _) = MockScanConnection::new(vec![batch(1, &["k1"]), batch(0, &["k2"])]);
    let mut scan = KeyScan::new(1, 0, 0);
    let out = scan.next_page(&mut conn, &live_token()).await;
    match out {
        PageOutcome::Page(p) => assert_eq!(p.new_keys.len(), 1),
        other => panic!("expected page, got {other:?}"),
    }
    assert_eq!(conn.calls()[0].2, 1); // COUNT hint floored to 1
}

/// A server that keeps returning an empty page with a live cursor
/// (sparse MATCH) hits the anti-stall bound instead of spinning.
#[tokio::test]
async fn empty_live_batches_hit_anti_stall_bound() {
    // page_size 1 + a batch with 0 keys + cursor != 0: the loop would
    // spin forever without the bound; the engine stops after
    // MAX_STALLED_BATCHES empty batches.
    let mut batches = Vec::new();
    for _ in 0..(browse::scan::MAX_STALLED_BATCHES + 1) {
        batches.push(batch(9, &[]));
    }
    let (mut conn, _) = MockScanConnection::new(batches);
    let mut scan = KeyScan::new(1, 1, DEFAULT_BATCH_HINT);
    let out = scan.next_page(&mut conn, &live_token()).await;
    assert!(out.is_exhausted());
    assert_eq!(
        conn.call_count(),
        browse::scan::MAX_STALLED_BATCHES as usize
    );
    // Never Exact: cursor never reached 0. Stays Unknown (nothing kept).
    assert_eq!(scan.key_count(), KeyCount::Unknown);
}
