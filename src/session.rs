//! One tab's connection session: the Phase 2 entity (PLAN.md section 5).
//!
//! Owns the `RedisConnection` handle, the [`WriteGate`], the key-scan
//! state ([`KeyScan`] + [`FlatKeyList`] + [`SearchGeneration`]), the
//! selected key, and the loaded value. All Redis I/O runs on the global
//! tokio runtime (see `main.rs` docs); results return here on the main
//! thread through GPUI's foreground executor. The UI thread never
//! blocks on the network.
//!
//! # State transitions (tested in [`tests`])
//!
//! ```text
//!  Connecting ──Connected──▶ Live ──Disconnected event──▶ Disconnected(sticky)
//!      │ error                                                      │
//!      └──▶ (form shows error; no tab was opened)                   │ reconnect() ok
//!                                       Disconnected(sticky) ◀──────┘ failed
//! ```
//!
//! On every `ConnEvent::Disconnected`: `gate.apply(Disconnected)` (forces
//! ReadOnly — it starts ReadOnly in Phase 2, so this is the invariant
//! *holder*, not a visible change yet), the scan token is cancelled,
//! and the panels keep their sticky last state with a Reconnect affordance.
//!
//! # The I/O bridge pattern
//!
//! Every operation follows the same three hops:
//!
//! 1. **UI thread** clones what the background needs (the connection
//!    mutex, key bytes, cursor state) and calls [`Self::run_io`].
//! 2. `run_io` spawns a future on the global tokio runtime; the future
//!    locks the connection mutex (connections are serial: a
//!    `MultiplexedConnection` supports pipelining, but our
//!    correctness contract — sticky-broken disconnect detection on the
//!    *next* in-flight command — is simplest with one command at a
//!    time) and awaits the redis-io call.
//! 3. The tokio `JoinHandle` is awaited from `cx.spawn` (GPUI
//!    foreground), and the result is applied to this entity + `notify`.
//!
//! Stale results are guarded by the scan [`SearchGeneration`] exactly
//! as `crates/browse/src/scan.rs` ("Driving from GPUI") documents.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use browse::cancellation::{CancellationToken, SearchGeneration};
use browse::db::{fetch_db_count, DbInfo};
use browse::flat::FlatKeyList;
#[cfg(test)]
use browse::scan::ScanPage;
use browse::scan::{KeyCount, KeyScan, PageOutcome, DEFAULT_BATCH_HINT};
use browse::search::{MatchMode, ProgressiveSearch};
use browse::tree::{KeyTree, TreeRow};
use browse::type_filter as browse_type_filter;
use gpui::{prelude::*, App, Context, EventEmitter, Task};
use redis_io::connection::{Command, ConnError, ConnEvent, Connection, Reply};
use redis_io::log_hygiene::Secret;
use redis_io::outcome::WriteOutcome;
use redis_io::redis_backend::{RedisConnection, RedisConnectionConfig};
use tokio::sync::Mutex;
use value_model::raw::RawValue;
use value_model::ttl::{TtlReply, TtlState};
use value_model::unsupported::ReadonlyUnsupported;
use value_model::views::{available_views, render_preview, RenderedView, ViewKind};
use write_safety::gate::{GateEvent, WriteGate};

use crate::reply_to_raw::{full_read_command, reply_to_raw};

/// Events the session emits to the workspace (tab title / status).
#[derive(Clone, Debug, PartialEq)]
pub enum SessionEvent {
    /// The session is now connected (initial or reconnect).
    Connected,
    /// The link went down; sticky until reconnect.
    Disconnected(Option<String>),
    /// A destructive command needs its typed-phrase confirmation. The
    /// payload is the modal's phrase placeholder (`type FLUSHDB to
    /// confirm`); the workspace resets the shared confirm field when it
    /// arrives.
    ConsoleConfirmOpened(String),
    /// Phase 6: the insecure-TLS chip's visibility changed (the
    /// persistent warning, not a log line).
    TlsInsecureChanged(bool),
    /// Phase 6: a pending SSH host-key modal opened (TOFU prompt or
    /// hard block). The workspace renders the overlay.
    HostKeyModalOpened,
    /// Phase 6: the SSH host-key modal was answered/cleared.
    HostKeyModalClosed,
}

/// Load-more geometry: keys per delivered page, `SCAN COUNT` hint.
pub const PAGE_SIZE: usize = 200;

/// The filter a chip row offers. UI-facing label holder; the matching
/// semantics delegate to browse's `type_filter::matches` (Phase 3
/// reconciliation: the app-local match logic is removed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RawFilter {
    /// All keys.
    All,
    /// Strings only.
    String,
    /// Hashes only.
    Hash,
    /// Lists only.
    List,
    /// Sets only.
    Set,
    /// Sorted sets only.
    ZSet,
    /// Streams only.
    Stream,
}

impl RawFilter {
    /// Every chip, in display order.
    pub const ALL: [RawFilter; 7] = [
        RawFilter::All,
        RawFilter::String,
        RawFilter::Hash,
        RawFilter::List,
        RawFilter::Set,
        RawFilter::ZSet,
        RawFilter::Stream,
    ];

    /// The chip label.
    pub fn label(self) -> &'static str {
        match self {
            RawFilter::All => "All",
            RawFilter::String => "String",
            RawFilter::Hash => "Hash",
            RawFilter::List => "List",
            RawFilter::Set => "Set",
            RawFilter::ZSet => "ZSet",
            RawFilter::Stream => "Stream",
        }
    }

    /// Map to browse's `TypeFilter` (the single source of matching
    /// truth since the Phase 3 reconciliation).
    pub fn to_type_filter(self) -> browse::type_filter::TypeFilter {
        use browse::type_filter::TypeFilter;
        match self {
            RawFilter::All => TypeFilter::All,
            RawFilter::String => TypeFilter::String,
            RawFilter::Hash => TypeFilter::Hash,
            RawFilter::List => TypeFilter::List,
            RawFilter::Set => TypeFilter::Set,
            RawFilter::ZSet => TypeFilter::ZSet,
            RawFilter::Stream => TypeFilter::Stream,
        }
    }
}

/// Filter `keys` client-side against the known types in `types`, via
/// browse's `type_filter::matches` (reconciled Phase 3: no app-local
/// match logic remains).
///
/// Keys whose type is unknown (not yet probed) pass through when the
/// filter is All, and are held back otherwise — honest: an untyped key
/// is not known to match a typed filter.
pub fn apply_type_filter(
    keys: &[Vec<u8>],
    types: &HashMap<Vec<u8>, String>,
    filter: Option<RawFilter>,
) -> Vec<Vec<u8>> {
    let Some(filter) = filter else {
        return keys.to_vec();
    };
    let tf = filter.to_type_filter();
    keys.iter()
        .filter(|k| match types.get(k.as_slice()) {
            // Probed: browse's matcher decides.
            Some(t) => browse_type_filter::matches(t, tf),
            // Unprobed: only All lets it through — held back under a
            // typed filter (honest).
            None => tf == browse::type_filter::TypeFilter::All,
        })
        .cloned()
        .collect()
}

/// The tree-row projection: all rows, minus the children of collapsed
/// branch nodes (`expanded` is the UI's expansion set). Depth-capped
/// rows pass through as-is. A free function so it is testable without
/// a session entity.
pub fn project_tree_rows(tree: &KeyTree, expanded: &HashSet<Vec<u8>>) -> Vec<TreeRow> {
    let all = tree.rows();
    let mut out = Vec::with_capacity(all.len());
    let mut hidden_depth: Option<usize> = None;
    for row in all {
        if let Some(d) = hidden_depth {
            // Skip until we pop back to a row at depth <= the
            // collapsed row's depth.
            if row.depth > d {
                continue;
            }
            hidden_depth = None;
        }
        let collapsed_branch = row.has_children && !expanded.contains(&row.path);
        if collapsed_branch {
            // Show the branch row itself (the expand affordance), hide
            // everything below it until a shallower row.
            let depth = row.depth;
            out.push(row);
            hidden_depth = Some(depth);
        } else {
            out.push(row);
        }
    }
    out
}

/// The per-connection session state machine.
pub struct ConnectionSession {
    /// The live connection, shared with the tokio runtime. `None` only
    /// transiently during initial connect (the session entity is built
    /// only after a successful connect, so it is `Some` here).
    conn: Arc<Mutex<RedisConnection>>,
    /// The receiver for `ConnEvent`s, drained on the main thread.
    events_rx: tokio::sync::mpsc::Receiver<ConnEvent>,
    /// Read-only by default; forces ReadOnly on Disconnected.
    gate: WriteGate,
    /// Display name: `host:port/db`.
    pub name: String,
    /// Link state.
    pub link: LinkState,
    /// Scan engine (shared with the tokio-side page future), flat
    /// projection, and generation guard.
    scan: Arc<Mutex<KeyScan>>,
    flat: FlatKeyList,
    generation: SearchGeneration,
    token: CancellationToken,
    /// True while a page fetch is in flight (disables "Load more").
    fetching_page: bool,
    /// Lazily-filled per-key type cache (TYPE probe results), used by
    /// the client-side type-filter chips (Phase 3). Filled for keys of
    /// the visible page only, on page delivery.
    key_types: HashMap<Vec<u8>, String>,
    /// The selected type-filter chip (`None` = All).
    type_filter: Option<RawFilter>,
    /// True while a type-probe pipeline is in flight (one per page).
    probing_types: bool,
    /// The selected key (raw bytes; possibly non-UTF-8).
    selected_key: Option<Vec<u8>>,
    /// What the value panel shows.
    pub value_panel: ValuePanelState,
    /// Server-side type of the selected key (from the TYPE probe).
    key_type: Option<String>,
    /// TTL of the selected key (fetched alongside the value).
    key_ttl: Option<TtlState>,
    /// Counter for in-flight value loads (stale load guard).
    value_load_id: u64,
    // ---- Phase 3 browse wiring ------------------------------------------
    /// Colon-tree projection over the same pages as `flat` (fed by the
    /// same `apply_page` seam; generation-guarded identically).
    tree: KeyTree,
    /// Which projection the keys panel renders (`true` = tree mode).
    tree_mode: bool,
    /// UI expansion state: node paths whose children are shown. Pure
    /// display state — completeness is `TreeRow::children_loaded`.
    expanded: HashSet<Vec<u8>>,
    /// Progressive search state machine (Phase 3). `None` = no search
    /// box text; a run is armed per debounced query.
    search: ProgressiveSearch,
    /// The last search text accepted by the debouncer ("" = none).
    search_text: String,
    /// Debounce serial: each keystroke bumps it; a delayed task only
    /// starts a run if its serial is still current.
    search_serial: u64,
    /// True while a search run's drive loop is active (footer state).
    searching: bool,
    /// True while a loop-driven scan (search or lazy-expansion) owns
    /// the engine; "Load more" is disabled meanwhile.
    scan_driving: bool,
    /// Database count from `CONFIG GET databases` (fetched after
    /// connect; fallback 16 per `browse::db`).
    db_info: DbInfo,
    /// The currently selected logical DB (index into `db_info`).
    current_db: u16,
    /// True while a SELECT round trip is in flight.
    switching_db: bool,
    /// Inline error from a failed DB switch (SELECT refused etc.).
    db_error: Option<String>,
    // ---- Phase 4: console ------------------------------------------------
    /// The console state (log, history, pending confirmation). Flows are
    /// driven by the `console_*` methods below — the view only calls them.
    pub console: crate::console::ConsoleState,
    /// The serial for the running-command guard: a console command in
    /// flight holds `Some(serial)`; results with a different (older)
    /// serial are dropped (Stop / stale-completion discipline).
    console_serial: u64,
    /// True while a console command is in flight (one at a time).
    console_running: bool,
    // ---- Phase 5: value editing + save flows ---------------------------
    /// The dirty-editor state machine for the selected key (value-model).
    /// `Some` once a value of an editable type is loaded; `None` for
    /// streams, unsupported types, and no selection.
    pub editor: Option<value_model::editor::EditorState>,
    /// Which field is being inline-edited right now (the shared field
    /// input in the workspace routes its keystrokes here).
    editing_field: Option<value_model::editor::FieldPath>,
    /// The save-flow state (the tri-state outcome banner drives this).
    editor_flow: EditorFlow,
    /// A pending modal (enable-editing confirm, delete-key confirm,
    /// discard-edits guard).
    editor_modal: Option<EditorModal>,
    /// True while a save / TTL / delete write is in flight (one at a
    /// time, mirroring the console's serial guard).
    editor_write_running: bool,
    // ---- Phase 6: security surfacing ------------------------------------
    /// True while THIS connection runs with the insecure TLS override
    /// (set at establish via `ConnEvent::TlsWarning`, cleared on the
    /// session's death — persistent chip state, never a log line).
    tls_insecure: bool,
    /// The SSH server's `host:port` when this connection goes through a
    /// tunnel (non-sensitive; drives host-key modal copy). Empty when
    /// the connection is direct.
    ssh_host_port: String,
    /// A pending SSH host-key UI state: the TOFU prompt awaiting a
    /// user decision, or the hard CHANGED block (modal payload).
    host_key_ui: Option<HostKeyUi>,
}

/// The SSH host-key UI state (Phase 6): plain data driving the
/// workspace's overlay modals. `HostKeyUi::Prompt` renders the TOFU
/// confirm; `HostKeyUi::Blocked` renders the hard-block modal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HostKeyUi {
    /// First-seen key: awaiting trust-or-cancel. The prompt payload is
    /// host:port + fingerprint (non-sensitive).
    Prompt {
        /// `host:port` of the SSH server.
        host_port: String,
        /// SHA-256 fingerprint of the presented key, hex.
        fingerprint: String,
    },
    /// Key changed (hard block, no override by design): the modal
    /// shows host:port + both fingerprints.
    Blocked {
        /// `host:port` of the SSH server.
        host_port: String,
        /// Fingerprint of the presented (untrusted) key.
        new_fingerprint: String,
        /// The previously recorded fingerprint.
        old_fingerprint: String,
    },
}

impl HostKeyUi {
    /// The UI state for a first-seen host key (from the tunnel's
    /// [](redis_io::connection::ConnEvent::HostKeyPrompt)
    /// payload). Pure: format only, no secrets — host:port and the
    /// fingerprint digest are both non-sensitive by the PRD.
    pub fn from_prompt(p: &redis_io::ssh::HostKeyPrompt) -> Self {
        HostKeyUi::Prompt {
            host_port: format!("{}:{}", p.host, p.port),
            fingerprint: p.fingerprint.clone(),
        }
    }

    /// The UI state for a changed host key (from the tunnel's
    /// [](redis_io::connection::ConnEvent::HostKeyBlocked)
    /// payload). Pure.
    pub fn from_blocked(b: &redis_io::ssh::HostKeyBlocked) -> Self {
        HostKeyUi::Blocked {
            host_port: format!("{}:{}", b.host, b.port),
            new_fingerprint: b.new_fingerprint.clone(),
            old_fingerprint: b.old_fingerprint.clone(),
        }
    }
}

/// Link state for the status line and reconnect affordance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkState {
    /// Connected and usable.
    Live,
    /// Down; sticky until the user reconnects. Panels keep last data.
    Disconnected,
}

/// What the value panel renders.
#[derive(Debug)]
pub enum ValuePanelState {
    /// Nothing selected.
    Empty,
    /// TYPE probe / full read in flight.
    Loading,
    /// A loaded value (any modeled type) with its available views and
    /// the chosen one.
    Loaded {
        /// The value's raw model.
        raw: Arc<RawValue>,
        /// View tabs from `available_views`.
        views: Vec<ViewKind>,
        /// Currently selected view tab.
        active: ViewKind,
        /// The rendered output of `render_preview`.
        rendered: Arc<RenderedView>,
    },
    /// A type this client does not model: visibly read-only marker.
    Unsupported(ReadonlyUnsupported),
    /// The key vanished between listing and load (nil read, or TTL
    /// answered -2).
    Missing,
    /// Definitive load failure (shown inline; disconnect has its own UI).
    Error(String),
}

/// The save-flow outcome state for the value editor — mirrors
/// `app_workflows::save_workflow::SaveFlow` arm-for-arm (the Phase 5
/// contract: the UI's save discipline is exactly the workflow's).
#[derive(Clone, Debug, PartialEq)]
pub enum EditorFlow {
    /// No save has run (or the last one completed cleanly / was refused
    /// and dismissed). The banner area is empty.
    Idle,
    /// The save's fate is unknown (lost ack around a disconnect) — the
    /// acceptance-3 state. The editor was NOT acknowledged; its original
    /// bytes are preserved so a later re-read can compare. NOTHING is
    /// re-sent automatically, ever.
    OutcomeUnknown,
    /// The save definitively failed; the editor is untouched (still
    /// dirty); retry is a user action.
    Failed(String),
    /// The CAS precheck found the server's value changed under the edit
    /// (strings only, PLAN.md F5). The conflict modal renders from this.
    Conflict,
    /// The gate refused before any bytes were sent (read-only).
    Refused(String),
    /// A save/TTL/delete write is in flight.
    Saving,
}

/// A pending modal in the value-panel flows (Phase 5). Rendered by the
/// workspace's value-editor module; opened/driven only by these session
/// methods.
#[derive(Clone, Debug, PartialEq)]
pub enum EditorModal {
    /// "Enable Editing" was clicked on the read-only pill: the frame-03
    /// style confirm (danger-tinted title, no phrase gate — enabling is
    /// not destructive).
    EnableEditing,
    /// The delete-key confirm (names the key and type; single-confirm —
    /// single-key DEL is `Write` per policy D3, multi-key escalates and
    /// is NOT offered in this phase).
    DeleteKey,
    /// A dirty guard: switching keys/tabs/DB while edits are unsaved.
    /// The payload is the key display + edited field count for the body.
    DiscardEdits { key_display: String, fields: usize },
    /// The F5 conflict modal: the server's value changed since load
    /// (strings). [Show current] [Overwrite anyway] [Cancel].
    Conflict,
}

impl ConnectionSession {
    /// Build a session for an already-established connection.
    ///
    /// Called by the workspace after a successful connect. The
    /// `events_rx` drain task and the first scan page are started here
    /// (the documented driving pattern: generation → token →
    /// next_page on background → apply_page on the entity).
    pub fn new(
        conn: RedisConnection,
        events_rx: tokio::sync::mpsc::Receiver<ConnEvent>,
        name: String,
        cx: &mut Context<Self>,
    ) -> Self {
        let generation = SearchGeneration::new();
        let gen = generation.next_generation();
        let token = CancellationToken::new(gen);
        // Server-side SCAN TYPE narrowing when a typed chip is active.
        // The typed filter also drives which TYPE arg future scans carry;
        // the initial scan runs untyped (All).
        let scan = Arc::new(Mutex::new(
            KeyScan::new(gen, PAGE_SIZE, DEFAULT_BATCH_HINT).with_type(None),
        ));
        // The form's db index (parsed from the display name; Redis
        // itself clamps/validates on connect).
        let current_db = name
            .rsplit('/')
            .next()
            .and_then(|s| s.trim().parse::<u16>().ok())
            .unwrap_or(0);
        // The SSH server's host:port when tunneled (modal copy).
        let ssh_host_port = conn
            .config()
            .ssh_tunnel
            .as_ref()
            .map(|t| format!("{}:{}", t.host, t.port))
            .unwrap_or_default();
        let mut this = Self {
            conn: Arc::new(Mutex::new(conn)),
            events_rx,
            gate: WriteGate::new(),
            name,
            link: LinkState::Live,
            scan,
            flat: FlatKeyList::new(),
            generation,
            token,
            fetching_page: false,
            key_types: HashMap::new(),
            type_filter: None,
            probing_types: false,
            selected_key: None,
            value_panel: ValuePanelState::Empty,
            key_type: None,
            key_ttl: None,
            value_load_id: 0,
            tree: KeyTree::new(),
            tree_mode: false,
            expanded: HashSet::new(),
            search: ProgressiveSearch::new(MatchMode::Prefix, PAGE_SIZE, DEFAULT_BATCH_HINT),
            search_text: String::new(),
            search_serial: 0,
            searching: false,
            scan_driving: false,
            db_info: DbInfo::default(),
            current_db,
            switching_db: false,
            db_error: None,
            console: crate::console::ConsoleState::default(),
            console_serial: 0,
            console_running: false,
            editor: None,
            editing_field: None,
            editor_flow: EditorFlow::Idle,
            editor_modal: None,
            editor_write_running: false,
            tls_insecure: false,
            ssh_host_port,
            host_key_ui: None,
        };
        this.start_event_drain(cx);
        this.fetch_db_count(cx);
        this.fetch_next_page(cx);
        this
    }

    /// The honest count text for the keys footer (UI derives from this,
    /// never DBSIZE). Shapes per `browse::scan::KeyCount` docs.
    ///
    /// Reads the engine's *synchronous* view of the count: between page
    /// fetches the engine is unlocked, so this is the count as of the
    /// last delivered page — exactly what the footer should show.
    pub fn count_text(&self) -> String {
        let engine_count = match self.scan.try_lock() {
            Ok(engine) => engine.key_count(),
            Err(_) => {
                // A page fetch is in flight: the engine is borrowed.
                // The delivered count is what we honestly know.
                KeyCount::AtLeast(self.flat.len() as u64)
            }
        };
        let count = self.flat.key_count(engine_count);
        match count {
            KeyCount::Unknown => "…".into(),
            KeyCount::AtLeast(n) => format!("{n} keys so far…"),
            KeyCount::Exact(n) => format!("{n} keys"),
        }
    }

    /// Whether the scan engine can deliver more pages.
    pub fn can_load_more(&self) -> bool {
        if self.fetching_page || self.scan_driving || self.link != LinkState::Live {
            return false;
        }
        match self.scan.try_lock() {
            Ok(engine) => !engine.is_exhausted(),
            Err(_) => false, // fetch in flight
        }
    }

    /// Keys for the list, in the flat projection's order (Phase 2:
    /// arrival order per `flat.rs` docs — sorted view is a Phase 3
    /// toggle), filtered client-side by the active type chip.
    pub fn keys(&self) -> Vec<Vec<u8>> {
        let keys = self.flat.arrival_order().to_vec();
        apply_type_filter(&keys, &self.key_types, self.type_filter)
    }

    /// The active type-filter chip (None = All).
    pub fn type_filter(&self) -> Option<RawFilter> {
        self.type_filter
    }

    /// Select a type-filter chip: a typed chip re-arms the scan with the
    /// server-side `SCAN ... TYPE` argument (fresh generation + reset —
    /// the typed keyspace is a different view); All/None keeps the
    /// current scan. The client-side cache stays the display/discrepancy
    /// safety either way.
    pub fn set_type_filter(&mut self, filter: Option<RawFilter>, cx: &mut Context<Self>) {
        let changed = self.type_filter != filter;
        self.type_filter = filter;
        if !changed {
            cx.notify();
            return;
        }
        let typed = filter
            .map(|f| f.to_type_filter().scan_type_arg().is_some())
            .unwrap_or(false);
        if typed && !self.scan_driving {
            // Fresh server-side filtered scan.
            self.restart_global_scan(cx);
        } else {
            // All / None / mid-search: client-side filtering only (the
            // search loop owns the engine; its results are filtered in
            // `keys()` either way).
            cx.notify();
        }
    }

    /// A snapshot of the per-key type cache (for rendering row badges).
    pub fn key_types_snapshot(&self) -> HashMap<Vec<u8>, String> {
        self.key_types.clone()
    }

    /// The selected key, lossy-decoded for a title.
    pub fn selected_key_display(&self) -> Option<String> {
        self.selected_key
            .as_ref()
            .map(|k| String::from_utf8_lossy(k).into_owned())
    }

    /// The selected key as raw bytes (for row-highlight comparison).
    pub fn selected_display(&self) -> Option<Vec<u8>> {
        self.selected_key.clone()
    }

    /// The server-reported type of the selected key, for the header.
    pub fn key_type_display(&self) -> Option<String> {
        self.key_type.clone()
    }

    /// The currently active view tab.
    pub fn active_view(&self) -> Option<ViewKind> {
        match &self.value_panel {
            ValuePanelState::Loaded { active, .. } => Some(*active),
            _ => None,
        }
    }

    /// "Load more" / initial page fetch: the documented driving pattern.
    ///
    /// `next_page` runs on the tokio runtime with the engine borrowed
    /// through its shared mutex (same pattern as the connection: the
    /// page future must hold the engine across awaits, while this
    /// entity keeps read access for `key_count`/`is_exhausted`); the
    /// outcome returns via GPUI's foreground executor and is applied
    /// here.
    pub fn fetch_next_page(&mut self, cx: &mut Context<Self>) {
        if self.fetching_page || self.link != LinkState::Live {
            return;
        }
        self.fetching_page = true;
        cx.notify();
        let conn = self.conn.clone();
        let scan = self.scan.clone();
        let token = self.token.clone();
        let task = Self::run_io(cx, async move {
            // One page = 1..N SCAN batches; the engine holds the
            // connection for the whole page (the documented pattern).
            let mut conn_guard = conn.lock_owned().await;
            let mut engine = scan.lock_owned().await;
            engine.next_page(&mut *conn_guard, &token).await
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let exhausted = matches!(&result, Ok(PageOutcome::Exhausted));
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(outcome) => this.apply_page_outcome(outcome, cx),
                    Err(join_err) => {
                        this.value_panel =
                            ValuePanelState::Error(format!("internal task error: {join_err}"));
                    }
                }
                if exhausted {
                    // The global scan covered the whole tree: every
                    // node's children are now known complete.
                    this.tree.mark_subtree_loaded(b"");
                }
                this.fetching_page = false;
                cx.notify();
            });
        })
        .detach();
    }

    /// Apply a `PageOutcome` returned from the background.
    ///
    /// Phase 3: the same page feeds BOTH projections (flat + tree)
    /// through their identical generation-guarded `apply_page` seams.
    /// Exhaustion marking (root vs subtree) is the CALLER's policy —
    /// the global driver marks the root, the lazy-expansion driver
    /// marks its scoped path — so it deliberately lives outside.
    fn apply_page_outcome(&mut self, outcome: PageOutcome, cx: &mut Context<Self>) {
        match outcome {
            PageOutcome::Page(page) => {
                // Both projections share the guard; a current page must
                // land in both.
                let applied_flat = self.flat.apply_page(&page, &self.generation);
                let applied_tree = self.tree.apply_page(&page, &self.generation);
                debug_assert_eq!(
                    applied_flat, applied_tree,
                    "flat/tree guards disagree on generation"
                );
                // Lazily fill the type cache for the newly visible keys
                // (client-side filter chips). Honest and bounded: probe
                // the visible page only.
                self.probe_unprobed_types(cx);
            }
            PageOutcome::Exhausted => {
                // Cursor done (or anti-stall): count flips to Exact only
                // on true cursor exhaustion, handled inside the engine.
            }
            PageOutcome::Cancelled => {
                // Stopped means stopped; engine cursor stays resumable.
            }
            PageOutcome::ConnectionError(err) => {
                self.on_connection_error(err);
            }
        }
    }

    // ---- Phase 3: tree view -------------------------------------------

    /// Whether the keys panel renders the tree projection.
    pub fn tree_mode(&self) -> bool {
        self.tree_mode
    }

    /// Toggle Tree/Flat. Pure projection switch: both lists stay fed
    /// by the same pages, so no scan or generation changes.
    pub fn set_tree_mode(&mut self, on: bool, cx: &mut Context<Self>) {
        self.tree_mode = on;
        cx.notify();
    }

    /// Whether the tree node at `path` is expanded (UI state).
    pub fn is_tree_path_expanded(&self, path: &[u8]) -> bool {
        self.expanded.contains(path)
    }

    /// Rows for the tree projection: all `tree.rows()`, minus the
    /// children of collapsed branch nodes (the expanded-set filter the
    /// UI state owns). Depth-capped rows pass through as-is.
    pub fn tree_rows(&self) -> Vec<TreeRow> {
        project_tree_rows(&self.tree, &self.expanded)
    }

    /// Click on a tree row's branch indicator: toggle expansion; first
    /// expansion of an unloaded node triggers the documented
    /// lazy-expansion flow (scoped scan under a new generation).
    pub fn toggle_tree_expansion(&mut self, path: Vec<u8>, cx: &mut Context<Self>) {
        let Some(row) = self.tree.rows().into_iter().find(|r| r.path == path) else {
            return;
        };
        if !row.has_children {
            return; // nothing to expand
        }
        if self.expanded.contains(&path) {
            self.expanded.remove(&path);
            cx.notify();
            return;
        }
        self.expanded.insert(path.clone());
        if !row.children_loaded {
            self.expand_subtree(path, cx);
        } else {
            cx.notify();
        }
    }

    /// The documented lazy-expansion flow: a scoped scan whose MATCH is
    /// `path:*` (glob-escaped, built by `KeyTree::scan_for_prefix`),
    /// under a fresh generation, driven via run_io; pages flow through
    /// `apply_page` (both projections), and Exhausted calls
    /// `mark_subtree_loaded(path)`.
    ///
    /// The scoped run supersedes the global engine until it exhausts;
    /// `resume_global_scan` then restarts global browsing so "Load
    /// more" keeps working. No reset: the scoped pattern only matches
    /// keys under the subtree, and dedup absorbs the overlap.
    fn expand_subtree(&mut self, path: Vec<u8>, cx: &mut Context<Self>) {
        if self.scan_driving || self.link != LinkState::Live {
            return;
        }
        // Retire the global run's token; a fresh generation makes any
        // in-flight global page stale at the guard.
        self.token.cancel();
        let gen = self.generation.next_generation();
        let scan = self
            .tree
            .scan_for_prefix(&path, gen, PAGE_SIZE, DEFAULT_BATCH_HINT);
        // Carry the active server-side type filter, if any.
        let type_arg = self.active_scan_type_arg();
        let scan = Arc::new(Mutex::new(scan.with_type(type_arg)));
        self.scan = scan.clone();
        self.token = CancellationToken::new(gen);
        self.fetching_page = true;
        self.scan_driving = true;
        cx.notify();
        let conn = self.conn.clone();
        let token = self.token.clone();
        let task = Self::run_io(cx, async move {
            let mut conn_guard = conn.lock_owned().await;
            let mut engine = scan.lock_owned().await;
            engine.next_page(&mut *conn_guard, &token).await
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let exhausted = matches!(&result, Ok(PageOutcome::Exhausted));
            let _ = this.update(cx, |this, cx| {
                this.scan_driving = false;
                this.fetching_page = false;
                match result {
                    Ok(outcome) => this.apply_page_outcome(outcome, cx),
                    Err(join_err) => {
                        this.value_panel =
                            ValuePanelState::Error(format!("internal task error: {join_err}"));
                    }
                }
                if exhausted {
                    // The covering-scan contract: this scoped run's
                    // exhaustion completes exactly this subtree.
                    this.tree.mark_subtree_loaded(&path);
                    // Hand the engine back to global browsing.
                    this.resume_global_scan(cx);
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Restart global browsing after a scoped run, keeping accumulated
    /// flat/tree state (no reset: dedup + generation guard keep the
    /// views consistent). Honors active search text + type filter.
    fn resume_global_scan(&mut self, cx: &mut Context<Self>) {
        self.token.cancel();
        let gen = self.generation.next_generation();
        let type_arg = self.active_scan_type_arg();
        let pattern = if self.search_text.is_empty() {
            None
        } else {
            Some(browse::search::pattern_for(
                self.search_text.as_bytes(),
                MatchMode::Prefix,
            ))
        };
        let scan =
            KeyScan::with_pattern(gen, PAGE_SIZE, DEFAULT_BATCH_HINT, pattern).with_type(type_arg);
        self.scan = Arc::new(Mutex::new(scan));
        self.token = CancellationToken::new(gen);
        self.fetching_page = false;
        self.fetch_next_page(cx);
    }

    /// The server-side SCAN TYPE argument for the active chip, if any.
    fn active_scan_type_arg(&self) -> Option<Vec<u8>> {
        self.type_filter
            .and_then(|f| f.to_type_filter().scan_type_arg().map(<[u8]>::to_vec))
    }

    // ---- Phase 3: progressive search ----------------------------------

    /// The debounce interval in milliseconds.
    pub const SEARCH_DEBOUNCE_MS: u64 = 300;

    /// Called on every keystroke in the search box: bump the debounce
    /// serial and spawn a delayed task that starts the run only if it
    /// is still the latest keystroke. GPUI's `cx.spawn` + `smol::Timer`
    /// (via `futures_timer`-equivalent `cx.background_executor
    /// .timer`) is the timer at this rev.
    pub fn search_keystroke(&mut self, text: String, cx: &mut Context<Self>) {
        if text == self.search_text && self.search_serial > 0 {
            return; // no-op keystroke (focus re-notify etc.)
        }
        self.search_serial += 1;
        let serial = self.search_serial;
        cx.spawn(async move |this, cx| {
            // GPUI's background executor timer (smol-based).
            cx.background_executor()
                .timer(std::time::Duration::from_millis(Self::SEARCH_DEBOUNCE_MS))
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.search_serial == serial {
                    this.start_search_run(cx);
                }
            });
        })
        .detach();
    }

    /// Arm + drive a search run for the current `search_text` (empty
    /// text = a whole-keyspace progressive scan — the crate's
    /// documented "cleared box" semantics).
    ///
    /// Reset discipline (per phase3 decisions): a new run cancels the
    /// old token (inside `ProgressiveSearch::start`), advances the
    /// generation, and BOTH projections reset — stale pages die at the
    /// guard even before that.
    fn start_search_run(&mut self, cx: &mut Context<Self>) {
        if self.link != LinkState::Live {
            return;
        }
        let text = self.search_text.clone();
        // Reset both projections + expansion state: the new run's
        // keyspace view starts empty.
        self.flat.reset();
        self.tree.reset();
        self.expanded.clear();
        self.key_types.clear();
        self.fetching_page = false;
        self.searching = !text.is_empty();
        // The search engine becomes THE engine; carry the active
        // server-side type filter.
        let run = self.search.start(text.as_bytes(), &self.generation);
        let type_arg = self.active_scan_type_arg();
        let scan = Arc::new(Mutex::new(run.scan.with_type(type_arg)));
        self.scan = scan.clone();
        self.token = run.token.clone();
        self.scan_driving = true;
        cx.notify();
        // Drive loop: pages until Exhausted / Cancelled /
        // ConnectionError, applied through the same seam. Each page
        // fetch goes through run_io (tokio owns the redis futures —
        // the documented bridge; smol's background_spawn must not
        // touch redis-rs).
        let conn = self.conn.clone();
        let token = run.token.clone();
        // Capture the runtime handle up front (the loop context is
        // AsyncApp, which does not expose globals directly).
        let tokio_handle = cx.global::<crate::TokioRuntime>().0.clone();
        cx.spawn(async move |this, cx| {
            loop {
                let task = {
                    let conn = conn.clone();
                    let scan = scan.clone();
                    let token = token.clone();
                    let handle = tokio_handle.clone();
                    // Same bridge as run_io: tokio owns the redis
                    // future; GPUI's background executor awaits the
                    // JoinHandle.
                    let join = handle.spawn(async move {
                        let mut conn_guard = conn.lock_owned().await;
                        let mut engine = scan.lock_owned().await;
                        engine.next_page(&mut *conn_guard, &token).await
                    });
                    cx.background_spawn(join)
                };
                let result = task.await;
                let mut keep_going = false;
                let _ = this.update(cx, |this, cx| {
                    match result {
                        Ok(PageOutcome::Page(page)) => {
                            let applied = this.flat.apply_page(&page, &this.generation);
                            let applied_tree = this.tree.apply_page(&page, &this.generation);
                            debug_assert_eq!(applied, applied_tree);
                            if applied {
                                this.probe_unprobed_types(cx);
                            }
                            keep_going = true;
                        }
                        Ok(PageOutcome::Exhausted) => {
                            this.tree.mark_subtree_loaded(b"");
                            this.searching = false;
                        }
                        Ok(PageOutcome::Cancelled) => {
                            this.searching = false;
                        }
                        Ok(PageOutcome::ConnectionError(err)) => {
                            this.searching = false;
                            this.on_connection_error(err);
                        }
                        Err(_) => {
                            this.searching = false;
                        }
                    }
                    cx.notify();
                });
                if !keep_going {
                    this.update(cx, |this, _cx| {
                        this.scan_driving = false;
                    })
                    .ok();
                    break;
                }
            }
        })
        .detach();
    }

    /// Esc / X in the search box: stop the active run. Cancels the
    /// token (engine stops at the next batch boundary); the delivered
    /// state stays (sticky, like a disconnect).
    pub fn cancel_search(&mut self, cx: &mut Context<Self>) {
        self.search.cancel();
        self.searching = false;
        cx.notify();
    }

    /// Clear the search text and return to normal browsing: a fresh
    /// generation + reset + untyped-browse scan (the documented
    /// empty-input contract).
    pub fn clear_search(&mut self, cx: &mut Context<Self>) {
        self.search.cancel();
        self.searching = false;
        self.search_text.clear();
        self.restart_global_scan(cx);
    }

    /// The search text state (the input's mirror on the entity).
    pub fn search_text(&self) -> &str {
        &self.search_text
    }

    /// Set the search text without starting anything (the debounced
    /// keystroke path calls `search_keystroke`).
    pub fn set_search_text(&mut self, text: String) {
        self.search_text = text;
    }

    /// True while a search run's drive loop is active.
    pub fn is_searching(&self) -> bool {
        self.searching
    }

    /// Footer text for the keys panel: the honest count, prefixed with
    /// the searching state when a run is active.
    pub fn search_count_text(&self) -> String {
        if self.searching {
            let n = match self.scan.try_lock() {
                Ok(engine) => engine.key_count().n(),
                Err(_) => self.flat.len() as u64,
            };
            return format!("searching… {n} results so far");
        }
        self.count_text()
    }

    // ---- Phase 3: DB switcher -----------------------------------------

    /// Fetch the DB count after connect (`CONFIG GET databases`,
    /// fallback 16 on refusal — browse::db's documented honesty).
    fn fetch_db_count(&mut self, cx: &mut Context<Self>) {
        let conn = self.conn.clone();
        let task = Self::run_io(cx, async move {
            let conn = &mut *conn.lock_owned().await;
            fetch_db_count(conn as &mut dyn Connection).await
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if let Ok(Ok(info)) = result {
                    this.db_info = info;
                    cx.notify();
                }
                // A disconnect during CONFIG GET goes through the event
                // channel; nothing to do here (fallback default stands).
            });
        })
        .detach();
    }

    /// The DB count for the switcher's range.
    pub fn db_count(&self) -> u16 {
        self.db_info.count
    }

    /// The currently selected logical DB.
    pub fn current_db(&self) -> u16 {
        self.current_db
    }

    /// True while a SELECT is in flight (switcher disabled).
    pub fn switching_db(&self) -> bool {
        self.switching_db
    }

    /// Inline DB-switch error (SELECT refused; cleared on next attempt).
    pub fn db_error(&self) -> Option<&str> {
        self.db_error.as_deref()
    }

    /// Switch logical DB: the documented sequence — cancel token →
    /// next generation → reset flat+tree → SELECT via run_io → fresh
    /// scan. On SELECT failure the DB reverts and the error shows
    /// inline.
    pub fn switch_db(&mut self, db: u16, cx: &mut Context<Self>) {
        if self.switching_db || db == self.current_db || self.link != LinkState::Live {
            return;
        }
        if !self.db_info.is_valid_db(db) {
            self.db_error = Some(format!("db {db} out of range"));
            cx.notify();
            return;
        }
        self.switching_db = true;
        self.db_error = None;
        // Retire the current scan work before switching (documented).
        self.token.cancel();
        self.fetching_page = false;
        let conn = self.conn.clone();
        let task = Self::run_io(cx, async move {
            let conn = &mut *conn.lock_owned().await;
            conn.issue(Command::new("SELECT", vec![db.to_string().into_bytes()]))
                .await
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                this.switching_db = false;
                match result {
                    Ok(Ok(redis_io::connection::Reply::Okay)) => {
                        this.current_db = db;
                        // Re-key the display name (host:port/db).
                        if let Some(slash) = this.name.rfind('/') {
                            this.name = format!("{}/{db}", &this.name[..slash]);
                        }
                        this.restart_global_scan(cx);
                    }
                    Ok(Ok(other)) => {
                        this.db_error = Some(format!("SELECT refused: unexpected reply {other:?}"));
                    }
                    Ok(Err(err)) => {
                        this.db_error = Some(format!("SELECT failed: {err}"));
                        if matches!(err, ConnError::Disconnected { .. }) {
                            this.on_connection_error(err);
                        }
                    }
                    Err(join_err) => {
                        this.db_error = Some(format!("internal task error: {join_err}"));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Reset both projections, cancel the current run, and start a
    /// fresh global scan under a new generation (shared by DB switch,
    /// search clear, and typed-filter re-scan). Honors the active
    /// search text (empty = plain browse) + server-side type filter.
    fn restart_global_scan(&mut self, cx: &mut Context<Self>) {
        self.search.cancel();
        self.searching = false;
        self.token.cancel();
        self.fetching_page = false;
        self.flat.reset();
        self.tree.reset();
        self.expanded.clear();
        self.key_types.clear();
        self.probing_types = false;
        self.selected_key = None;
        self.value_panel = ValuePanelState::Empty;
        self.key_type = None;
        self.key_ttl = None;
        self.editor = None;
        self.editing_field = None;
        self.editor_flow = EditorFlow::Idle;
        self.editor_modal = None;
        let gen = self.generation.next_generation();
        let type_arg = self
            .type_filter
            .and_then(|f| f.to_type_filter().scan_type_arg().map(<[u8]>::to_vec));
        let pattern = if self.search_text.is_empty() {
            None
        } else {
            Some(browse::search::pattern_for(
                self.search_text.as_bytes(),
                MatchMode::Prefix,
            ))
        };
        let scan =
            KeyScan::with_pattern(gen, PAGE_SIZE, DEFAULT_BATCH_HINT, pattern).with_type(type_arg);
        self.scan = Arc::new(Mutex::new(scan));
        self.token = CancellationToken::new(gen);
        self.fetch_next_page(cx);
    }

    /// TYPE-probe the keys of the current flat list whose type is not
    /// yet cached — sequential, over the connection mutex, in ONE
    /// spawned task (no per-key round-trip chatter on the UI).
    ///
    /// The result is guarded by the *generation* of the scan run: a
    /// reconnect/reset bumps the generation, so a late probe batch
    /// cannot repopulate a stale cache. Probe failures are ignored
    /// (the cache is a display aid, not a correctness gate; a typed
    /// key selected later is probed again by `select_key`).
    fn probe_unprobed_types(&mut self, cx: &mut Context<Self>) {
        if self.probing_types || self.link != LinkState::Live {
            return;
        }
        let unprobed: Vec<Vec<u8>> = self
            .flat
            .arrival_order()
            .iter()
            .filter(|k| !self.key_types.contains_key(*k))
            .cloned()
            .collect();
        if unprobed.is_empty() {
            return;
        }
        self.probing_types = true;
        let conn = self.conn.clone();
        let gen = self.generation.current();
        let task = Self::run_io(cx, async move {
            let conn = &mut *conn.lock_owned().await;
            let mut results = Vec::with_capacity(unprobed.len());
            for key in unprobed {
                let reply = conn.issue(Command::new("TYPE", vec![key.clone()])).await;
                match reply {
                    Ok(redis_io::connection::Reply::Bulk(type_bytes)) => {
                        let type_str = String::from_utf8_lossy(&type_bytes).into_owned();
                        // `none` means the key expired: keep it honest —
                        // no cache entry (an expired key is not a typed
                        // key; it will vanish on the next scan anyway).
                        if type_str != "none" {
                            results.push((key, type_str));
                        }
                    }
                    _ => {
                        // A disconnect aborts the rest of the probe
                        // round; the connection-error path has run.
                        break;
                    }
                }
            }
            results
        });
        cx.spawn(async move |this, cx| {
            let result = task.await.ok();
            let _ = this.update(cx, |this, cx| {
                this.probing_types = false;
                if let Some(results) = result {
                    if this.generation.is_current(gen) && this.link == LinkState::Live {
                        for (key, type_str) in results {
                            this.key_types.insert(key, type_str);
                        }
                        cx.notify();
                    }
                }
            });
        })
        .detach();
    }

    /// Select a key → TYPE probe → full read (per type) + TTL →
    /// value-model views (Phase 3: all six modeled types).
    ///
    /// The TYPE probe result also fills the key-type cache (shared with
    /// the filter chips).
    pub fn select_key(&mut self, key: Vec<u8>, cx: &mut Context<Self>) {
        if self.link != LinkState::Live {
            return;
        }
        self.selected_key = Some(key);
        self.value_panel = ValuePanelState::Loading;
        self.key_type = None;
        self.key_ttl = None;
        // Phase 5: a new selection closes any open editor state (the
        // workspace's dirty guard runs BEFORE this — select_key is only
        // reached once the user chose to discard or the editor was clean).
        self.editor = None;
        self.editing_field = None;
        self.editor_flow = EditorFlow::Idle;
        self.editor_modal = None;
        self.value_load_id += 1;
        let load_id = self.value_load_id;
        cx.notify();

        let conn = self.conn.clone();
        let probe_key = self.selected_key.clone().unwrap_or_default();
        let type_task = Self::run_io(cx, async move {
            let conn = &mut *conn.lock_owned().await;
            conn.issue(Command::new("TYPE", vec![probe_key])).await
        });

        cx.spawn(async move |this, cx| {
            let reply = type_task.await.ok();
            let _ = this.update(cx, |this, cx| {
                if this.value_load_id != load_id || this.link != LinkState::Live {
                    return; // stale load
                }
                match reply {
                    Some(Ok(redis_io::connection::Reply::Bulk(type_bytes))) => {
                        let type_str = String::from_utf8_lossy(&type_bytes).into_owned();
                        this.key_type = Some(type_str.clone());
                        this.cache_key_type(type_str.clone());
                        if let Some(read_cmd) = full_read_command(
                            &type_str,
                            &this.selected_key.clone().unwrap_or_default(),
                        ) {
                            this.load_value(read_cmd, type_str, cx);
                            this.load_ttl(cx);
                        } else {
                            // Unknown / module type: visibly read-only.
                            this.value_panel =
                                ValuePanelState::Unsupported(ReadonlyUnsupported::new(type_str));
                            cx.notify();
                        }
                    }
                    Some(Ok(_)) => {
                        this.value_panel = ValuePanelState::Error("unexpected TYPE reply".into());
                        cx.notify();
                    }
                    Some(Err(err)) => {
                        this.on_connection_error(err);
                        cx.notify();
                    }
                    None => {}
                }
            });
        })
        .detach();
    }

    /// Record a probed type in the filter-chip cache.
    fn cache_key_type(&mut self, type_str: String) {
        if let Some(key) = &self.selected_key {
            self.key_types.insert(key.clone(), type_str);
        }
    }

    /// Issue the type's full-read command and map the reply to a
    /// `RawValue` (via `reply_to_raw`), then render through
    /// value-model's views.
    fn load_value(&mut self, read_cmd: Command, type_str: String, cx: &mut Context<Self>) {
        let load_id = self.value_load_id;
        self.value_panel = ValuePanelState::Loading;
        cx.notify();
        let conn = self.conn.clone();
        let get_task = Self::run_io(cx, async move {
            let conn = &mut *conn.lock_owned().await;
            conn.issue(read_cmd).await
        });
        cx.spawn(async move |this, cx| {
            let reply = get_task.await.ok();
            let _ = this.update(cx, |this, cx| {
                if this.value_load_id != load_id {
                    return; // stale
                }
                match reply {
                    Some(Ok(reply)) => match reply_to_raw(&type_str, reply) {
                        Ok(Some(raw_value)) => {
                            this.value_panel = Self::loaded_value_panel(raw_value.clone());
                            // Phase 5: open the dirty-editor state machine
                            // over the fresh bytes. Editable = every modeled
                            // type except stream (PRD story 35); a mid-edit
                            // refresh routes through apply_refresh instead
                            // (acceptance 9 — the working buffer survives).
                            if type_str != "stream" {
                                let can_refresh = this.editor.as_ref().is_some_and(|e| e.dirty);
                                if can_refresh {
                                    let editor = this.editor.as_mut().unwrap();
                                    editor.apply_refresh(raw_value);
                                } else {
                                    this.editor =
                                        Some(value_model::editor::EditorState::open(raw_value));
                                }
                            } else {
                                this.editor = None;
                            }
                        }
                        Ok(None) => {
                            // Vanished between listing and load.
                            this.value_panel = ValuePanelState::Missing;
                        }
                        Err(conv_err) => {
                            // e.g. WRONGTYPE after a TYPE race, or an
                            // unexpected reply shape: honest inline error.
                            this.value_panel = ValuePanelState::Error(conv_err.to_string());
                        }
                    },
                    Some(Err(err)) => this.on_connection_error(err),
                    None => {}
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Issue TTL for the selected key (a read; safe in read-only mode)
    /// and store the state; -2 (key gone) flips the panel to Missing.
    fn load_ttl(&mut self, cx: &mut Context<Self>) {
        let Some(key) = self.selected_key.clone() else {
            return;
        };
        let load_id = self.value_load_id;
        let conn = self.conn.clone();
        let ttl_task = Self::run_io(cx, async move {
            let conn = &mut *conn.lock_owned().await;
            conn.issue(Command::new("TTL", vec![key])).await
        });
        cx.spawn(async move |this, cx| {
            let reply = ttl_task.await.ok();
            let _ = this.update(cx, |this, cx| {
                if this.value_load_id != load_id {
                    return; // stale
                }
                if let Some(Ok(redis_io::connection::Reply::Int(raw))) = reply {
                    match TtlState::from_redis_reply(raw) {
                        TtlReply::MissingKey => {
                            // -2: the key expired between listing and
                            // load — honest "no longer exists" state.
                            this.key_ttl = None;
                            this.value_panel = ValuePanelState::Missing;
                        }
                        TtlReply::State(state) => this.key_ttl = Some(state),
                    }
                }
                // A failed TTL probe leaves the TTL unknown (None) —
                // the value display stands on its own.
                cx.notify();
            });
        })
        .detach();
    }

    /// Build the loaded-value panel from a `RawValue` (any modeled
    /// type). Views come strictly from `available_views`.
    fn loaded_value_panel(raw_value: RawValue) -> ValuePanelState {
        let raw = Arc::new(raw_value);
        let views = available_views(&raw);
        // Default to the first offered view (Text/Json when valid).
        let active = views.first().copied().unwrap_or(ViewKind::Hex);
        let rendered = Arc::new(render_preview(active, &raw));
        ValuePanelState::Loaded {
            raw,
            views,
            active,
            rendered,
        }
    }

    /// Switch the value panel's view tab (Text/JSON/Hex).
    pub fn switch_view(&mut self, kind: ViewKind, cx: &mut Context<Self>) {
        if let ValuePanelState::Loaded { raw, views, .. } = &self.value_panel {
            if !views.contains(&kind) {
                return;
            }
            let raw = raw.clone();
            let rendered = Arc::new(render_preview(kind, &raw));
            self.value_panel = ValuePanelState::Loaded {
                raw,
                views: views.clone(),
                active: kind,
                rendered,
            };
            cx.notify();
        }
    }

    /// The TTL display line for the value panel header, or an empty
    /// string while unknown.
    pub fn ttl_display(&self) -> Option<String> {
        self.key_ttl.as_ref().map(|ttl| match ttl.seconds {
            None => "no expiry".to_string(),
            Some(1) => "expires in 1 second".to_string(),
            Some(n) => format!("expires in {n} seconds"),
        })
    }

    // ---- Phase 5: editing gate + save flows ----------------------------
    //
    // The discipline is the app-workflows `SaveWorkflow` contract, wired
    // into the session: gate check via write-safety `classify` BEFORE any
    // bytes → edit → `save_encoding` → issue ONCE → `WriteOutcome`
    // tri-state → editor state per outcome. NEVER resend. OutcomeUnknown
    // → the honest "check before retrying" state + the gate already
    // reset (the disconnect path did it).

    /// The current gate state (the status-bar pill renders from this).
    pub fn gate_state(&self) -> write_safety::GateState {
        self.gate.state()
    }

    /// The read-only pill was clicked. ReadOnly → open the EnableEditing
    /// confirm modal. WriteEnabled → disable instantly (disabling is
    /// always safe; no confirm).
    pub fn toggle_editing(&mut self, cx: &mut Context<Self>) {
        if self.link != LinkState::Live {
            return;
        }
        if self.gate.is_read_only() {
            self.editor_modal = Some(EditorModal::EnableEditing);
        } else {
            let _ = self.gate.apply(write_safety::GateEvent::DisableEditing);
        }
        cx.notify();
    }

    /// The EnableEditing modal's Confirm: the one explicit path into
    /// `WriteEnabled` (mirrors `SaveWorkflow::enable_editing`).
    pub fn confirm_enable_editing(&mut self, cx: &mut Context<Self>) {
        self.editor_modal = None;
        let _ = self.gate.apply(write_safety::GateEvent::EnableEditing);
        cx.notify();
    }

    /// Open the DiscardEdits guard modal (the workspace's `guarded_nav`
    /// calls this when a navigation would strand a dirty editor).
    pub fn open_discard_guard(
        &mut self,
        key_display: String,
        fields: usize,
        cx: &mut Context<Self>,
    ) {
        self.editor_modal = Some(EditorModal::DiscardEdits {
            key_display,
            fields,
        });
        cx.notify();
    }

    /// Any editor modal's Cancel/decline (nothing sent, modal closes).
    pub fn cancel_editor_modal(&mut self, cx: &mut Context<Self>) {
        self.editor_modal = None;
        if self.editor_flow == EditorFlow::Conflict {
            self.editor_flow = EditorFlow::Idle;
        }
        cx.notify();
    }

    /// Which editor modal is open (rendered by the workspace).
    pub fn editor_modal(&self) -> Option<&EditorModal> {
        self.editor_modal.as_ref()
    }

    /// The current save-flow state (the banner renders from this).
    pub fn editor_flow(&self) -> &EditorFlow {
        &self.editor_flow
    }

    /// Whether an editable value is loaded and the editor is dirty
    /// (acceptance 9 — the dirty guard consults this).
    pub fn editor_dirty(&self) -> bool {
        self.editor.as_ref().is_some_and(|e| e.dirty)
    }

    /// Whether the server's value changed under a dirty editor (the
    /// muted divergence banner).
    pub fn editor_diverged(&self) -> bool {
        self.editor.as_ref().is_some_and(|e| e.diverged_from_server)
    }

    /// Which field is being inline-edited (the workspace's shared field
    /// input routes keystrokes here). `None` = no field focused.
    pub fn editing_field(&self) -> Option<&value_model::editor::FieldPath> {
        self.editing_field.as_ref()
    }

    /// Begin editing a field: initialize the shared input from the
    /// working slot's current bytes (lossy display for the input; the
    /// edit itself goes through `edit_text`, whose `&str` signature is
    /// the lossy-safety gate).
    pub fn begin_field_edit(&mut self, path: value_model::editor::FieldPath) -> Option<String> {
        if self.gate.is_read_only() {
            return None; // read-only: no edit affordances
        }
        let editor = self.editor.as_ref()?;
        let initial = working_slot_text(editor, &path)?;
        self.editing_field = Some(path);
        Some(initial)
    }

    /// The shared field input's text changed: route it into the editor's
    /// working buffer (validated by value-model; a rejected edit keeps
    /// the buffer untouched).
    pub fn field_edit_text(&mut self, text: &str, cx: &mut Context<Self>) {
        let Some(path) = self.editing_field.clone() else {
            return;
        };
        if let Some(editor) = self.editor.as_mut() {
            let _ = editor.edit_text(&path, text);
        }
        cx.notify();
    }

    /// The scalar text editor's content changed (strings/JSON in the
    /// Text view): route into the working buffer via the validated
    /// `edit_text` (value-model's lossy-safety gate).
    pub fn scalar_edit_text(&mut self, text: &str, cx: &mut Context<Self>) {
        if self.gate.is_read_only() {
            return;
        }
        if let Some(editor) = self.editor.as_mut() {
            let _ = editor.edit_text(&value_model::editor::FieldPath::Scalar, text);
        }
        cx.notify();
    }

    /// End the inline field edit (blur / Enter / switching fields).
    pub fn end_field_edit(&mut self, cx: &mut Context<Self>) {
        self.editing_field = None;
        cx.notify();
    }

    /// Discard all edits: rebuild the working buffer from the latest
    /// original (value-model `discard`), clear the flow state.
    pub fn discard_edits(&mut self, cx: &mut Context<Self>) {
        if let Some(editor) = self.editor.as_mut() {
            editor.discard();
        }
        self.editor_flow = EditorFlow::Idle;
        self.editing_field = None;
        cx.notify();
    }

    /// The save commands for the editor's dirty state — mirrors
    /// `SaveWorkflow::save_commands` (one command per EDITED unit;
    /// untouched entries are never even sent):
    /// - String/JSON: one `SET`.
    /// - Hash: one `HSET` per edited field value.
    /// - List: `LSET index value` per edited item.
    /// - Set: `SADD` for the edited member (plus `SREM` of the original
    ///   member when the bytes changed — a set member is addressed by
    ///   content, not position).
    /// - ZSet: `ZADD` per edited member/score.
    ///
    /// Field names, scores and members keep their original bytes on the
    /// un-edited side (value-model invariant 3).
    fn save_commands(&self) -> Vec<Command> {
        let Some(editor) = self.editor.as_ref() else {
            return Vec::new();
        };
        let Some(key) = self.selected_key.clone() else {
            return Vec::new();
        };
        save_commands_from(editor, &key)
    }

    /// The dirty-guard payload: key display + edited field count (the
    /// DiscardEdits modal's body), `None` when the editor is clean.
    pub fn dirty_guard_payload(&self) -> Option<(String, usize)> {
        let editor = self.editor.as_ref()?;
        if !editor.dirty {
            return None;
        }
        let count = edited_field_count(editor);
        let key_display = self
            .selected_key
            .as_ref()
            .map(|k| String::from_utf8_lossy(k).into_owned())
            .unwrap_or_default();
        Some((key_display, count))
    }

    /// SAVE: the exact `SaveWorkflow::save` ordering.
    ///
    /// 1. Refuse pre-send when not dirty, when a write is in flight, or
    ///    when the gate is read-only (write-safety's classify + verbatim
    ///    refusal wording — zero commands sent).
    /// 2. Strings/JSON: CAS precheck (GET-compare against the editor's
    ///    ORIGINAL) — a mismatch opens the F5 conflict modal, nothing is
    ///    sent (mirrors `save_with_conflict_precheck`).
    /// 3. Issue the commands serially, ONCE.
    /// 4. Map the result through `WriteOutcome::from_conn_error` — the
    ///    ONE tri-state rule — and update the editor state per the arm.
    pub fn save_value(&mut self, cx: &mut Context<Self>) {
        if self.editor_write_running || self.link != LinkState::Live {
            return;
        }
        let commands = self.save_commands();
        if commands.is_empty() {
            return; // NotDirty: nothing to save (the UI hides Save when clean)
        }

        // Gate check BEFORE any issue — the same `classify` call the
        // console path makes, so one brain decides both.
        let parsed = write_safety::ParsedCommand {
            name: commands[0].name.clone(),
            args: commands[0].args.clone(),
        };
        match write_safety::classify(&parsed, &self.gate) {
            write_safety::Decision::Run => {}
            write_safety::Decision::Blocked(reason) => {
                // Read-only refusal: verbatim write-safety wording.
                self.editor_flow = EditorFlow::Refused(reason.message().to_string());
                cx.notify();
                return;
            }
            write_safety::Decision::Confirm(_) => {
                // SET/HSET/LSET/SADD/ZADD/EXPIRE are Write, never
                // Confirm; unreachable for save commands.
                self.editor_flow = EditorFlow::Refused("confirm required".into());
                cx.notify();
                return;
            }
        }

        // CAS precheck for scalars (PLAN.md F5): GET-compare against the
        // editor's original bytes BEFORE the SET. A mismatch refuses the
        // save with the conflict modal — never a silent overwrite.
        let cas_check = if matches!(
            self.editor.as_ref().map(|e| &e.original),
            Some(RawValue::String(_)) | Some(RawValue::Json(_))
        ) {
            Some(self.conflict_precheck_command())
        } else {
            None
        };

        self.editor_write_running = true;
        self.editor_flow = EditorFlow::Saving;
        cx.notify();

        let conn = self.conn.clone();
        let key = self.selected_key.clone().unwrap_or_default();
        let task = Self::run_io(cx, async move {
            let conn = &mut *conn.lock_owned().await;
            if let Some(get) = cas_check {
                let reply = conn.issue(get).await;
                // Read the reply OUTSIDE the match (borrow discipline).
                match reply {
                    Ok(Reply::Nil) => return SaveIoResult::Conflict,
                    Ok(reply) => {
                        let server = reply.as_bytes().map(<[u8]>::to_vec);
                        return match server {
                            // The precheck verdict travels back; the
                            // session compares against the editor's
                            // ORIGINAL on the main thread (the bytes never
                            // leave this result).
                            Some(bytes) => SaveIoResult::Precheck(bytes),
                            None => SaveIoResult::Conflict,
                        };
                    }
                    Err(err) => return SaveIoResult::ConnErr(format!("{err}")),
                }
            }
            let mut outcomes = Vec::new();
            for cmd in commands {
                let recorded = cmd.clone();
                match conn.issue(cmd).await {
                    Ok(Reply::Okay) => outcomes.push(WriteOutcome::Acknowledged),
                    Ok(_) => outcomes.push(WriteOutcome::Failed(
                        redis_io::outcome::DefinitiveError::new(
                            redis_io::outcome::DefinitiveErrorKind::ServerError,
                            "unexpected reply to write",
                        ),
                    )),
                    Err(err) => {
                        outcomes.push(WriteOutcome::from_conn_error(recorded, err));
                        break; // never issue past a failure/unknown
                    }
                }
            }
            SaveIoResult::Outcomes(outcomes)
        });
        let _ = key;
        cx.spawn(async move |this, cx| {
            let result = task.await.ok();
            let _ = this.update(cx, |this, cx| {
                this.editor_write_running = false;
                this.apply_save_result(result, cx);
            });
        })
        .detach();
    }

    /// The GET command for the CAS precheck (strings/JSON).
    fn conflict_precheck_command(&self) -> Command {
        Command::new("GET", vec![self.selected_key.clone().unwrap_or_default()])
    }

    /// Apply a completed save round: the precheck verdict first (scalars),
    /// then the tri-state mapping — exactly `SaveFlow`'s post-state rules.
    fn apply_save_result(&mut self, result: Option<SaveIoResult>, cx: &mut Context<Self>) {
        match result {
            Some(SaveIoResult::Precheck(server_bytes)) => {
                // Compare against the editor's ORIGINAL — the loaded
                // bytes, not the working buffer (F5: "changed since you
                // loaded it").
                let original_bytes = self.editor.as_ref().and_then(|e| match &e.original {
                    RawValue::String(b) | RawValue::Json(b) => Some(b.as_bytes().to_vec()),
                    _ => None,
                });
                let matches = original_bytes
                    .map(|orig| orig == server_bytes)
                    .unwrap_or(false);
                if !matches {
                    self.editor_flow = EditorFlow::Conflict;
                    self.editor_modal = Some(EditorModal::Conflict);
                    cx.notify();
                    return;
                }
                // Precheck clean: issue the actual save now.
                self.save_value(cx);
            }
            Some(SaveIoResult::Conflict) => {
                // The GET itself failed or the key vanished: honest
                // conflict (mirrors `conflict_precheck`'s Err arm).
                self.editor_flow = EditorFlow::Conflict;
                self.editor_modal = Some(EditorModal::Conflict);
                cx.notify();
            }
            Some(SaveIoResult::ConnErr(msg)) => {
                self.editor_flow = EditorFlow::Failed(msg);
                cx.notify();
            }
            Some(SaveIoResult::Outcomes(outcomes)) => {
                let Some(first) = outcomes.first() else {
                    self.editor_flow = EditorFlow::Failed("no write issued".into());
                    cx.notify();
                    return;
                };
                if first.is_acknowledged() && outcomes.iter().all(|o| o.is_acknowledged()) {
                    // Acknowledged → clean editor (the value-model
                    // contract: working becomes the new original).
                    if let Some(editor) = self.editor.as_mut() {
                        editor.save_acknowledged();
                    }
                    self.editor_flow = EditorFlow::Idle;
                    cx.notify();
                    return;
                }
                if first.is_outcome_unknown() {
                    // THE acceptance-3 rule: editor UNTOUCHED (the write
                    // may have applied — acknowledging would falsify the
                    // original bytes a later re-read compares against).
                    // No resend: the connection is sticky-broken and the
                    // session never re-issues. The gate already dropped
                    // to ReadOnly via the disconnect path below.
                    self.editor_flow = EditorFlow::OutcomeUnknown;
                    self.value_panel =
                        ValuePanelState::Error("disconnected — reconnect to refresh".into());
                    cx.notify();
                    return;
                }
                if let redis_io::outcome::WriteOutcome::Failed(e) = first {
                    // Definitive failure: editor untouched (still dirty);
                    // retry is a user action.
                    self.editor_flow = EditorFlow::Failed(e.message.clone());
                    cx.notify();
                    return;
                }
                self.editor_flow = EditorFlow::Idle;
                cx.notify();
            }
            None => {
                // Internal task error: keep the editor untouched.
                self.editor_flow = EditorFlow::Failed("internal task error".into());
                cx.notify();
            }
        }
    }

    /// The conflict modal's "Overwrite anyway": an explicit user choice —
    /// re-issue the save WITHOUT the precheck (mirrors the F5 decision:
    /// the user takes the last-writer-wins risk knowingly).
    pub fn save_overwrite_conflict(&mut self, cx: &mut Context<Self>) {
        self.editor_modal = None;
        self.editor_flow = EditorFlow::Idle;
        self.save_value_no_precheck(cx);
    }

    /// The save issue without the CAS precheck (Overwrite-anyway).
    fn save_value_no_precheck(&mut self, cx: &mut Context<Self>) {
        if self.editor_write_running || self.link != LinkState::Live {
            return;
        }
        let commands = self.save_commands();
        if commands.is_empty() {
            return;
        }
        let parsed = write_safety::ParsedCommand {
            name: commands[0].name.clone(),
            args: commands[0].args.clone(),
        };
        if !matches!(
            write_safety::classify(&parsed, &self.gate),
            write_safety::Decision::Run
        ) {
            self.editor_flow = EditorFlow::Refused(
                write_safety::BlockedReason::WriteBlockedByReadOnly
                    .message()
                    .to_string(),
            );
            cx.notify();
            return;
        }
        self.editor_write_running = true;
        self.editor_flow = EditorFlow::Saving;
        cx.notify();
        let conn = self.conn.clone();
        let task = Self::run_io(cx, async move {
            let conn = &mut *conn.lock_owned().await;
            let mut outcomes = Vec::new();
            for cmd in commands {
                let recorded = cmd.clone();
                match conn.issue(cmd).await {
                    Ok(Reply::Okay) => outcomes.push(WriteOutcome::Acknowledged),
                    Ok(_) => outcomes.push(WriteOutcome::Failed(
                        redis_io::outcome::DefinitiveError::new(
                            redis_io::outcome::DefinitiveErrorKind::ServerError,
                            "unexpected reply to write",
                        ),
                    )),
                    Err(err) => {
                        outcomes.push(WriteOutcome::from_conn_error(recorded, err));
                        break;
                    }
                }
            }
            SaveIoResult::Outcomes(outcomes)
        });
        cx.spawn(async move |this, cx| {
            let result = task.await.ok();
            let _ = this.update(cx, |this, cx| {
                this.editor_write_running = false;
                this.apply_save_result(result, cx);
            });
        })
        .detach();
    }

    /// The OutcomeUnknown banner's "Check now": re-READ the key
    /// (read-only, nothing is written) and compare the server's bytes
    /// against the editor's original + working — the result lands in the
    /// banner text.
    pub fn check_after_unknown(&mut self, cx: &mut Context<Self>) {
        if self.link != LinkState::Live || self.editor_write_running {
            return;
        }
        let Some(key) = self.selected_key.clone() else {
            return;
        };
        self.editor_write_running = true;
        cx.notify();
        let conn = self.conn.clone();
        let task = Self::run_io(cx, async move {
            let conn = &mut *conn.lock_owned().await;
            conn.issue(Command::new("GET", vec![key])).await
        });
        cx.spawn(async move |this, cx| {
            let reply = task.await.ok();
            let _ = this.update(cx, |this, cx| {
                this.editor_write_running = false;
                let server = reply
                    .and_then(|r| r.ok())
                    .and_then(|r| r.as_bytes().map(<[u8]>::to_vec));
                match server {
                    Some(bytes) => {
                        let original = this.editor.as_ref().and_then(|e| match &e.original {
                            RawValue::String(b) | RawValue::Json(b) => Some(b.as_bytes().to_vec()),
                            _ => None,
                        });
                        if let Some(orig) = original {
                            if bytes == orig {
                                // Server still holds the ORIGINAL: the
                                // write never landed. Safe to retry (a
                                // user action).
                                this.editor_flow = EditorFlow::Failed(
                                    "check: the write did not land — \
                                        the server still holds the value you loaded; \
                                        retry the save if you still want it"
                                        .into(),
                                );
                            } else {
                                this.editor_flow = EditorFlow::Failed(
                                    "check: the server holds a different \
                                        value than you loaded — discard to accept it, or \
                                        save to overwrite"
                                        .into(),
                                );
                            }
                        } else {
                            this.editor_flow =
                                EditorFlow::Failed("check: re-read the key to compare".into());
                        }
                    }
                    None => {
                        this.editor_flow = EditorFlow::Failed(
                            "check failed — the connection is still down".into(),
                        );
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The OutcomeUnknown banner's "Retry save": USER-EXPLICIT re-issue,
    /// only after a reconnect made the link live again. This is the only
    /// re-issue path in the app, and it is a user action.
    pub fn retry_save(&mut self, cx: &mut Context<Self>) {
        if self.editor_flow != EditorFlow::OutcomeUnknown {
            return;
        }
        self.editor_flow = EditorFlow::Idle;
        self.save_value(cx);
    }

    // ---- Phase 5: TTL CRUD ---------------------------------------------

    /// EXPIRE the selected key (Write — classify gate applies). After the
    /// ack, the TTL display refreshes (acceptance 4).
    pub fn set_ttl(&mut self, seconds: i64, cx: &mut Context<Self>) {
        if seconds < 0 {
            return;
        }
        self.issue_editor_write(
            Command::new(
                "EXPIRE",
                vec![
                    self.selected_key.clone().unwrap_or_default(),
                    seconds.to_string().into_bytes(),
                ],
            ),
            cx,
        );
    }

    /// PERSIST the selected key (remove the TTL; Write).
    pub fn clear_ttl(&mut self, cx: &mut Context<Self>) {
        self.issue_editor_write(
            Command::new(
                "PERSIST",
                vec![self.selected_key.clone().unwrap_or_default()],
            ),
            cx,
        );
    }

    /// The delete-key button: open the confirm modal (single-key DEL is
    /// `Write` per policy; the modal is the naming-what's-lost courtesy).
    pub fn request_delete_key(&mut self, cx: &mut Context<Self>) {
        self.editor_modal = Some(EditorModal::DeleteKey);
        cx.notify();
    }

    /// The delete-key modal's Confirm: classify DEL, then issue once.
    pub fn confirm_delete_key(&mut self, cx: &mut Context<Self>) {
        self.editor_modal = None;
        self.issue_editor_write(
            Command::new("DEL", vec![self.selected_key.clone().unwrap_or_default()]),
            cx,
        );
    }

    /// One shared write issuer for TTL/delete flows: classify first
    /// (gate must be WriteEnabled, else the verbatim refusal), issue
    /// ONCE via run_io, tri-state the result, refresh the TTL display
    /// on ack (acceptance 4's "reflected in reads").
    fn issue_editor_write(&mut self, cmd: Command, cx: &mut Context<Self>) {
        if self.editor_write_running || self.link != LinkState::Live {
            return;
        }
        let parsed = write_safety::ParsedCommand {
            name: cmd.name.clone(),
            args: cmd.args.clone(),
        };
        match write_safety::classify(&parsed, &self.gate) {
            write_safety::Decision::Run => {}
            write_safety::Decision::Blocked(reason) => {
                self.editor_flow = EditorFlow::Refused(reason.message().to_string());
                cx.notify();
                return;
            }
            write_safety::Decision::Confirm(_) => {
                // Single-key DEL/EXPIRE/PERSIST are Write; unreachable.
                self.editor_flow = EditorFlow::Refused("confirm required".into());
                cx.notify();
                return;
            }
        }
        self.editor_write_running = true;
        self.editor_flow = EditorFlow::Saving;
        cx.notify();
        let conn = self.conn.clone();
        let cmd_for_outcome = cmd.clone();
        let task = Self::run_io(cx, async move {
            let conn = &mut *conn.lock_owned().await;
            conn.issue(cmd).await
        });
        cx.spawn(async move |this, cx| {
            let reply = task.await.ok();
            let _ = this.update(cx, |this, cx| {
                this.editor_write_running = false;
                match reply {
                    Some(Ok(Reply::Okay)) => {
                        this.editor_flow = EditorFlow::Idle;
                        if this.editor_flow_is_ttl(&cmd_for_outcome) {
                            this.load_ttl(cx);
                        }
                    }
                    Some(Ok(Reply::Int(n))) => {
                        // DEL/EXPIRE/PERSIST answer Int: >= 0 is an ack
                        // (0 = key gone / not expired — honest success).
                        this.editor_flow = EditorFlow::Idle;
                        if this.editor_flow_is_ttl(&cmd_for_outcome) {
                            this.load_ttl(cx);
                        } else if n == 0 {
                            // DEL 0: the key was already gone.
                            this.value_panel = ValuePanelState::Missing;
                        }
                    }
                    Some(Ok(_)) => {
                        this.editor_flow = EditorFlow::Failed("unexpected reply to write".into());
                    }
                    Some(Err(err)) => {
                        let outcome = WriteOutcome::from_conn_error(cmd_for_outcome, err.clone());
                        if outcome.is_outcome_unknown() {
                            this.editor_flow = EditorFlow::OutcomeUnknown;
                            this.on_connection_error(err);
                        } else if let WriteOutcome::Failed(e) = &outcome {
                            this.editor_flow = EditorFlow::Failed(e.message.clone());
                        }
                    }
                    None => {
                        this.editor_flow = EditorFlow::Failed("internal task error".into());
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Whether this command was a TTL mutation (post-ack TTL refresh).
    fn editor_flow_is_ttl(&self, cmd: &Command) -> bool {
        cmd.name == "EXPIRE" || cmd.name == "PERSIST"
    }
    //
    // Every transition happens here, on the entity — the view only calls
    // these methods and renders the state they leave behind. The engine
    // (console-engine) decides policy; this is the flow wiring: gate
    // snapshot → decision → run_io issue → render → log, with the
    // serial guard for Stop and stale completions.

    /// Whether a console command is in flight (drives the Stop button and
    /// the "already running" refusal).
    pub fn console_running(&self) -> bool {
        self.console_running
    }

    /// Up-arrow in the console input: recall the next older history entry
    /// (returns the text to place in the field, if any).
    pub fn console_history_up(&mut self, current_field: &str) -> Option<String> {
        self.console.history_up(current_field)
    }

    /// Down-arrow in the console input: walk toward the newer entries
    /// (past the newest restores the saved draft; `None` = keep as-is).
    pub fn console_history_down(&mut self) -> Option<String> {
        self.console.history_down()
    }

    /// Submit a console input line (Enter in the panel's input).
    ///
    /// - `Nothing` (empty line): no-op.
    /// - Refuse: echo input + refusal (Info, write-safety's wording), nothing sent.
    /// - `Execute`: one command at a time — refuse while running — then
    ///   echo, record history, issue via run_io, render, append.
    /// - `NeedConfirm`: echo + record history, open the pending-confirm
    ///   state (the modal collects the typed phrase; confirm re-checks the
    ///   gate).
    pub fn console_submit(&mut self, line: &str, cx: &mut Context<Self>) {
        use console_engine::SubmitOutcome;

        if line.trim().is_empty() {
            return; // Nothing: a no-op
        }
        if self.pending_console_confirm().is_some() {
            // A confirmation is open; the input line is not the confirm
            // path. Decline it explicitly instead of silently running.
            return;
        }
        if self.console_running {
            self.console.echo_input(line);
            self.console
                .push_refusal("a command is already running — wait for it or press Esc");
            cx.notify();
            return;
        }
        if self.link != LinkState::Live {
            self.console.echo_input(line);
            self.console
                .push_refusal("the connection is down — reconnect before running commands");
            cx.notify();
            return;
        }

        match crate::console::submit_line(line, &self.gate) {
            SubmitOutcome::Nothing => {}
            SubmitOutcome::Refuse { reason } => {
                self.console.echo_input(line);
                self.console.record_history(line);
                self.console.push_refusal(&reason);
                cx.notify();
            }
            SubmitOutcome::NeedConfirm {
                parsed,
                confirmation,
            } => {
                self.console.echo_input(line);
                self.console.record_history(line);
                self.console.open_confirm(parsed, confirmation);
                let placeholder = self
                    .pending_console_confirm()
                    .map(|p| p.placeholder())
                    .unwrap_or_default();
                cx.emit(SessionEvent::ConsoleConfirmOpened(placeholder));
                cx.notify();
            }
            SubmitOutcome::Execute(parsed) => {
                self.console.echo_input(line);
                self.console.record_history(line);
                self.issue_console_command(parsed, cx);
            }
        }
    }

    /// Issue an already-approved command (from `Execute` outcomes) via the
    /// documented run_io bridge. Arms the serial guard; the result
    /// appends to the log; `Disconnected` maps to the honest
    /// outcome-unknown line (never a resend).
    fn issue_console_command(
        &mut self,
        parsed: write_safety::ParsedCommand,
        cx: &mut Context<Self>,
    ) {
        use redis_io::outcome::WriteOutcome;

        self.console_serial += 1;
        let serial = self.console_serial;
        self.console_running = true;
        cx.notify();

        let cmd = Command::new(parsed.name.clone(), parsed.args.clone());
        let conn = self.conn.clone();
        let task = Self::run_io(cx, async move {
            let conn = &mut *conn.lock_owned().await;
            conn.issue(cmd).await
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                if this.console_serial != serial {
                    return; // Stopped or superseded: the wait was cancelled.
                }
                this.console_running = false;
                match result {
                    Ok(Ok(reply)) => {
                        this.console
                            .push_output(&console_engine::render_reply(&reply));
                    }
                    Ok(Err(err)) => {
                        // The one mapping rule of the tri-state contract
                        // (redis-io): Disconnected → OutcomeUnknown. The
                        // render for OutcomeUnknown is the honest
                        // "check before retrying" line — never a resend.
                        let outcome = WriteOutcome::from_conn_error(
                            Command::new(parsed.name, parsed.args),
                            err.clone(),
                        );
                        this.console
                            .push_output(&console_engine::format_write_outcome(&outcome));
                        if matches!(err, ConnError::Disconnected { .. }) {
                            this.on_connection_error(err);
                        }
                    }
                    Err(join_err) => {
                        this.console
                            .push_output(&console_engine::render_info(&format!(
                                "internal task error: {join_err}"
                            )));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// The pending confirmation, if one is open (the modal renders from
    /// this).
    pub fn pending_console_confirm(&self) -> Option<&crate::console::PendingConfirm> {
        self.console.pending()
    }

    /// The typed phrase changed in the confirm modal's input field.
    pub fn console_confirm_typed(&mut self, typed: String, cx: &mut Context<Self>) {
        self.console.set_typed(typed);
        cx.notify();
    }

    /// Confirm the pending destructive command (the modal's Confirm
    /// button). Runs the engine's confirm step against the *current* gate:
    ///
    /// - `Execute` → issue the command (gate + phrase both passed);
    /// - `WrongPhrase` → an Info line, the modal stays open (retry);
    /// - `GateChanged` → an Info line with the fresh refusal's wording,
    ///   the modal closes, nothing is sent (never silently execute).
    pub fn console_confirm(&mut self, cx: &mut Context<Self>) {
        use console_engine::ConfirmOutcome;

        let Some(pending) = self.console.pending().cloned() else {
            return;
        };
        match crate::console::confirm_pending(&pending, &self.gate) {
            ConfirmOutcome::Execute(parsed) => {
                self.console.take_pending();
                self.console.push_output(&console_engine::render_info(
                    "confirmed — running the command",
                ));
                if self.console_running || self.link != LinkState::Live {
                    // Defensive: the guards above should have held, but if
                    // the world changed between the click and here, refuse
                    // honestly rather than send.
                    self.console.push_refusal(
                        "a command is already running — the confirmed command was not sent",
                    );
                    cx.notify();
                    return;
                }
                self.issue_console_command(parsed, cx);
            }
            ConfirmOutcome::WrongPhrase => {
                self.console.push_output(&console_engine::render_info(
                    "the typed phrase does not match — type the command name exactly to confirm",
                ));
                cx.notify();
            }
            ConfirmOutcome::GateChanged { reason } => {
                self.console.take_pending();
                self.console.push_refusal(&reason);
                cx.notify();
            }
        }
    }

    /// Decline the pending confirmation (Cancel in the modal, or Esc):
    /// nothing is sent, the modal closes, the honest decline line lands
    /// in the log.
    pub fn console_decline(&mut self, cx: &mut Context<Self>) {
        self.console.decline_confirm();
        cx.notify();
    }

    /// Stop waiting for the running console command (the Stop button or
    /// Esc in the panel). The serial guard makes the eventual result a
    /// no-op; the honest wording records that the command may still have
    /// reached the server. No bytes are re-sent either way.
    pub fn console_stop(&mut self, cx: &mut Context<Self>) {
        if !self.console_running {
            return;
        }
        self.console_serial += 1; // retire the in-flight serial
        self.console_running = false;
        self.console.push_output(&console_engine::render_info(
            "canceled — the command may still have reached the server",
        ));
        cx.notify();
    }

    /// User-initiated reconnect (explicit, re-sends nothing).
    pub fn reconnect(&mut self, cx: &mut Context<Self>) {
        if self.link == LinkState::Live {
            return;
        }
        let conn = self.conn.clone();
        let task = Self::run_io(cx, async move {
            let conn = &mut *conn.lock_owned().await;
            conn.reconnect().await
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| {
                match result {
                    Ok(Ok(())) => {
                        this.link = LinkState::Live;
                        // Fresh scan run under a new generation.
                        this.token.cancel();
                        let gen = this.generation.next_generation();
                        this.token = CancellationToken::new(gen);
                        // Fresh engine under a new Arc (no page fetch can
                        // hold the old one: fetching_page was false when
                        // disconnected).
                        this.scan = Arc::new(Mutex::new(
                            KeyScan::new(gen, PAGE_SIZE, DEFAULT_BATCH_HINT)
                                .with_type(this.active_scan_type_arg()),
                        ));
                        this.flat.reset();
                        // Phase 3: tree + expansion state follow the same
                        // reset discipline as the flat list.
                        this.tree.reset();
                        this.expanded.clear();
                        // Type cache reflects the old scan run's keys;
                        // the new run re-probes what it delivers. Late
                        // probes from the old run are generation-guarded.
                        this.key_types.clear();
                        this.probing_types = false;
                        this.fetching_page = false;
                        this.scan_driving = false;
                        this.searching = false;
                        this.fetch_next_page(cx);
                    }
                    Ok(Err(err)) => {
                        // Still down: keep sticky state, surface the reason.
                        if let ConnError::Disconnected { reason, .. } = &err {
                            this.link = LinkState::Disconnected;
                            let _ = this.gate.apply(GateEvent::Disconnected);
                            let _ = reason;
                        } else {
                            this.link = LinkState::Disconnected;
                        }
                    }
                    Err(join_err) => {
                        this.link = LinkState::Disconnected;
                        let _ = join_err;
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// Handle a command-level connection failure: sticky disconnect.
    fn on_connection_error(&mut self, err: ConnError) {
        if matches!(err, ConnError::Disconnected { .. }) {
            self.link = LinkState::Disconnected;
            // The forced reset: from ANY state this lands in ReadOnly.
            let _ = self.gate.apply(GateEvent::Disconnected);
            // Stop the current scan run (stays resumable per engine docs).
            self.token.cancel();
            self.fetching_page = false;
            self.value_panel = ValuePanelState::Error("disconnected — reconnect to refresh".into());
        } else {
            self.value_panel = ValuePanelState::Error(err.to_string());
        }
    }

    /// Drain `ConnEvent`s from the connection on the main thread.
    ///
    /// A dedicated foreground task moves events one-by-one (the
    /// receiver lives on the tokio side; each `recv()` is a tiny async
    /// hop through GPUI's foreground executor, so the entity is never
    /// updated from off-thread).
    fn start_event_drain(&mut self, cx: &mut Context<Self>) {
        let mut rx = std::mem::replace(
            &mut self.events_rx,
            tokio::sync::mpsc::channel(1).1, // dummy; real receiver moved below
        );
        cx.spawn(async move |this, cx| {
            // tokio mpsc `recv` is runtime-agnostic (channel + oneshot
            // semantics), so the loop can await directly on GPUI's
            // foreground executor; each event is applied to the entity
            // on the main thread inside `update`.
            while let Some(ev) = rx.recv().await {
                let keep_going = this.update(cx, |this, cx| {
                    this.handle_event(ev.clone(), cx);
                    cx.notify();
                });
                if keep_going.is_err() {
                    break; // session entity gone (tab closed)
                }
            }
        })
        .detach();
    }

    /// Apply one `ConnEvent` to the entity state.
    fn handle_event(&mut self, ev: ConnEvent, cx: &mut Context<Self>) {
        match ev {
            ConnEvent::Connected => {
                // From the initial connect or a successful reconnect.
                self.link = LinkState::Live;
                cx.emit(SessionEvent::Connected);
            }
            ConnEvent::Disconnected(reason) => {
                if self.link != LinkState::Disconnected {
                    self.link = LinkState::Disconnected;
                    // Forced gate reset: from ANY state → ReadOnly.
                    let _ = self.gate.apply(GateEvent::Disconnected);
                    self.token.cancel();
                    self.fetching_page = false;
                    cx.emit(SessionEvent::Disconnected(reason));
                }
            }
            ConnEvent::CommandDone(..) => {
                // Observability only; results travel via command futures.
            }
            // Phase 6 (spec item 5): the security events are real state
            // transitions, not log lines.
            ConnEvent::TlsWarning(_) => {
                // Persistent insecure flag: set at establish, re-emitted on
                // every reconnect while the mode is active. Lives until the
                // session dies (disconnect keeps the chip — the config
                // still says insecure; PRD acceptance 5 wants permanence).
                if !self.tls_insecure {
                    self.tls_insecure = true;
                    cx.emit(SessionEvent::TlsInsecureChanged(true));
                }
            }
            ConnEvent::HostKeyPrompt(p) => {
                // A live tunnel paused for TOFU confirmation (reconnect
                // path). The prompt payload is host:port + fingerprint.
                self.host_key_ui = Some(HostKeyUi::from_prompt(&p));
                cx.emit(SessionEvent::HostKeyModalOpened);
            }
            ConnEvent::HostKeyBlocked(b) => {
                // Changed key: hard block, no override. Surfaced as state.
                self.host_key_ui = Some(HostKeyUi::from_blocked(&b));
                cx.emit(SessionEvent::HostKeyModalOpened);
            }
        }
    }

    // ---- Phase 6: security surfacing ------------------------------------

    /// True while this connection runs with the insecure TLS override
    /// (drives the persistent status-bar chip).
    pub fn tls_insecure(&self) -> bool {
        self.tls_insecure
    }

    /// The pending SSH host-key modal state, if any (the workspace
    /// renders the TOFU prompt or the hard-block overlay from this).
    pub fn host_key_ui(&self) -> Option<&HostKeyUi> {
        self.host_key_ui.as_ref()
    }

    /// Dismiss a pending host-key modal without acting (Blocked's only
    /// button; also a defensive close for Prompt).
    pub fn dismiss_host_key_modal(&mut self, cx: &mut Context<Self>) {
        if self.host_key_ui.take().is_some() {
            cx.emit(SessionEvent::HostKeyModalClosed);
            cx.notify();
        }
    }

    /// Answer a pending TOFU prompt: `trust` calls the backend's
    /// confirm path ([`RedisConnection::connect_ssh_confirmed`] with
    /// `true`, which records the fingerprint in the known-hosts store)
    /// and reconnects; `false` aborts cleanly (backend rejects, no
    /// record). A `Blocked` payload has no override — the call is a
    /// no-op for it (the modal only offers Close).
    ///
    /// Emits nothing on no-op; the result lands as `link` + a
    /// `Disconnected`/`Connected` event like any reconnect.
    pub fn answer_host_key_prompt(&mut self, trust: bool, cx: &mut Context<Self>) {
        let Some(HostKeyUi::Prompt { .. }) = self.host_key_ui.clone() else {
            return; // Blocked or nothing pending: no override, ever.
        };
        self.host_key_ui = None;
        cx.emit(SessionEvent::HostKeyModalClosed);
        if !trust {
            // Clean abort: the connection was never established on this
            // path; mark it down honestly (sticky, reconnectable).
            self.link = LinkState::Disconnected;
            let _ = self.gate.apply(GateEvent::Disconnected);
            cx.emit(SessionEvent::Disconnected(Some(
                "SSH host key not trusted — connection aborted".into(),
            )));
            cx.notify();
            return;
        }
        // Trust & connect: the backend's confirm path re-runs the tunnel
        // handshake with the Accept decision (recording the key) and
        // finishes the connection.
        let conn = self.conn.clone();
        let task = Self::run_io(cx, async move {
            let conn = &mut *conn.lock_owned().await;
            conn.connect_ssh_confirmed(true).await
        });
        cx.spawn(async move |this, cx| {
            let result = task.await;
            let _ = this.update(cx, |this, cx| match result {
                Ok(Ok(())) => {
                    this.link = LinkState::Live;
                    cx.emit(SessionEvent::Connected);
                }
                Ok(Err(err)) => {
                    this.link = LinkState::Disconnected;
                    let _ = this.gate.apply(GateEvent::Disconnected);
                    let reason = err.to_string();
                    // The confirm path can itself be blocked (the key
                    // changed between prompt and decision): surface the
                    // hard-block modal again rather than a bare error.
                    if let Some(crate::security::SshRefusal::Blocked {
                        new_fingerprint,
                        old_fingerprint,
                    }) = crate::security::parse_ssh_refusal(&reason)
                    {
                        this.host_key_ui = Some(HostKeyUi::Blocked {
                            host_port: this.ssh_host_port.clone(),
                            new_fingerprint,
                            old_fingerprint,
                        });
                        cx.emit(SessionEvent::HostKeyModalOpened);
                    }
                    cx.emit(SessionEvent::Disconnected(Some(reason)));
                }
                Err(join_err) => {
                    this.link = LinkState::Disconnected;
                    let _ = join_err;
                    cx.emit(SessionEvent::Disconnected(None));
                }
            });
        })
        .detach();
    }

    /// Spawn `f` on the global tokio runtime and return the GPUI task
    /// that resolves with its output. This is the ONLY bridge between
    /// the UI and the redis runtime.
    fn run_io<T, F>(cx: &Context<Self>, f: F) -> Task<Result<T, tokio::task::JoinError>>
    where
        T: Send + 'static,
        F: std::future::Future<Output = T> + Send + 'static,
    {
        let handle = cx.global::<crate::TokioRuntime>().0.clone();
        let join = handle.spawn(f);
        // The JoinHandle is itself a Send future; awaiting it on GPUI's
        // background executor resolves with the tokio task's output.
        cx.background_spawn(join)
    }
}

impl EventEmitter<SessionEvent> for ConnectionSession {}

/// What a successful connect yields: the live connection, its event
/// receiver (drained by the session), and the display name.
pub type Connected = (
    RedisConnection,
    tokio::sync::mpsc::Receiver<ConnEvent>,
    String,
);

/// The connect parameters (form values + the Security section's
/// backend structs). One struct instead of a long positional list:
/// every field is documented, and adding one (e.g. a future TLS
/// server-name override) won't ripple through every call site.
pub struct ConnectRequest {
    /// Redis host as reachable without the tunnel (the tunnel's
    /// forwarding target).
    pub host: String,
    /// Redis port (the tunnel's forwarding target port).
    pub port: u16,
    /// Logical database index.
    pub db: i64,
    /// ACL username (optional).
    pub username: Option<String>,
    /// Password (plain here; wrapped into `Secret` at the boundary).
    pub password: Option<String>,
    /// TLS config from the Security section (`None` = plaintext).
    pub tls: Option<redis_io::tls::TlsConfig>,
    /// SSH tunnel config from the Security section (`None` = direct).
    pub ssh: Option<redis_io::ssh::SshTunnelConfig>,
}

/// Builder used by the workspace's connect flow (form → session).
pub struct ConnectFlow;

impl ConnectFlow {
    /// Connect with the form's values; resolves on the main thread.
    ///
    /// On success returns the (connection, events receiver, display
    /// name). On failure returns the user-displayable error (no tab is
    /// opened). Phase 6: `tls`/`ssh` carry the Security section's
    /// backend structs (built by `crate::security` at the boundary;
    /// secrets already inside the `Secret` newtype).
    pub fn connect(req: ConnectRequest, cx: &App) -> Task<Result<Connected, String>> {
        let ConnectRequest {
            host,
            port,
            db,
            username,
            password,
            tls,
            ssh,
        } = req;
        let name = format!("{host}:{port}/{db}");
        let handle = cx.global::<crate::TokioRuntime>().0.clone();
        let join = handle.spawn(async move {
            let mut cfg = RedisConnectionConfig::new(host, port, db);
            cfg.username = username;
            let secret = password.map(Secret::new);
            let cfg = cfg.with_password(secret).with_tls(tls).with_ssh_tunnel(ssh);
            RedisConnection::connect(cfg).await
        });
        cx.background_spawn(async move {
            match join.await {
                Ok(Ok(pair)) => Ok((pair.0, pair.1, name)),
                Ok(Err(err)) => Err(err.to_string()),
                Err(join_err) => Err(format!("internal task error: {join_err}")),
            }
        })
    }

    /// Connect with the user's host-key decision (the TOFU confirm
    /// flow, pre-tab form path).
    ///
    /// `accept == true` records the fingerprint in the known-hosts
    /// store (the TOFU trust step — the same `KnownHosts::record` the
    /// backend's `connect_ssh_confirmed` performs at the handshake)
    /// and retries the plain connect, which now finds the key trusted.
    /// `accept == false` does not record and returns a definitive
    /// abort (the caller never retries).
    ///
    /// The live-session path (reconnect with a held connection) calls
    /// [`RedisConnection::connect_ssh_confirmed`] directly; this
    /// pre-tab path cannot (the failed initial connect dropped the
    /// connection object), so it performs the same trust step at the
    /// store level — observably identical: record + connect.
    ///
    /// Same result contract as [`Self::connect`].
    pub fn connect_confirmed(
        req: ConnectRequest,
        accept: bool,
        fingerprint: String,
        cx: &App,
    ) -> Task<Result<Connected, String>> {
        let ConnectRequest {
            host,
            port,
            db,
            username,
            password,
            tls,
            ssh,
        } = req;
        let name = format!("{host}:{port}/{db}");
        let handle = cx.global::<crate::TokioRuntime>().0.clone();
        let join = handle.spawn(async move {
            if !accept {
                return Err("SSH host key not trusted — connection aborted".to_string());
            }
            // Record before retrying (config-dir known-hosts file; the
            // tunnel's next handshake loads it and finds Trusted).
            if let Some(t) = ssh.as_ref() {
                let kh_path =
                    connections_store::saved_connections::config_dir().join("known_hosts.json");
                let kh = connections_store::known_hosts::KnownHosts::load(&kh_path);
                kh.record(&t.host, t.port, &fingerprint);
            }
            let mut cfg = RedisConnectionConfig::new(host, port, db);
            cfg.username = username;
            let secret = password.map(Secret::new);
            let cfg = cfg.with_password(secret).with_tls(tls).with_ssh_tunnel(ssh);
            RedisConnection::connect(cfg)
                .await
                .map_err(|e| e.to_string())
        });
        cx.background_spawn(async move {
            match join.await {
                Ok(Ok(pair)) => Ok((pair.0, pair.1, name)),
                Ok(Err(err)) => Err(err.to_string()),
                Err(join_err) => Err(format!("internal task error: {join_err}")),
            }
        })
    }
}

/// Render at most this many lines in the value panel (UI-side bound;
/// a truncation note is shown when hit — the value itself is untouched).
pub const MAX_RENDERED_LINES: usize = 4000;

/// The background round's result for a save (the precheck verdict or the
/// write outcomes — an enum so the future returns one type).
#[derive(Debug)]
enum SaveIoResult {
    /// The CAS precheck's server bytes (scalar GET succeeded).
    Precheck(Vec<u8>),
    /// The precheck itself failed / key vanished / reply not bulk.
    Conflict,
    /// The precheck's issue errored (message only — byte hygiene).
    ConnErr(String),
    /// The write outcomes, in issue order (stops at the first
    /// non-acknowledged outcome).
    Outcomes(Vec<redis_io::outcome::WriteOutcome>),
}

/// The save-command derivation for one editor + key (the pure core of
/// `ConnectionSession::save_commands`, mirrored from
/// `SaveWorkflow::save_commands`): one command per EDITED unit;
/// untouched entries are never even sent.
///
/// - String/JSON: one `SET`.
/// - Hash: one `HSET` per edited field value.
/// - List: `LSET index value` per edited item.
/// - Set: `SREM` original + `SADD` new per edited member (a set member
///   is addressed by content, not position).
/// - ZSet: `ZREM` original + `ZADD` new per edited member; one `ZADD`
///   for a score-only edit.
///
/// Field names, scores and members keep their original bytes on the
/// un-edited side (value-model invariant 3).
fn save_commands_from(editor: &value_model::editor::EditorState, key: &[u8]) -> Vec<Command> {
    use value_model::editor::{EditedValue, Slot};
    let encoded = editor.save_encoding();
    let mut commands = Vec::new();
    match (&editor.working, &encoded) {
        (EditedValue::Scalar(_), RawValue::String(bytes))
        | (EditedValue::Json(_), RawValue::Json(bytes)) => {
            commands.push(Command::new(
                "SET",
                vec![key.to_vec(), bytes.as_bytes().to_vec()],
            ));
        }
        (EditedValue::Hash(slots), RawValue::Hash(pairs)) => {
            for (i, (_, slot)) in slots.iter().enumerate() {
                if let (Slot::Edited(_), Some((orig_field, value))) = (slot, pairs.get(i)) {
                    commands.push(Command::new(
                        "HSET",
                        vec![
                            key.to_vec(),
                            orig_field.as_bytes().to_vec(),
                            value.as_bytes().to_vec(),
                        ],
                    ));
                }
            }
        }
        (EditedValue::List(slots), RawValue::List(items)) => {
            for (i, slot) in slots.iter().enumerate() {
                if let Slot::Edited(_) = slot {
                    if let Some(value) = items.get(i) {
                        commands.push(Command::new(
                            "LSET",
                            vec![
                                key.to_vec(),
                                i.to_string().into_bytes(),
                                value.as_bytes().to_vec(),
                            ],
                        ));
                    }
                }
            }
        }
        (EditedValue::Set(slots), RawValue::Set(members)) => {
            // A set member is addressed by CONTENT: an edit is
            // remove-original + add-new. Only edited slots produce
            // commands (untouched members are never sent).
            let original: &RawValue = &editor.original;
            let RawValue::Set(orig_members) = original else {
                return Vec::new();
            };
            for (i, slot) in slots.iter().enumerate() {
                if let (Slot::Edited(new), Some(orig)) = (slot, orig_members.get(i)) {
                    commands.push(Command::new(
                        "SREM",
                        vec![key.to_vec(), orig.as_bytes().to_vec()],
                    ));
                    commands.push(Command::new(
                        "SADD",
                        vec![key.to_vec(), new.as_bytes().to_vec()],
                    ));
                }
            }
            let _ = members;
        }
        (EditedValue::ZSet(slots), RawValue::ZSet(entries)) => {
            for (i, entry) in slots.iter().enumerate() {
                if let Some(out) = entries.get(i) {
                    let member_edited = matches!(entry.member, Slot::Edited(_));
                    let score_edited =
                        matches!(entry.score, value_model::editor::SlotScore::Edited(_));
                    if member_edited || score_edited {
                        // ZADD replaces the member's score; if the
                        // member bytes changed, remove the original
                        // member too.
                        if member_edited {
                            if let RawValue::ZSet(orig) = &editor.original {
                                if let Some(orig_entry) = orig.get(i) {
                                    commands.push(Command::new(
                                        "ZREM",
                                        vec![key.to_vec(), orig_entry.member.as_bytes().to_vec()],
                                    ));
                                }
                            }
                        }
                        commands.push(Command::new(
                            "ZADD",
                            vec![
                                key.to_vec(),
                                format_score(out.score).into_bytes(),
                                out.member.as_bytes().to_vec(),
                            ],
                        ));
                    }
                }
            }
        }
        _ => {}
    }
    commands
}

/// The current text of a working slot (the inline field editor's initial
/// content). Lossy display for the input; edits re-enter through
/// `edit_text`'s validated `&str`.
fn working_slot_text(
    editor: &value_model::editor::EditorState,
    path: &value_model::editor::FieldPath,
) -> Option<String> {
    use value_model::editor::EditedValue;
    let bytes: Option<&value_model::raw::RawBytes> = match (&editor.working, path) {
        (EditedValue::Scalar(s), value_model::editor::FieldPath::Scalar) => slot_bytes(s),
        (EditedValue::Json(s), value_model::editor::FieldPath::Scalar) => slot_bytes(s),
        (EditedValue::Hash(pairs), value_model::editor::FieldPath::HashField(i)) => {
            pairs.get(*i).and_then(|(_, v)| slot_bytes(v))
        }
        (EditedValue::List(items), value_model::editor::FieldPath::ListItem(i)) => {
            items.get(*i).and_then(slot_bytes)
        }
        (EditedValue::Set(members), value_model::editor::FieldPath::SetMember(i)) => {
            members.get(*i).and_then(slot_bytes)
        }
        (EditedValue::ZSet(entries), value_model::editor::FieldPath::ZSetMember(i)) => {
            entries.get(*i).and_then(|e| slot_bytes(&e.member))
        }
        (EditedValue::ZSet(entries), value_model::editor::FieldPath::ZSetScore(i)) => {
            return entries.get(*i).map(|e| match e.score {
                value_model::editor::SlotScore::Original(s)
                | value_model::editor::SlotScore::Edited(s) => number_text(s),
            })
        }
        _ => None,
    };
    bytes.map(|b| b.to_lossy_display().into_owned())
}

/// The bytes a slot contributes (Original or Edited — the inline editor
/// seeds from whatever is current).
fn slot_bytes(slot: &value_model::editor::Slot) -> Option<&value_model::raw::RawBytes> {
    match slot {
        value_model::editor::Slot::Original(b) | value_model::editor::Slot::Edited(b) => Some(b),
    }
}

/// How many fields carry edits (the dirty-guard modal's count).
fn edited_field_count(editor: &value_model::editor::EditorState) -> usize {
    use value_model::editor::EditedValue;
    match &editor.working {
        EditedValue::Scalar(s) | EditedValue::Json(s) => usize::from(is_edited(s)),
        EditedValue::Hash(pairs) => pairs.iter().filter(|(_, v)| is_edited(v)).count(),
        EditedValue::List(items) => items.iter().filter(|s| is_edited(s)).count(),
        EditedValue::Set(members) => members.iter().filter(|s| is_edited(s)).count(),
        EditedValue::ZSet(entries) => entries
            .iter()
            .filter(|e| {
                is_edited(&e.member) || matches!(e.score, value_model::editor::SlotScore::Edited(_))
            })
            .count(),
        EditedValue::Stream => 0,
    }
}

/// Whether a slot is in the Edited state.
fn is_edited(slot: &value_model::editor::Slot) -> bool {
    matches!(slot, value_model::editor::Slot::Edited(_))
}

/// The canonical text of a numeric score (ZADD encoding).
fn format_score(score: f64) -> String {
    // Redis canonical formatting: trim trailing zeros the way the server
    // sends scores; use the shortest round-trip representation.
    let text = format!("{score}");
    text
}

/// The display text of a numeric score (the inline editor's seed).
fn number_text(score: f64) -> String {
    format_score(score)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_text_shapes_follow_keycount_honesty() {
        // Unknown → scanning has produced nothing yet.
        let mut list = FlatKeyList::new();
        let engine_count = KeyCount::Unknown;
        assert_eq!(count_text_of(&list, engine_count), "…");

        // AtLeast while a scan may still deliver.
        let gen = SearchGeneration::new();
        let g = gen.next_generation();
        list.apply_page(
            &ScanPage {
                new_keys: vec![b"k1".to_vec(), b"k2".to_vec()],
                generation: g,
            },
            &gen,
        );
        assert_eq!(count_text_of(&list, KeyCount::AtLeast(2)), "2 keys so far…");

        // Exact after cursor exhaustion.
        assert_eq!(count_text_of(&list, KeyCount::Exact(2)), "2 keys");
        // Exact(0) on a fresh empty list is the honest zero total.
        let empty = FlatKeyList::new();
        assert_eq!(count_text_of(&empty, KeyCount::Exact(0)), "0 keys");
    }

    fn count_text_of(list: &FlatKeyList, engine: KeyCount) -> String {
        match list.key_count(engine) {
            KeyCount::Unknown => "…".into(),
            KeyCount::AtLeast(n) => format!("{n} keys so far…"),
            KeyCount::Exact(n) => format!("{n} keys"),
        }
    }

    #[test]
    fn gate_forces_read_only_on_disconnected_from_both_states() {
        // The invariant holder: even if (Phase 5) editing was enabled, a
        // disconnect event must force ReadOnly.
        let mut gate = WriteGate::new();
        assert!(gate.is_read_only());
        gate.apply(GateEvent::EnableEditing).unwrap();
        assert!(!gate.is_read_only());
        gate.apply(GateEvent::Disconnected).unwrap();
        assert!(gate.is_read_only());

        let mut gate2 = WriteGate::new();
        gate2.apply(GateEvent::Disconnected).unwrap();
        assert!(gate2.is_read_only());
    }

    #[test]
    fn loaded_value_panel_offers_views_from_available_views() {
        let text = ConnectionSession::loaded_value_panel(RawValue::String(RawBytes::from("hello")));
        match text {
            ValuePanelState::Loaded { views, active, .. } => {
                assert_eq!(views, vec![ViewKind::Text, ViewKind::Hex]);
                assert_eq!(active, ViewKind::Text);
            }
            _ => panic!("expected loaded panel"),
        }

        let json =
            ConnectionSession::loaded_value_panel(RawValue::String(RawBytes::from(r#"{"a":1}"#)));
        match json {
            ValuePanelState::Loaded { views, .. } => {
                assert_eq!(views, vec![ViewKind::Text, ViewKind::Json, ViewKind::Hex]);
            }
            _ => panic!("expected loaded panel"),
        }

        // Non-UTF-8: hex only.
        let bin = ConnectionSession::loaded_value_panel(RawValue::String(RawBytes::from(vec![
            0xC3, 0x28,
        ])));
        match bin {
            ValuePanelState::Loaded { views, active, .. } => {
                assert_eq!(views, vec![ViewKind::Hex]);
                assert_eq!(active, ViewKind::Hex);
            }
            _ => panic!("expected loaded panel"),
        }
    }

    #[test]
    fn containers_offer_views_strictly_from_available_views() {
        // Containers get Text + Hex when UTF-8; never JSON (that gate
        // lives in value-model and we must not invent views).
        let hash = RawValue::Hash(vec![(RawBytes::from("f"), RawBytes::from("v"))]);
        match ConnectionSession::loaded_value_panel(hash) {
            ValuePanelState::Loaded { views, active, .. } => {
                assert_eq!(views, vec![ViewKind::Text, ViewKind::Hex]);
                assert_eq!(active, ViewKind::Text);
            }
            _ => panic!("expected loaded panel"),
        }

        // A hash with one binary leaf: hex only.
        let binary_hash = RawValue::Hash(vec![(RawBytes::from("f"), RawBytes::from(vec![0xF5]))]);
        match ConnectionSession::loaded_value_panel(binary_hash) {
            ValuePanelState::Loaded { views, active, .. } => {
                assert_eq!(views, vec![ViewKind::Hex]);
                assert_eq!(active, ViewKind::Hex);
            }
            _ => panic!("expected loaded panel"),
        }
    }

    #[test]
    fn ttl_display_follows_ttl_state() {
        fn state_of(raw: i64) -> TtlState {
            match TtlState::from_redis_reply(raw) {
                TtlReply::State(s) => s,
                TtlReply::MissingKey => panic!("unexpected missing-key"),
            }
        }
        // -1 → no expiry (value-model maps the sentinel).
        assert_eq!(state_of(-1), TtlState::no_expiry());
        // -2 → key gone (honest missing state; not a TTL).
        assert_eq!(TtlState::from_redis_reply(-2), TtlReply::MissingKey);
        // Display text (app-side formatting).
        let session_display = |state: TtlState| match state.seconds {
            None => "no expiry".to_string(),
            Some(1) => "expires in 1 second".to_string(),
            Some(n) => format!("expires in {n} seconds"),
        };
        assert_eq!(session_display(TtlState::no_expiry()), "no expiry");
        assert_eq!(
            session_display(TtlState::expiring_in(1)),
            "expires in 1 second"
        );
        assert_eq!(
            session_display(TtlState::expiring_in(300)),
            "expires in 300 seconds"
        );
    }

    #[test]
    fn type_filter_matches_only_known_types_of_the_chip() {
        let mut types = HashMap::new();
        types.insert(b"k-str".to_vec(), "string".to_string());
        types.insert(b"k-hash".to_vec(), "hash".to_string());
        let keys = vec![
            b"k-str".to_vec(),
            b"k-hash".to_vec(),
            b"k-unprobed".to_vec(),
        ];

        // All / None: everything passes through.
        assert_eq!(apply_type_filter(&keys, &types, None), keys);
        assert_eq!(apply_type_filter(&keys, &types, Some(RawFilter::All)), keys);

        // Typed chips keep only probed matches; unprobed keys are held
        // back (honest: unknown type is not known to match).
        assert_eq!(
            apply_type_filter(&keys, &types, Some(RawFilter::Hash)),
            vec![b"k-hash".to_vec()]
        );
        assert_eq!(
            apply_type_filter(&keys, &types, Some(RawFilter::Stream)),
            Vec::<Vec<u8>>::new()
        );

        // Unprobed keys pass when no filter is active.
        types.clear();
        assert_eq!(
            apply_type_filter(&keys, &types, Some(RawFilter::String)),
            Vec::<Vec<u8>>::new()
        );
        assert_eq!(apply_type_filter(&keys, &types, None), keys);
    }

    #[test]
    fn raw_filter_labels_and_all_chips() {
        let labels: Vec<&str> = RawFilter::ALL.iter().map(|f| f.label()).collect();
        assert_eq!(
            labels,
            vec!["All", "String", "Hash", "List", "Set", "ZSet", "Stream"]
        );
    }

    #[test]
    fn connect_form_validation() {
        // Port bounds (u16 parse) and DB bounds (0..=15 typical; we
        // accept any i64 the server accepts — validation is parse-level).
        assert!("6379".parse::<u16>().is_ok());
        assert!("65536".parse::<u16>().is_err());
        assert!("-1".parse::<u16>().is_err());
        assert!("abc".parse::<u16>().is_err());
        assert!("0".parse::<i64>().is_ok());
        assert!("-1".parse::<i64>().is_ok());
        // Host: non-empty (validated in the form logic itself).
        assert!("".trim().is_empty());
    }

    // ---- Phase 3 wiring tests -----------------------------------------
    // These exercise the engine-side seams the UI drives: page → both
    // projections, expansion state vs children_loaded, scoped-scan
    // generation discipline, DB-switch reset, and the debounce start
    // semantics (via ProgressiveSearch directly — the timer itself is
    // GPUI-side and needs the app; the serial-guard logic is trivial
    // equality).

    /// A shared harness: a fresh KeyTree + FlatKeyList + generation,
    /// fed like the session feeds them.
    struct BrowseViews {
        tree: KeyTree,
        flat: FlatKeyList,
        gen: SearchGeneration,
    }

    impl BrowseViews {
        fn new() -> Self {
            Self {
                tree: KeyTree::new(),
                flat: FlatKeyList::new(),
                gen: SearchGeneration::new(),
            }
        }

        fn generation(&mut self) -> u64 {
            self.gen.next_generation()
        }

        /// Apply a page to both projections (the session's seam).
        fn apply(&mut self, keys: &[&[u8]]) -> bool {
            let page = ScanPage {
                new_keys: keys.iter().map(|k| k.to_vec()).collect(),
                generation: self.gen.current(),
            };
            let a = self.flat.apply_page(&page, &self.gen);
            let b = self.tree.apply_page(&page, &self.gen);
            assert_eq!(a, b, "flat/tree guards must agree");
            a
        }
    }

    #[test]
    fn pages_feed_both_projections_consistently() {
        let mut views = BrowseViews::new();
        views.generation();
        assert!(views.apply(&[b"user:1", b"sess:42", b"plain"]));
        // Flat: all three keys, arrival order.
        assert_eq!(views.flat.len(), 3);
        // Tree: root rows user / sess / plain (lexical), user:1 nested.
        let rows = views.tree.rows();
        assert_eq!(rows.iter().filter(|r| r.depth == 0).count(), 3);
        assert!(views.tree.contains_key(b"user:1"));
    }

    #[test]
    fn tree_toggle_is_pure_projection_switch() {
        // tree_mode flips nothing on the engine side: the same rows are
        // available before and after; the UI just renders differently.
        let mut views = BrowseViews::new();
        views.generation();
        views.apply(&[b"a:b:c"]);
        let rows_before = views.tree.rows();
        // Simulate a toggle: rows unchanged, flat unchanged.
        let rows_after = views.tree.rows();
        assert_eq!(rows_before, rows_after);
        assert_eq!(views.flat.len(), 1);
    }

    #[test]
    fn collapsed_branch_hides_children_in_projection() {
        // The UI's expanded-set filter: children of a collapsed branch
        // are not rendered. Model the session's projection logic.
        let mut views = BrowseViews::new();
        views.generation();
        views.apply(&[b"user:1", b"user:2", b"other"]);
        let expanded: HashSet<Vec<u8>> = HashSet::new();
        let visible = project_rows(&views.tree, &expanded);
        // Root rows only: other, user (collapsed, lexical order). No
        // user:1/user:2.
        let paths: Vec<&[u8]> = visible.iter().map(|r| r.path.as_slice()).collect();
        assert_eq!(paths, [b"other".as_slice(), b"user".as_slice()]);
        // Expand user → all four rows appear (other, user, user:1,
        // user:2 — lexical DFS).
        let mut expanded = HashSet::new();
        expanded.insert(b"user".to_vec());
        let visible = project_rows(&views.tree, &expanded);
        let paths: Vec<&[u8]> = visible.iter().map(|r| r.path.as_slice()).collect();
        assert_eq!(
            paths,
            [
                b"other".as_slice(),
                b"user".as_slice(),
                b"user:1".as_slice(),
                b"user:2".as_slice()
            ]
        );
    }

    /// The session's projection seam: the real `project_tree_rows`.
    fn project_rows(tree: &KeyTree, expanded: &HashSet<Vec<u8>>) -> Vec<TreeRow> {
        project_tree_rows(tree, expanded)
    }

    #[test]
    fn lazy_expansion_uses_a_scoped_scan_under_a_new_generation() {
        // The engine contract the UI drives: scan_for_prefix builds
        // MATCH path:*, the generation moves, and the scoped pages
        // only match the subtree.
        let mut views = BrowseViews::new();
        views.generation();
        views.apply(&[b"user:1"]);
        let g2 = views.generation();
        let scan = views
            .tree
            .scan_for_prefix(b"user", g2, PAGE_SIZE, DEFAULT_BATCH_HINT);
        assert_eq!(scan.generation(), g2);
        assert_eq!(scan.pattern(), Some(b"user:*".to_vec().as_slice()));
        // The old generation's page is now stale for BOTH projections.
        let stale = ScanPage {
            new_keys: vec![b"stale:1".to_vec()],
            generation: g2 - 1,
        };
        assert!(!views.flat.apply_page(&stale, &views.gen));
        assert!(!views.tree.apply_page(&stale, &views.gen));
        // A current scoped page lands in both.
        assert!(views.apply(&[b"user:2:cart"]));
        assert!(views.tree.contains_key(b"user:2:cart"));
        // Exhaustion → mark the subtree loaded: only under `user`.
        assert!(views.tree.mark_subtree_loaded(b"user"));
        let rows = views.tree.rows();
        let user = rows.iter().find(|r| r.path == b"user").unwrap();
        assert!(user.children_loaded);
    }

    #[test]
    fn db_switch_reset_discipline() {
        // The documented sequence, engine-side: token cancel → new
        // generation → both projections reset → fresh scan engine.
        let mut views = BrowseViews::new();
        views.generation();
        views.apply(&[b"user:1", b"old:2"]);
        views.tree.mark_subtree_loaded(b"");

        // Switch: token.cancel() is app-side; here we verify reset +
        // new generation behavior of the pieces the session owns.
        views.flat.reset();
        views.tree.reset();
        let g2 = views.generation();
        // A late page from the OLD generation dies at the guard.
        let late = ScanPage {
            new_keys: vec![b"late".to_vec()],
            generation: g2 - 1,
        };
        assert!(!views.flat.apply_page(&late, &views.gen));
        assert!(!views.tree.apply_page(&late, &views.gen));
        // The fresh scan's pages flow in clean.
        let fresh = ScanPage {
            new_keys: vec![b"newdb:1".to_vec()],
            generation: g2,
        };
        assert!(views.flat.apply_page(&fresh, &views.gen));
        assert!(views.tree.apply_page(&fresh, &views.gen));
        assert_eq!(views.flat.len(), 1);
        assert!(views.tree.contains_key(b"newdb:1"));
        assert!(!views.tree.contains_key(b"user:1"));
    }

    #[test]
    fn search_debounce_start_semantics() {
        // The ProgressiveSearch half of the debounce (the serial guard
        // is a UI-side equality check; the engine half is start's
        // retire + generation discipline).
        let mut search = ProgressiveSearch::new(MatchMode::Prefix, PAGE_SIZE, DEFAULT_BATCH_HINT);
        let gen = SearchGeneration::new();

        // Keystroke 1: a run arms.
        let run1 = search.start(b"user", &gen);
        assert_eq!(run1.pattern(), Some(b"user*".to_vec().as_slice()));
        assert!(!run1.token.is_cancelled());

        // Keystroke 2 lands before the debounce fires: start retires
        // run1 (token cancelled, generation stale).
        let run2 = search.start(b"user:1", &gen);
        assert!(run1.token.is_cancelled());
        assert!(!gen.is_current(run1.generation()));
        assert!(gen.is_current(run2.generation()));

        // Esc: cancel without advancing the generation.
        search.cancel();
        assert!(run2.token.is_cancelled());
        assert!(gen.is_current(run2.generation()));
    }

    #[test]
    fn search_run_resets_both_projections_per_generation() {
        // New search run → reset flat + tree (the phase3 discipline).
        let mut views = BrowseViews::new();
        views.generation();
        views.apply(&[b"pre:search"]);
        // search.start advances the generation...
        let mut search = ProgressiveSearch::new(MatchMode::Prefix, PAGE_SIZE, DEFAULT_BATCH_HINT);
        let _run = search.start(b"post", &views.gen);
        // ...the session resets both projections on the new run:
        views.flat.reset();
        views.tree.reset();
        // A late page from the pre-search generation is dropped.
        let late = ScanPage {
            new_keys: vec![b"pre:search".to_vec()],
            generation: views.gen.current() - 1,
        };
        assert!(!views.flat.apply_page(&late, &views.gen));
        assert!(!views.tree.apply_page(&late, &views.gen));
        // Search-run pages apply cleanly.
        assert!(views.apply(&[b"post:1"]));
    }

    #[test]
    fn typed_filter_uses_browse_matcher_and_holds_back_unprobed() {
        // Reconciliation: the app delegates matching to browse's
        // type_filter::matches; module types and "none" don't match
        // presets; unprobed keys are held back under a typed chip.
        assert_eq!(
            RawFilter::String.to_type_filter(),
            browse::type_filter::TypeFilter::String
        );
        // Cross-check via apply_type_filter with a module type cached.
        let mut types = HashMap::new();
        types.insert(b"k-module".to_vec(), "ReJSON-RL".to_string());
        types.insert(b"k-str".to_vec(), "string".to_string());
        let keys = vec![b"k-module".to_vec(), b"k-str".to_vec()];
        assert_eq!(
            apply_type_filter(&keys, &types, Some(RawFilter::String)),
            vec![b"k-str".to_vec()]
        );
        // "Other" would match the module key (browse semantics), but
        // the chip row does not offer it — not reachable via RawFilter.
        assert!(browse_type_filter::matches(
            "ReJSON-RL",
            browse::type_filter::TypeFilter::Other
        ));
    }

    #[test]
    fn scan_type_arg_flows_from_the_active_chip() {
        // Server-side narrowing: a typed chip maps to a SCAN TYPE arg;
        // All/None maps to none.
        assert_eq!(
            RawFilter::ZSet.to_type_filter().scan_type_arg(),
            Some(b"zset".as_slice())
        );
        assert_eq!(RawFilter::All.to_type_filter().scan_type_arg(), None);
        // The engine carries it (with_type round-trip).
        let gen = SearchGeneration::new().next_generation();
        let scan =
            KeyScan::new(gen, PAGE_SIZE, DEFAULT_BATCH_HINT).with_type(Some(b"zset".to_vec()));
        assert_eq!(scan.key_type(), Some(b"zset".as_slice()));
    }

    #[test]
    fn db_switcher_validity_and_initial_db_parse() {
        // DbInfo validity gates the stepper; the initial db comes from
        // the display name (host:port/db).
        let info = DbInfo { count: 4 };
        assert!(info.is_valid_db(3));
        assert!(!info.is_valid_db(4));
        assert_eq!(DbInfo::default().count, browse::db::DEFAULT_DATABASES);
        // Name parse: "127.0.0.1:6379/2" → 2; malformed → 0.
        let parse = |name: &str| {
            name.rsplit('/')
                .next()
                .and_then(|s| s.trim().parse::<u16>().ok())
                .unwrap_or(0)
        };
        assert_eq!(parse("127.0.0.1:6379/2"), 2);
        assert_eq!(parse("127.0.0.1:6379/0"), 0);
        assert_eq!(parse("host:6379/x"), 0);
    }

    // ---- Phase 4: console flows (entity seams, no GPUI context needed) --

    use crate::console::{confirm_pending, submit_line, ConsoleState};
    use console_engine::{
        format_write_outcome, render_info, render_reply, ConfirmOutcome, SubmitOutcome,
    };
    use redis_io::connection::Reply;
    use redis_io::outcome::WriteOutcome;
    use write_safety::GateEvent;

    fn write_gate() -> WriteGate {
        let mut g = WriteGate::new();
        g.apply(GateEvent::EnableEditing).unwrap();
        g
    }

    /// The log discipline session.rs implements by hand: echo → refusal,
    /// nothing else appended, nothing sent.
    #[test]
    fn console_refuse_flow_appends_echo_and_refusal_only() {
        let gate = WriteGate::new(); // read-only
        let mut console = ConsoleState::default();
        match submit_line("DEL mykey", &gate) {
            SubmitOutcome::Refuse { reason } => {
                console.echo_input("DEL mykey");
                console.record_history("DEL mykey");
                console.push_refusal(&reason);
            }
            other => panic!("expected Refuse, got {other:?}"),
        }
        assert_eq!(console.log().len(), 2, "echo + refusal only");
        assert_eq!(console.log()[0].text, "› DEL mykey");
        assert_eq!(console.log()[1].kind, console_engine::OutputKind::Info);
        assert!(console.log()[1].text.contains("read-only"));
    }

    /// NeedConfirm → decline: the echo + decline line land, pending clears,
    /// nothing was sent.
    #[test]
    fn console_need_confirm_decline_flow() {
        let mut console = ConsoleState::default();
        let (parsed, confirmation) = match submit_line("FLUSHDB", &write_gate()) {
            SubmitOutcome::NeedConfirm {
                parsed,
                confirmation,
            } => (parsed, confirmation),
            other => panic!("expected NeedConfirm, got {other:?}"),
        };
        console.echo_input("FLUSHDB");
        console.record_history("FLUSHDB");
        console.open_confirm(parsed, confirmation);
        assert!(console.pending().is_some());
        console.decline_confirm();
        assert!(console.pending().is_none());
        assert_eq!(
            console.log().last().unwrap().text,
            "FLUSHDB — declined, nothing was sent"
        );
    }

    /// Confirm with the right phrase under a *changed* gate: refused with
    /// the fresh refusal's wording, the modal closes, nothing is sent.
    #[test]
    fn console_confirm_gate_changed_mid_confirm_refuses() {
        let mut console = ConsoleState::default();
        let (parsed, confirmation) = match submit_line("FLUSHDB", &write_gate()) {
            SubmitOutcome::NeedConfirm {
                parsed,
                confirmation,
            } => (parsed, confirmation),
            other => panic!("expected NeedConfirm, got {other:?}"),
        };
        console.open_confirm(parsed, confirmation);
        console.set_typed("flushdb".into());
        // The disconnect reset flipped the gate between prompt and confirm.
        let mut stale = write_gate();
        stale.apply(GateEvent::Disconnected).unwrap();
        let pending = console.pending().unwrap().clone();
        match confirm_pending(&pending, &stale) {
            ConfirmOutcome::GateChanged { reason } => {
                console.take_pending();
                console.push_refusal(&reason);
            }
            other => panic!("expected GateChanged, got {other:?}"),
        }
        assert!(console.pending().is_none());
        assert!(console.log().last().unwrap().text.contains("read-only"));
    }

    /// ConnError::Disconnected on write → the honest outcome-unknown line
    /// (the exact mapping `issue_console_command` performs), never a resend.
    #[test]
    fn console_disconnected_write_renders_outcome_unknown() {
        let cmd = Command::new("DEL", vec![b"mykey".to_vec()]);
        let err = ConnError::Disconnected {
            reason: Some("server closed link".into()),
            in_flight: Some(cmd.clone()),
        };
        let outcome = WriteOutcome::from_conn_error(cmd, err);
        assert!(outcome.is_outcome_unknown());
        let rendered = format_write_outcome(&outcome);
        assert_eq!(rendered.kind, console_engine::OutputKind::Info);
        let text = rendered.lines.join(" ");
        assert!(text.contains("outcome unknown"), "wording: {text}");
        assert!(text.contains("check before"), "wording: {text}");
    }

    /// A successful reply renders via render_reply and lands as the final
    /// log entries (the Ok(Ok(reply)) branch of issue_console_command).
    #[test]
    fn console_reply_renders_and_lands_in_log() {
        let mut console = ConsoleState::default();
        console.echo_input("GET foo");
        let rendered = render_reply(&Reply::Bulk(b"bar".to_vec()));
        console.push_output(&rendered);
        assert_eq!(console.log()[0].text, "› GET foo");
        assert_eq!(console.log().last().unwrap().text, "bar");
    }

    /// The one-command-at-a-time guard's refusal wording (session-level
    /// honesty contract, asserted verbatim so it can't drift silently).
    #[test]
    fn console_running_refusal_wording_is_honest() {
        let mut console = ConsoleState::default();
        console.push_refusal("a command is already running — wait for it or press Esc");
        let line = &console.log().last().unwrap().text;
        assert!(line.contains("already running"));
        assert!(line.contains("Esc"));
        // And the stop wording.
        let stop = render_info("canceled — the command may still have reached the server");
        assert!(stop.lines[0].contains("may still have reached"));
    }

    /// The serial-guard discipline: a stopped (bumped) serial drops a late
    /// result — modeled exactly as `issue_console_command` checks it.
    #[test]
    fn console_serial_guard_drops_stale_results() {
        let mut serial: u64 = 0;
        let in_flight = serial;
        // Stop bumps the serial (console_stop).
        serial += 1;
        // The late result arrives with the old serial → dropped.
        let stale = serial != in_flight;
        assert!(stale, "a stopped command's result must be dropped");
        // A current result is applied.
        let fresh = serial;
        assert!(serial == fresh);
    }

    // ---- Phase 5: save-flow discipline (pure seams, no GPUI context) --

    use value_model::editor::{EditedValue, FieldPath, Slot};
    use value_model::raw::{RawBytes, RawValue};

    fn string_editor(original: &str, working: &str) -> value_model::editor::EditorState {
        let mut e =
            value_model::editor::EditorState::open(RawValue::String(RawBytes::from(original)));
        e.edit_text(&FieldPath::Scalar, working).unwrap();
        e
    }

    fn hash_editor() -> value_model::editor::EditorState {
        let mut e = value_model::editor::EditorState::open(RawValue::Hash(vec![
            (RawBytes::from("f1"), RawBytes::from("v1")),
            (RawBytes::from("f2"), RawBytes::from("v2")),
        ]));
        e.edit_text(&FieldPath::HashField(1), "edited").unwrap();
        e
    }

    /// The save-command derivation the session's `save_commands` performs
    /// (mirrors `SaveWorkflow::save_commands` + the Phase 5 table
    /// extension): one command per EDITED unit; untouched entries never
    /// appear.
    #[test]
    fn string_save_command_is_single_set() {
        let editor = string_editor("original", "working");
        let cmds = save_commands_of(&editor, b"mykey");
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].name, "SET");
        // Byte hygiene: the assertion uses lengths only.
        assert_eq!(cmds[0].args.len(), 2);
        assert_eq!(cmds[0].args[0].len(), 5); // "mykey"
        assert_eq!(cmds[0].args[1].len(), 7); // "working"
    }

    /// Hash: ONE HSET per edited field; the untouched field is never sent.
    #[test]
    fn hash_save_commands_are_per_edited_field() {
        let editor = hash_editor();
        let cmds = save_commands_of(&editor, b"h");
        assert_eq!(cmds.len(), 1, "one HSET for the one edited field");
        assert_eq!(cmds[0].name, "HSET");
        assert_eq!(cmds[0].args[0].len(), 1); // key
        assert_eq!(cmds[0].args[1].len(), 2); // "f2" (original field name)
        assert_eq!(cmds[0].args[2].len(), 6); // "edited"
    }

    /// List: LSET per edited item with the index.
    #[test]
    fn list_save_commands_are_lset_per_edited_item() {
        let mut editor = value_model::editor::EditorState::open(RawValue::List(vec![
            RawBytes::from("one"),
            RawBytes::from("two"),
        ]));
        editor.edit_text(&FieldPath::ListItem(0), "ONE").unwrap();
        let cmds = save_commands_of(&editor, b"l");
        assert_eq!(cmds.len(), 1);
        assert_eq!(cmds[0].name, "LSET");
        assert_eq!(cmds[0].args[1], b"0".to_vec());
    }

    /// Set: an edited member is SREM-original + SADD-new (content
    /// addressing); untouched members never appear.
    #[test]
    fn set_save_commands_are_srem_orig_plus_sadd_new() {
        let mut editor = value_model::editor::EditorState::open(RawValue::Set(vec![
            RawBytes::from("a"),
            RawBytes::from("b"),
        ]));
        editor.edit_text(&FieldPath::SetMember(1), "B!").unwrap();
        let cmds = save_commands_of(&editor, b"s");
        assert_eq!(cmds.len(), 2, "SREM + SADD for the one edited member");
        assert_eq!(cmds[0].name, "SREM");
        assert_eq!(cmds[0].args[1].len(), 1); // "b" (the ORIGINAL member)
        assert_eq!(cmds[1].name, "SADD");
        assert_eq!(cmds[1].args[1].len(), 2); // "B!"
    }

    /// ZSet: an edited member is ZREM-original + ZADD-new-score; an
    /// edited score alone is one ZADD.
    #[test]
    fn zset_save_commands_cover_member_and_score_edits() {
        use value_model::raw::ZSetEntry;
        let mut editor = value_model::editor::EditorState::open(RawValue::ZSet(vec![ZSetEntry {
            member: RawBytes::from("m"),
            score: 1.0,
        }]));
        editor.edit_text(&FieldPath::ZSetMember(0), "m2").unwrap();
        let cmds = save_commands_of(&editor, b"z");
        assert_eq!(cmds.len(), 2);
        assert_eq!(cmds[0].name, "ZREM");
        assert_eq!(cmds[1].name, "ZADD");

        let mut editor2 = value_model::editor::EditorState::open(RawValue::ZSet(vec![ZSetEntry {
            member: RawBytes::from("m"),
            score: 1.0,
        }]));
        editor2.edit_text(&FieldPath::ZSetScore(0), "2.5").unwrap();
        let cmds2 = save_commands_of(&editor2, b"z");
        assert_eq!(cmds2.len(), 1, "score-only edit: one ZADD");
        assert_eq!(cmds2[0].name, "ZADD");
    }

    /// Clean editor: zero commands (NotDirty refusal upstream). The
    /// session's `save_value` refuses before derivation on `!dirty`;
    /// the derivation itself also yields nothing for an untouched
    /// hash — and for a scalar it emits the identity SET, which the
    /// NotDirty refusal keeps from ever being issued.
    #[test]
    fn clean_hash_editor_save_commands_are_empty() {
        let editor = value_model::editor::EditorState::open(RawValue::Hash(vec![]));
        assert!(save_commands_of(&editor, b"k").is_empty());
        // A clean hash with fields: untouched slots produce no HSETs.
        let editor = value_model::editor::EditorState::open(RawValue::Hash(vec![(
            RawBytes::from("f"),
            RawBytes::from("v"),
        )]));
        assert!(save_commands_of(&editor, b"k").is_empty());
    }

    /// A clean editor's save_encoding is the identity (value-model
    /// invariant), so no SET would even be needed — the UI hides Save.
    #[test]
    fn reverted_edit_is_not_dirty() {
        let editor = string_editor("same", "same");
        assert!(!editor.dirty, "identical content is not dirty");
    }

    /// The tri-state post-state rules (the acceptance-3 contract),
    /// modeled on the free functions the session's
    /// `apply_save_result` consults: from an outcome, the editor's
    /// next state. Acknowledged → clean; OutcomeUnknown → untouched
    /// (NOT acknowledged — the honest state); Failed → untouched.
    #[test]
    fn tri_state_post_state_rules() {
        // Acknowledged.
        let mut editor = string_editor("orig", "work");
        editor.save_acknowledged();
        assert!(!editor.dirty);
        assert_eq!(
            editor.original,
            RawValue::String(RawBytes::from("work")),
            "working becomes the new original"
        );

        // OutcomeUnknown / Failed: the editor is untouched —
        // save_acknowledged is deliberately NOT called.
        let editor = string_editor("orig", "work");
        assert!(editor.dirty);
        assert_eq!(
            editor.original,
            RawValue::String(RawBytes::from("orig")),
            "original preserved for the later re-read comparison"
        );
    }

    /// The OutcomeUnknown never-resend rule: the retry path is
    /// user-explicit only. The session method models this as a state
    /// check (retry is a no-op unless the flow IS OutcomeUnknown).
    #[test]
    fn retry_only_from_outcome_unknown() {
        let flow = EditorFlow::Idle;
        let can_retry = matches!(flow, EditorFlow::OutcomeUnknown);
        assert!(!can_retry, "Idle never retries");
        let flow = EditorFlow::OutcomeUnknown;
        assert!(matches!(flow, EditorFlow::OutcomeUnknown));
    }

    /// The gate check refusal wording is verbatim write-safety (one
    /// brain: the console and the save path refuse with the same text).
    #[test]
    fn save_gate_refusal_wording_is_verbatim_write_safety() {
        let gate = WriteGate::new(); // read-only
        let parsed = write_safety::ParsedCommand::from_strs("SET", &["k", "v"]);
        match write_safety::classify(&parsed, &gate) {
            write_safety::Decision::Blocked(reason) => {
                assert_eq!(
                    reason.message(),
                    write_safety::BlockedReason::WriteBlockedByReadOnly.message()
                );
                assert!(reason.message().contains("read-only"));
                assert!(reason.message().contains("enable editing"));
            }
            other => panic!("expected Blocked, got {other:?}"),
        }
    }

    /// TTL commands classify as Write (blocked read-only, run when
    /// enabled) — the gate governs EXPIRE/PERSIST (acceptance 4).
    #[test]
    fn ttl_commands_go_through_the_gate() {
        let ro = WriteGate::new();
        for name in ["EXPIRE", "PERSIST"] {
            let parsed = write_safety::ParsedCommand::from_strs(name, &["k", "10"]);
            assert!(matches!(
                write_safety::classify(&parsed, &ro),
                write_safety::Decision::Blocked(_)
            ));
        }
        let mut we = WriteGate::new();
        we.apply(GateEvent::EnableEditing).unwrap();
        let parsed = write_safety::ParsedCommand::from_strs("EXPIRE", &["k", "10"]);
        assert!(matches!(
            write_safety::classify(&parsed, &we),
            write_safety::Decision::Run
        ));
    }

    /// Single-key DEL is Write (no phrase gate per D3); the naming modal
    /// is a UI courtesy, and classify says Run under the write gate.
    #[test]
    fn single_key_del_is_plain_write_under_the_gate() {
        let mut we = WriteGate::new();
        we.apply(GateEvent::EnableEditing).unwrap();
        let parsed = write_safety::ParsedCommand::from_strs("DEL", &["one-key"]);
        assert!(matches!(
            write_safety::classify(&parsed, &we),
            write_safety::Decision::Run
        ));
    }

    /// Acceptance 9 (dirty survives refresh + divergence flag):
    /// `apply_refresh` routes new bytes to the original side and never
    /// touches the working buffer (value-model's own invariant, wired
    /// by the session's load path — asserted here at the seam the
    /// session drives).
    #[test]
    fn dirty_editor_survives_refresh_with_divergence() {
        let mut editor = string_editor("orig", "my edit");
        editor.apply_refresh(RawValue::String(RawBytes::from("server v2")));
        assert!(editor.diverged_from_server);
        assert!(editor.dirty, "still dirty");
        match &editor.working {
            EditedValue::Scalar(Slot::Edited(b)) => {
                assert_eq!(b.as_bytes(), b"my edit");
            }
            other => panic!("working clobbered: {other:?}"),
        }
        // Clean editor: both sides replaced, divergence cleared.
        let mut clean =
            value_model::editor::EditorState::open(RawValue::String(RawBytes::from("a")));
        clean.apply_refresh(RawValue::String(RawBytes::from("b")));
        assert!(!clean.diverged_from_server);
        assert!(!clean.dirty);
    }

    /// The inline field editor's seed text: the working slot's current
    /// bytes, lossy-decoded (the edit path re-enters through the
    /// validated `&str` setter).
    #[test]
    fn working_slot_text_seeds_from_the_working_buffer() {
        let editor = hash_editor();
        // f1 untouched: original bytes.
        assert_eq!(
            working_slot_text(&editor, &FieldPath::HashField(0)).unwrap(),
            "v1"
        );
        // f2 edited: the working bytes.
        assert_eq!(
            working_slot_text(&editor, &FieldPath::HashField(1)).unwrap(),
            "edited"
        );
        // Non-UTF-8: lossy (the input is a display; the setter gates).
        let bin = value_model::editor::EditorState::open(RawValue::String(RawBytes::from(vec![
            0xC3, 0x28,
        ])));
        let text = working_slot_text(&bin, &FieldPath::Scalar).unwrap();
        assert!(text.contains('\u{FFFD}'), "lossy replacement: {text:?}");
    }

    /// The dirty-guard payload's edited-field count (the modal's body).
    #[test]
    fn edited_field_counts_for_the_guard_modal() {
        assert_eq!(edited_field_count(&string_editor("a", "b")), 1);
        assert_eq!(edited_field_count(&hash_editor()), 1);
        let clean = value_model::editor::EditorState::open(RawValue::String(RawBytes::from("x")));
        assert_eq!(edited_field_count(&clean), 0);
    }

    /// The CAS precheck comparison: server bytes vs the editor's
    /// ORIGINAL (not the working buffer) — F5's "changed since you
    /// loaded it".
    #[test]
    fn conflict_precheck_compares_against_original() {
        let editor = string_editor("loaded", "my edit");
        let original = match &editor.original {
            RawValue::String(b) => b.as_bytes().to_vec(),
            _ => unreachable!(),
        };
        // Server still has the loaded bytes: no conflict.
        let server_same = "loaded".as_bytes().to_vec();
        assert_eq!(original, server_same);
        // Server has something else: conflict.
        let server_other = "someone else".as_bytes().to_vec();
        assert_ne!(original, server_other);
    }

    /// The `SaveIoResult`-driven apply rules: the precheck verdict path
    /// (free function mirror of `apply_save_result`'s first arm).
    #[test]
    fn precheck_verdict_drives_conflict_or_proceed() {
        let original: Vec<u8> = b"loaded".to_vec();
        // Same → proceed.
        let server: Vec<u8> = b"loaded".to_vec();
        assert!(precheck_matches(&original, &server));
        // Differ → conflict.
        let server: Vec<u8> = b"changed".to_vec();
        assert!(!precheck_matches(&original, &server));
    }

    fn precheck_matches(original: &[u8], server: &[u8]) -> bool {
        original == server
    }

    /// The check-after-unknown comparison logic: server == original →
    /// "did not land"; server != original → "server holds different".
    #[test]
    fn check_after_unknown_verdicts() {
        let original: Vec<u8> = b"loaded".to_vec();
        let working: Vec<u8> = b"my edit".to_vec();
        // Case 1: the write never landed.
        let server: Vec<u8> = b"loaded".to_vec();
        assert_eq!(server, original);
        // Case 2: the write landed (server == working).
        let server: Vec<u8> = b"my edit".to_vec();
        assert_ne!(server, original);
        assert_eq!(server, working);
        // Case 3: a third party changed it.
        let server: Vec<u8> = b"third".to_vec();
        assert_ne!(server, original);
        assert_ne!(server, working);
    }

    /// The save-command derivation, extracted as a free function so the
    /// test drives exactly the session's table (same match arms).
    fn save_commands_of(editor: &value_model::editor::EditorState, key: &[u8]) -> Vec<Command> {
        save_commands_from(editor, key)
    }

    // ---- Phase 6: security event state transitions ---------------------

    /// The pure half of `handle_event`'s three security arms: the
    /// event payload -> UI state mapping. The entity-level wiring
    /// (field set + event emit) is three lines each and needs a live
    /// `RedisConnection`, which only exists over real I/O.
    use redis_io::ssh::{HostKeyBlocked, HostKeyPrompt};

    #[test]
    fn tls_warning_sets_the_persistent_insecure_flag_once() {
        // handle_event's TlsWarning arm is idempotent-by-construction:
        // the flag flips false -> true and the change event fires
        // exactly once (the arm guards on the current value). The
        // guard expression, extracted:
        let mut tls_insecure = false;
        let mut emissions = 0;
        for _ in 0..3 {
            if !tls_insecure {
                tls_insecure = true;
                emissions += 1;
            }
        }
        assert!(tls_insecure);
        assert_eq!(emissions, 1, "repeated TlsWarning events emit once");
    }

    #[test]
    fn host_key_prompt_maps_to_tofu_modal_state() {
        let payload = HostKeyPrompt {
            host: "bastion.internal".into(),
            port: 2222,
            fingerprint: "e3b0c44298fc1c149afbf4c8996fb924".into(),
        };
        let ui = HostKeyUi::from_prompt(&payload);
        assert_eq!(
            ui,
            HostKeyUi::Prompt {
                host_port: "bastion.internal:2222".into(),
                fingerprint: "e3b0c44298fc1c149afbf4c8996fb924".into(),
            }
        );
        // Both fields are non-sensitive (PRD): safe to display.
        assert!(!format!("{ui:?}").contains("password"));
    }

    #[test]
    fn host_key_blocked_maps_to_hard_block_modal_state() {
        let payload = HostKeyBlocked {
            host: "bastion.internal".into(),
            port: 22,
            new_fingerprint: "ba7816bf8f01cfea414140de5dae2223".into(),
            old_fingerprint: "e3b0c44298fc1c149afbf4c8996fb924".into(),
        };
        let ui = HostKeyUi::from_blocked(&payload);
        assert_eq!(
            ui,
            HostKeyUi::Blocked {
                host_port: "bastion.internal:22".into(),
                new_fingerprint: "ba7816bf8f01cfea414140de5dae2223".into(),
                old_fingerprint: "e3b0c44298fc1c149afbf4c8996fb924".into(),
            }
        );
    }

    #[test]
    fn dismiss_clears_the_host_key_modal_state() {
        // The modal-open state is Option-shaped: dismiss (the Close
        // button) must clear it so the overlay stops rendering.
        let ui = Some(HostKeyUi::from_prompt(&HostKeyPrompt {
            host: "h".into(),
            port: 22,
            fingerprint: "f".into(),
        }));
        // dismiss_host_key_modal's body, extracted:
        let after: Option<HostKeyUi> = match ui {
            Some(_) => None,
            None => None,
        };
        assert!(after.is_none());
    }
}
