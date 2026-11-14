//! Mock-connection tests for the Phase 3 browse additions (no Redis, no
//! Docker): the progressive-search driver over the real engine seam,
//! the tree's scoped lazy-expansion scans, and the server-side TYPE
//! passthrough through the generic `issue` path.

use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use browse::cancellation::SearchGeneration;
use browse::db::fetch_db_count;
use browse::scan::{KeyCount, KeyScan, PageOutcome, DEFAULT_BATCH_HINT};
use browse::search::{MatchMode, ProgressiveSearch};
use browse::tree::KeyTree;
use browse::type_filter::TypeFilter;
use redis_io::connection::{Command, ConnError, Connection, Reply, ScanResult};

/// Recorded SCAN call via the dedicated seam: (cursor, pattern, count).
type ScanCall = (u64, Option<Vec<u8>>, u64);

/// Recorded raw command via `issue`: (name, args).
type Issued = (String, Vec<Vec<u8>>);

/// Mock that scripts BOTH connection seams: `scan_batch` pops scripted
/// batches; `issue` pops scripted replies. Both record their calls so
/// tests can prove which seam was used and with what arguments.
struct MockConn {
    batches: Vec<ScanResult>,
    /// Scripted replies for `issue`, popped front-first.
    replies: Vec<Reply>,
    scan_calls: Arc<Mutex<Vec<ScanCall>>>,
    issues: Arc<Mutex<Vec<Issued>>>,
}

impl MockConn {
    fn new(batches: Vec<ScanResult>, replies: Vec<Reply>) -> Self {
        Self {
            batches,
            replies,
            scan_calls: Arc::new(Mutex::new(Vec::new())),
            issues: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn scan_calls(&self) -> Vec<ScanCall> {
        self.scan_calls.lock().unwrap().clone()
    }

    fn issues(&self) -> Vec<Issued> {
        self.issues.lock().unwrap().clone()
    }
}

#[async_trait]
impl Connection for MockConn {
    async fn issue(&mut self, cmd: Command) -> Result<Reply, ConnError> {
        self.issues
            .lock()
            .unwrap()
            .push((cmd.name.clone(), cmd.args.clone()));
        match self.replies.pop() {
            Some(r) => Ok(r),
            None => Err(ConnError::Disconnected {
                reason: Some("mock: no scripted reply".into()),
                in_flight: None,
            }),
        }
    }

    async fn scan_batch(
        &mut self,
        cursor: u64,
        pattern: Option<Vec<u8>>,
        count: u64,
    ) -> Result<ScanResult, ConnError> {
        self.scan_calls
            .lock()
            .unwrap()
            .push((cursor, pattern, count));
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

fn live_token() -> browse::cancellation::CancellationToken {
    browse::cancellation::CancellationToken::new(1)
}

// ---------- Progressive search over the mock ----------

/// The full search flow: pattern → engine → pages → flat view, with the
/// MATCH pattern passed through untouched and honest counts.
#[tokio::test]
async fn search_scans_with_converted_pattern_and_honest_counts() {
    // 3 matches over 2 batches, page size 2.
    let mut conn = MockConn::new(
        vec![batch(4, &["user:1", "user:2"]), batch(0, &["user:20"])],
        vec![],
    );
    let mut search = ProgressiveSearch::new(MatchMode::Prefix, 2, DEFAULT_BATCH_HINT);
    let gen = SearchGeneration::new();
    let mut run = search.start(b"user:1", &gen);

    // The engine was born with the converted glob `user:1*`.
    assert_eq!(run.pattern(), Some(b"user:1*".to_vec().as_slice()));
    assert_eq!(run.generation(), 1);

    match run.scan.next_page(&mut conn, &run.token).await {
        PageOutcome::Page(p) => {
            assert_eq!(p.generation, 1);
            assert_eq!(p.new_keys.len(), 2);
        }
        other => panic!("expected page, got {other:?}"),
    }
    // While searching: AtLeast (honest, never DBSIZE).
    assert_eq!(run.key_count(), KeyCount::AtLeast(2));

    match run.scan.next_page(&mut conn, &run.token).await {
        PageOutcome::Page(p) => assert_eq!(p.new_keys.len(), 1),
        other => panic!("expected page, got {other:?}"),
    }
    // Cursor hit 0: Exact.
    assert_eq!(run.key_count(), KeyCount::Exact(3));
    // The MATCH pattern arrived byte-identical on every call.
    let calls = conn.scan_calls();
    assert_eq!(calls.len(), 2);
    for (cursor, pattern, _) in &calls {
        assert_eq!(pattern.as_deref(), Some(b"user:1*".to_vec().as_slice()));
        assert!(*cursor == 0 || *cursor == 4);
    }
}

/// Cancel mid-search: the engine stops at a batch boundary; the token
/// for the old run is cancelled by the NEXT start (the debounce
/// restart), not by magic.
#[tokio::test]
async fn cancel_mid_search_stops_the_engine() {
    let mut conn = MockConn::new(
        vec![batch(7, &["user:1", "user:2"]), batch(0, &["user:3"])],
        vec![],
    );
    let mut search = ProgressiveSearch::new(MatchMode::Contains, 2, DEFAULT_BATCH_HINT);
    let gen = SearchGeneration::new();
    let mut run = search.start(b"cart", &gen);
    assert_eq!(run.pattern(), Some(b"*cart*".to_vec().as_slice()));

    match run.scan.next_page(&mut conn, &run.token).await {
        PageOutcome::Page(p) => assert_eq!(p.new_keys.len(), 2),
        other => panic!("expected page, got {other:?}"),
    }
    assert_eq!(conn.scan_calls().len(), 1);

    // The user retypes / hits escape: the driver is told to cancel.
    search.cancel();
    assert!(run.token.is_cancelled());
    let out = run.scan.next_page(&mut conn, &run.token).await;
    assert!(matches!(out, PageOutcome::Cancelled));
    // No further SCAN after the cancel.
    assert_eq!(conn.scan_calls().len(), 1);
    // Delivered keys still counted honestly: AtLeast, not Exact.
    assert_eq!(run.key_count(), KeyCount::AtLeast(2));
}

/// The generation guard drops late pages from a superseded search: the
/// app-facing seam (flat list / tree apply_page) is protected by the
/// shared SearchGeneration, and a new start cancels the old token.
#[tokio::test]
async fn generation_guard_drops_late_search_pages() {
    // Old run: completes a page under generation 1 (would arrive "late").
    let mut conn = MockConn::new(vec![batch(0, &["user:1:stale"])], vec![]);
    let mut search = ProgressiveSearch::new(MatchMode::Prefix, 10, DEFAULT_BATCH_HINT);
    let gen = SearchGeneration::new();
    let mut run1 = search.start(b"user", &gen);
    let late = match run1.scan.next_page(&mut conn, &live_token()).await {
        PageOutcome::Page(p) => p,
        other => panic!("expected page, got {other:?}"),
    };
    assert_eq!(late.generation, 1);

    // User typed more: new run, generation 2; run1's token auto-cancelled.
    let run2 = search.start(b"user:1:c", &gen);
    assert!(run1.token.is_cancelled());
    assert!(run2.generation() > late.generation);

    // The seam the app wires: the flat view drops the late page.
    let mut flat = browse::flat::FlatKeyList::new();
    assert!(!flat.apply_page(&late, &gen));
    assert!(flat.is_empty());
    // ...and so does the tree.
    let mut tree = KeyTree::new();
    assert!(!tree.apply_page(&late, &gen));
    assert!(tree.is_empty());
}

/// Retyping mid-flight: each start retires the previous token AND the
/// previous engine simply stops being asked (the app drops it); the new
/// engine starts at cursor 0 with the new pattern.
#[tokio::test]
async fn each_start_begins_a_fresh_engine_at_cursor_zero() {
    let mut conn = MockConn::new(vec![batch(0, &["a"])], vec![]);
    let mut search = ProgressiveSearch::new(MatchMode::Prefix, 10, DEFAULT_BATCH_HINT);
    let gen = SearchGeneration::new();
    let run1 = search.start(b"a", &gen);
    let mut run2 = search.start(b"ab", &gen);
    assert!(run1.token.is_cancelled());
    assert!(!run2.token.is_cancelled());
    // Fresh engine: nothing seen, cursor at 0 (first SCAN call uses 0).
    assert_eq!(run2.key_count(), KeyCount::Unknown);
    let _ = run2.scan.next_page(&mut conn, &run2.token).await;
    assert_eq!(conn.scan_calls()[0].0, 0);
    assert_eq!(
        conn.scan_calls()[0].1.as_deref(),
        Some(b"ab*".to_vec().as_slice())
    );
}

// ---------- Tree lazy expansion over the mock ----------

/// The lazy-expansion protocol end-to-end against the mock:
/// global page → expand an unloaded node → scoped `prefix*` scan →
/// pages apply → cursor exhausts → mark_subtree_loaded flips the flag.
#[tokio::test]
async fn lazy_expansion_issues_scoped_scan_and_marks_loaded() {
    let mut conn = MockConn::new(
        vec![
            batch(0, &["user:1", "user:2"]),     // the global scan page
            batch(0, &["user:2:a", "user:2:b"]), // the scoped expansion
        ],
        vec![],
    );
    let mut tree = KeyTree::new();
    let gen = SearchGeneration::new();
    let g = gen.next_generation();

    // 1. Global scan page arrives; nothing is children_loaded yet.
    let page = match KeyScan::new(g, 10, DEFAULT_BATCH_HINT)
        .next_page(&mut conn, &live_token())
        .await
    {
        PageOutcome::Page(p) => p,
        other => panic!("expected page, got {other:?}"),
    };
    assert!(tree.apply_page(&page, &gen));
    let user2 = tree
        .rows()
        .into_iter()
        .find(|r| r.path == b"user:2")
        .unwrap();
    assert!(!user2.children_loaded);
    assert!(!user2.has_children); // children unknown: not expanded yet

    // 2. User expands `user:2`: the app builds the scoped scan.
    let g2 = gen.next_generation();
    let mut scoped = tree.scan_for_prefix(b"user:2", g2, 10, DEFAULT_BATCH_HINT);
    assert_eq!(scoped.pattern(), Some(b"user:2:*".to_vec().as_slice()));

    // 3. The scoped page flows back through the same seam.
    let scoped_page = match scoped.next_page(&mut conn, &live_token()).await {
        PageOutcome::Page(p) => p,
        other => panic!("expected page, got {other:?}"),
    };
    assert!(tree.apply_page(&scoped_page, &gen));
    let user2 = tree
        .rows()
        .into_iter()
        .find(|r| r.path == b"user:2")
        .unwrap();
    assert!(user2.has_children); // a, b arrived
    assert!(!user2.children_loaded); // scan not yet known complete

    // 4. The scoped scan's cursor hit 0 (mock ended it): mark loaded.
    assert!(tree.mark_subtree_loaded(b"user:2"));
    let user2 = tree
        .rows()
        .into_iter()
        .find(|r| r.path == b"user:2")
        .unwrap();
    assert!(user2.children_loaded);
    let leaf_a = tree
        .rows()
        .into_iter()
        .find(|r| r.path == b"user:2:a")
        .unwrap();
    assert!(leaf_a.is_leaf);
    // The scoped SCAN call carried the escaped prefix glob.
    let calls = conn.scan_calls();
    assert_eq!(calls[1].1.as_deref(), Some(b"user:2:*".to_vec().as_slice()));
}

// ---------- Server-side TYPE passthrough ----------

/// A TYPE-filtered scan goes through `issue` with SCAN ... TYPE t
/// (cursor, [MATCH], COUNT, TYPE), and the reply is parsed as a normal
/// SCAN batch.
#[tokio::test]
async fn type_filtered_scan_passes_type_arg_via_issue() {
    let reply = Reply::Array(vec![
        Reply::Bulk(b"0".to_vec()),
        Reply::Array(vec![
            Reply::Bulk(b"h:1".to_vec()),
            Reply::Bulk(b"h:2".to_vec()),
        ]),
    ]);
    let mut conn = MockConn::new(vec![], vec![reply]);
    let mut scan =
        KeyScan::with_pattern(1, 10, 100, Some(b"h:*".to_vec())).with_type(Some(b"hash".to_vec()));

    match scan.next_page(&mut conn, &live_token()).await {
        PageOutcome::Page(p) => {
            assert_eq!(
                p.new_keys.iter().map(|k| k.as_slice()).collect::<Vec<_>>(),
                [b"h:1".as_slice(), b"h:2"]
            );
        }
        other => panic!("expected page, got {other:?}"),
    }
    // The dedicated scan_batch seam was NOT used.
    assert!(conn.scan_calls().is_empty());
    // One SCAN via issue, with cursor 0, MATCH h:*, COUNT 100, TYPE hash.
    let issues = conn.issues();
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].0, "SCAN");
    assert_eq!(
        issues[0].1,
        vec![
            b"0".to_vec(),
            b"MATCH".to_vec(),
            b"h:*".to_vec(),
            b"COUNT".to_vec(),
            b"100".to_vec(),
            b"TYPE".to_vec(),
            b"hash".to_vec(),
        ]
    );
    assert_eq!(scan.key_count(), KeyCount::Exact(2));
}

/// Without a TYPE filter the engine uses the dedicated scan_batch seam
/// (unchanged Phase 2 behavior — the app crate's browsing path).
#[tokio::test]
async fn untyped_scan_uses_the_dedicated_seam() {
    let mut conn = MockConn::new(vec![batch(0, &["k"])], vec![]);
    let mut scan = KeyScan::with_pattern(1, 10, 100, None); // no TYPE
    assert!(scan.key_type().is_none());
    assert!(matches!(
        scan.next_page(&mut conn, &live_token()).await,
        PageOutcome::Page(_)
    ));
    assert_eq!(conn.scan_calls().len(), 1);
    assert!(conn.issues().is_empty()); // issue never touched
}

/// `TypeFilter::scan_type_arg` feeds `with_type` directly: the preset
/// name is the TYPE argument; All/Other produce no TYPE narrowing (and
/// the mock sees the plain scan_batch seam for them).
#[tokio::test]
async fn type_filter_arg_feeds_with_type() {
    for filter in [
        TypeFilter::String,
        TypeFilter::List,
        TypeFilter::Set,
        TypeFilter::ZSet,
        TypeFilter::Hash,
        TypeFilter::Stream,
    ] {
        let t = filter.scan_type_arg().unwrap().to_vec();
        let reply = Reply::Array(vec![Reply::Bulk(b"0".to_vec()), Reply::Array(vec![])]);
        let mut conn = MockConn::new(vec![], vec![reply]);
        let mut scan = KeyScan::new(1, 10, 100).with_type(Some(t.clone()));
        let out = scan.next_page(&mut conn, &live_token()).await;
        assert!(out.is_exhausted(), "{filter:?} scan should exhaust");
        let issues = conn.issues();
        assert_eq!(issues[0].1.last().unwrap(), &t, "TYPE arg must be last");
        assert_eq!(issues[0].1[issues[0].1.len() - 2], b"TYPE".to_vec());
    }
    // All and Other: no server-side narrowing at all.
    assert!(TypeFilter::All.scan_type_arg().is_none());
    assert!(TypeFilter::Other.scan_type_arg().is_none());
}

// ---------- DB count over the mock ----------

#[tokio::test]
async fn fetch_db_count_round_trip_over_issue() {
    let reply = Reply::Array(vec![
        Reply::Bulk(b"databases".to_vec()),
        Reply::Bulk(b"16".to_vec()),
    ]);
    let mut conn = MockConn::new(vec![], vec![reply]);
    let info = fetch_db_count(&mut conn).await.unwrap();
    assert_eq!(info.count, 16);
    assert!(info.is_valid_db(15));
    assert!(!info.is_valid_db(16));
    let issues = conn.issues();
    assert_eq!(issues.len(), 1);
    assert_eq!(issues[0].0, "CONFIG");
    assert_eq!(issues[0].1, vec![b"GET".to_vec(), b"databases".to_vec()]);
}
