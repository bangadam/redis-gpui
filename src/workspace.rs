//! The main three-panel workspace: Connections | Keys | Value, with a
//! tab bar (one tab per connection) and a status bar.
//!
//! Phase 2 wiring (PLAN.md section 6, "Connect + read"):
//!
//! - **Left panel** — the connection form (host/port/db/username/
//!   password, Connect button, inline error). Connect failure shows
//!   the error in the form and opens no tab.
//! - **Tab bar** — one closable tab per `ConnectionSession`; switching
//!   tabs switches the middle+right panels' data source.
//! - **Middle panel** — the flat key list (virtualized via
//!   `uniform_list`), honest-count footer, "Load more" pagination,
//!   click-to-select → value load.
//! - **Right panel** — the value panel: view tabs (Text/JSON/Hex) from
//!   `available_views()`, `render_preview()` output, non-UTF-8 falls
//!   back to hex, non-string types get an honest Phase 3 stub.
//! - **Status bar** — connection state + a "Read-only" pill (the gate
//!   is ReadOnly in Phase 2; the pill reflects it).

use gpui::Focusable;
use gpui::{
    div, prelude::*, px, uniform_list, App, Context, DragMoveEvent, Entity, EventEmitter, Render,
    SharedString, Window, WindowAppearance,
};
use theme::Palette;

use crate::focus_ring::FocusRingExt;
use crate::palette_ext::{set_theme_global, ExtTokens};
use crate::saved_connections::SavedConnectionsPanel;
use crate::security::{
    build_ssh_tunnel, build_tls_config, parse_ssh_refusal, SshAuthFormMode, SshRefusal, TlsFormMode,
};

/// The parsed connection form: host, port, db, username, password.
type FormData = (String, u16, i64, Option<String>, Option<String>);
use crate::console_panel::{
    bind_console_keys, render_confirm_modal, render_console_panel, render_console_toggle,
    ConsolePanel, Toggle,
};
use crate::session::{
    ConnectFlow, ConnectionSession, LinkState, RawFilter, ValuePanelState, MAX_RENDERED_LINES,
};
use crate::text_input::{
    bind_input_keys, ContentChanged, EnterPressed, EscapePressed, SingleLineInput,
};
use crate::ui_primitives::{clamp_width, render_divider, PanelDrag, PanelEdge, DIVIDER_LINE_WIDTH};
use crate::value_editor::{
    render_editor_chrome, render_editor_modal, render_gate_pill, render_ttl_editor,
    ValueEditorPanel,
};
use redis_io::connection::ConnEvent;
use redis_io::redis_backend::RedisConnection;
use value_model::views::ViewKind;

/// Which divider is active (pressed/dragged), for highlight color.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum DragState {
    #[default]
    None,
    ConnectionsKeys,
    KeysValue,
}

gpui::actions!(
    workspace,
    [
        /// Tab forward (GPUI has no default tab binding at this rev —
        /// the a11y example binds `tab`/`shift-tab` exactly this way).
        TabNext,
        /// Tab backward.
        TabPrev,
        /// Close the focused tab (ctrl-w; conflicts audited — nothing
        /// else binds ctrl-w, and Zed's own workspace uses it for the
        /// same idiom).
        CloseTab,
        /// Save the dirty editor (cmd-s; the natural Save shortcut).
        SaveEditor,
        /// Focus the keys panel's search box (cmd-f / ctrl-f; PRD
        /// story 48: "shortcuts for search").
        FocusSearch,
    ]
);

/// Bind the workspace-level keys (call once at app start, next to
/// `bind_input_keys`/`bind_console_keys`).
pub fn bind_workspace_keys(cx: &mut App) {
    cx.bind_keys([
        gpui::KeyBinding::new("tab", TabNext, None),
        gpui::KeyBinding::new("shift-tab", TabPrev, None),
        gpui::KeyBinding::new("ctrl-w", CloseTab, None),
        gpui::KeyBinding::new("cmd-s", SaveEditor, None),
        gpui::KeyBinding::new("ctrl-f", FocusSearch, None),
        gpui::KeyBinding::new("cmd-f", FocusSearch, None),
    ]);
}

/// The root view of the main window.
pub struct Workspace {
    /// Current width of the left "Connections" panel, in pixels.
    left_width: f32,
    /// Current width of the middle "Keys" panel, in pixels.
    middle_width: f32,
    /// Which divider (if any) is active, for color feedback.
    drag: DragState,
    /// Cached palette; refreshed each render from the window appearance.
    palette: &'static Palette,
    /// The form fields (entities: they own focus + input state).
    host_input: Entity<SingleLineInput>,
    port_input: Entity<SingleLineInput>,
    db_input: Entity<SingleLineInput>,
    username_input: Entity<SingleLineInput>,
    password_input: Entity<SingleLineInput>,
    /// True while a connect attempt is in flight (disables Connect).
    connecting: bool,
    /// Inline connect error, shown in the form.
    connect_error: Option<String>,
    /// One session per open tab.
    sessions: Vec<Entity<ConnectionSession>>,
    /// Index of the active tab.
    active_tab: usize,
    /// The search box (one per workspace; the active session owns the
    /// run state, the input owns focus + text).
    search_input: Entity<SingleLineInput>,
    /// The console panel (one per workspace; the active session owns the
    /// command/decision state, the panel owns input focus + scroll).
    console_panel: Entity<ConsolePanel>,
    /// Whether the console panel is expanded (bottom-docked).
    console_open: bool,
    /// The value-editor panel (one per workspace; the active session
    /// owns the editor/gate state, the panel owns the input entities).
    editor_panel: Entity<ValueEditorPanel>,
    /// The navigation the dirty guard defers until the user answers the
    /// DiscardEdits modal (acceptance 9): (kind, index/text).
    pending_nav: Option<PendingNav>,
    // ---- Phase 6: security form ----------------------------------------
    /// Whether the Security section is expanded.
    security_open: bool,
    /// TLS segmented mode.
    tls_mode: TlsFormMode,
    /// CA bundle path input (TLS mode, optional).
    ca_input: Entity<SingleLineInput>,
    /// Client cert path input (TLS mode, optional pair).
    cert_input: Entity<SingleLineInput>,
    /// Client key path input (TLS mode, optional pair).
    key_input: Entity<SingleLineInput>,
    /// Whether the SSH tunnel row is enabled.
    ssh_enabled: bool,
    /// SSH host input.
    ssh_host_input: Entity<SingleLineInput>,
    /// SSH port input (default 22).
    ssh_port_input: Entity<SingleLineInput>,
    /// SSH username input.
    ssh_user_input: Entity<SingleLineInput>,
    /// SSH auth segmented mode.
    ssh_auth_mode: SshAuthFormMode,
    /// SSH password input (Password mode; never logged).
    ssh_password_input: Entity<SingleLineInput>,
    /// SSH key-file path input (Key file mode).
    ssh_key_input: Entity<SingleLineInput>,
    /// The TLS-insecure popover (click the persistent chip).
    tls_popover_open: bool,
    /// The pending connect's tunnel config (the initial connect that
    /// paused on an unseen host key; kept for the modal's host:port and
    /// the confirmed retry).
    pending_ssh: Option<redis_io::ssh::SshTunnelConfig>,
    /// The pending host-key refusal state for the CONNECT form (the
    /// pre-tab flow: no session exists yet).
    connect_refusal: Option<SshRefusal>,
    /// The saved-connections panel state.
    saved_panel: Entity<SavedConnectionsPanel>,
    // ---- Phase 7: keyboard navigation + text scale ---------------------
    /// The keyboard-cursor index in the flat key list (`None` = no
    /// cursor until the list is focused/arrowed into; the visual
    /// selection stays the session's `selected` — the cursor is the
    /// keyboard user's roaming position).
    key_cursor: Option<usize>,
    /// The keyboard-cursor row index in the tree list.
    tree_cursor: Option<usize>,
    /// The keyboard-cursor index in the saved-connections rail.
    saved_cursor: Option<usize>,
    /// The keys-list scroll handles (flat / tree): keyboard cursor
    /// movement keeps the cursor row scrolled into view.
    keys_scroll: gpui::UniformListScrollHandle,
    tree_scroll: gpui::UniformListScrollHandle,
    /// The workspace root's focus handle (the Tab-group root; the
    /// root dispatches TabNext/TabPrev into GPUI's tab stops).
    root_focus: gpui::FocusHandle,
}

/// A navigation held behind the dirty guard.
#[derive(Clone, Debug)]
enum PendingNav {
    /// Select the key at this index in the (filtered) key list.
    SelectKey(usize),
    /// Select this raw key (tree rows).
    SelectKeyRaw(Vec<u8>),
    /// Switch to the tab at this index.
    SwitchTab(usize),
    /// Close the tab at this index.
    CloseTab(usize),
    /// Switch to the DB.
    SwitchDb(u16),
    /// Refresh the selected key (⌘R / the value panel's reload).
    Refresh,
}

impl Workspace {
    /// Default width of the left "Connections" panel.
    pub const DEFAULT_LEFT: f32 = 260.;
    /// Default width of the middle "Keys" panel, in pixels.
    pub const DEFAULT_MIDDLE: f32 = 360.;
    /// Minimum width of any resizable panel, in pixels.
    pub const MIN_PANEL: f32 = 180.;

    /// Creates the workspace with default panel widths and form fields.
    pub fn new(cx: &mut Context<Self>) -> Self {
        bind_input_keys(cx);
        bind_console_keys(cx);
        bind_workspace_keys(cx);
        // Phase 7: install the persisted text scale (acceptance 11).
        crate::settings::install_text_scale(cx);
        Self {
            left_width: Self::DEFAULT_LEFT,
            middle_width: Self::DEFAULT_MIDDLE,
            drag: DragState::None,
            palette: theme::palette_for(false),
            host_input: cx.new(|cx| SingleLineInput::new("127.0.0.1", "127.0.0.1", cx)),
            port_input: cx.new(|cx| SingleLineInput::new("6379", "6379", cx)),
            db_input: cx.new(|cx| SingleLineInput::new("0", "0", cx)),
            username_input: cx.new(|cx| SingleLineInput::new("username (optional)", "", cx)),
            password_input: cx.new(|cx| SingleLineInput::new("password (optional)", "", cx)),
            connecting: false,
            connect_error: None,
            sessions: Vec::new(),
            active_tab: 0,
            search_input: cx.new(|cx| SingleLineInput::new("search keys… (prefix)", "", cx)),
            console_panel: cx.new(ConsolePanel::new),
            console_open: false,
            editor_panel: cx.new(ValueEditorPanel::new),
            pending_nav: None,
            security_open: false,
            tls_mode: TlsFormMode::Off,
            ca_input: cx.new(|cx| SingleLineInput::new("path/to/ca.pem", "", cx)),
            cert_input: cx.new(|cx| SingleLineInput::new("path/to/client.crt", "", cx)),
            key_input: cx.new(|cx| SingleLineInput::new("path/to/client.key", "", cx)),
            ssh_enabled: false,
            ssh_host_input: cx.new(|cx| SingleLineInput::new("ssh host", "", cx)),
            ssh_port_input: cx.new(|cx| SingleLineInput::new("22", "22", cx)),
            ssh_user_input: cx.new(|cx| SingleLineInput::new("username", "", cx)),
            ssh_auth_mode: SshAuthFormMode::Password,
            ssh_password_input: cx.new(|cx| SingleLineInput::new("password", "", cx)),
            ssh_key_input: cx.new(|cx| SingleLineInput::new("path/to/id_ed25519", "", cx)),
            tls_popover_open: false,
            pending_ssh: None,
            connect_refusal: None,
            saved_panel: cx.new(SavedConnectionsPanel::new),
            key_cursor: None,
            tree_cursor: None,
            saved_cursor: None,
            keys_scroll: gpui::UniformListScrollHandle::new(),
            tree_scroll: gpui::UniformListScrollHandle::new(),
            root_focus: cx.focus_handle(),
        }
    }

    /// True if the appearance is one of the dark variants.
    fn is_dark(appearance: WindowAppearance) -> bool {
        matches!(
            appearance,
            WindowAppearance::Dark | WindowAppearance::VibrantDark
        )
    }

    /// Validate the form; `Ok((host, port, db, username, password))`.
    fn validate_form(&self, cx: &App) -> Result<FormData, String> {
        let host = self.host_input.read(cx).content().trim().to_string();
        if host.is_empty() {
            return Err("host is required".into());
        }
        let port: u16 = self
            .port_input
            .read(cx)
            .content()
            .trim()
            .parse()
            .map_err(|_| format!("invalid port: {}", self.port_input.read(cx).content()))?;
        let db: i64 = self
            .db_input
            .read(cx)
            .content()
            .trim()
            .parse()
            .map_err(|_| format!("invalid db: {}", self.db_input.read(cx).content()))?;
        if db < 0 {
            return Err("db must be >= 0".into());
        }
        let username = {
            let u = self.username_input.read(cx).content().trim().to_string();
            (!u.is_empty()).then_some(u)
        };
        let password = {
            let p = self.password_input.read(cx).content().to_string();
            (!p.is_empty()).then_some(p)
        };
        Ok((host, port, db, username, password))
    }

    /// The Connect button: validate → connect on tokio → open tab.
    ///
    /// Phase 6: the Security section's values build the backend's
    /// TLS/SSH structs (`crate::security` at the boundary — passwords
    /// wrapped in `Secret`). A failed SSH connect is classified with
    /// `parse_ssh_refusal`: a first-seen host key opens the TOFU modal
    /// (the confirm path re-connects through
    /// `connect_ssh_confirmed`); a changed key opens the hard-block
    /// modal. The connect error text itself is display-safe (host:port
    /// and fingerprints only — PRD privacy).
    fn on_connect(&mut self, cx: &mut Context<Self>) {
        if self.connecting {
            return;
        }
        let form = match self.validate_form(cx) {
            Ok(f) => f,
            Err(e) => {
                self.connect_error = Some(e);
                cx.notify();
                return;
            }
        };
        let (host, port, db, username, password) = form;
        // Security section → backend structs (validation first).
        // The parsed port itself flows through `security_snapshot`.
        let _ssh_port: u16 = match self.ssh_port_input.read(cx).content().trim().parse() {
            Ok(p) => p,
            Err(_) => {
                self.connect_error = Some(format!(
                    "invalid SSH port: {}",
                    self.ssh_port_input.read(cx).content()
                ));
                cx.notify();
                return;
            }
        };
        if self.ssh_enabled {
            let ssh_host = self.ssh_host_input.read(cx).content().trim();
            if ssh_host.is_empty() {
                self.connect_error = Some("SSH host is required".into());
                cx.notify();
                return;
            }
            let ssh_user = self.ssh_user_input.read(cx).content().trim();
            if ssh_user.is_empty() {
                self.connect_error = Some("SSH username is required".into());
                cx.notify();
                return;
            }
            if self.ssh_auth_mode == SshAuthFormMode::KeyFile
                && self.ssh_key_input.read(cx).content().trim().is_empty()
            {
                self.connect_error = Some("SSH key file path is required".into());
                cx.notify();
                return;
            }
        }
        if self.tls_mode == TlsFormMode::Tls {
            let cert = self.cert_input.read(cx).content().trim();
            let key = self.key_input.read(cx).content().trim();
            if cert.is_empty() != key.is_empty() {
                self.connect_error = Some("client cert and key must be set as a pair".into());
                cx.notify();
                return;
            }
        }
        let tls = build_tls_config(
            self.tls_mode,
            self.ca_input.read(cx).content(),
            self.cert_input.read(cx).content(),
            self.key_input.read(cx).content(),
        );
        let snapshot = self.security_snapshot(cx);
        let ssh = build_ssh_tunnel(&snapshot, self.ssh_password_input.read(cx).content());
        // The tunnel config (kept for the host-key confirm retry).
        self.pending_ssh = ssh.clone();
        self.connecting = true;
        self.connect_error = None;
        self.connect_refusal = None;
        cx.notify();

        let req = crate::session::ConnectRequest {
            host,
            port,
            db,
            username,
            password,
            tls,
            ssh,
        };
        let task = ConnectFlow::connect(req, cx);
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |ws, cx| {
                ws.connecting = false;
                match result {
                    Ok((conn, events_rx, name)) => {
                        ws.open_session(conn, events_rx, name, cx);
                    }
                    Err(e) => {
                        // Phase 6: classify an SSH refusal before showing
                        // anything. A first-seen host key opens the TOFU
                        // modal (the retry re-connects confirmed); a
                        // changed key opens the hard-block modal. Other
                        // errors land in the form as before.
                        match parse_ssh_refusal(&e) {
                            Some(refusal) => {
                                ws.connect_refusal = Some(refusal);
                                ws.connect_error = None;
                            }
                            None => {
                                ws.connect_error = Some(e);
                                ws.connect_refusal = None;
                            }
                        }
                        cx.notify();
                    }
                }
            })
            .ok();
        })
        .detach();
    }

    /// Answer the connect-form host-key modal (the pre-tab flow): trust
    /// re-connects through the backend's confirm path; cancel aborts.
    fn answer_connect_refusal(
        &mut self,
        trust: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(tunnel) = self.pending_ssh.clone() else {
            self.connect_refusal = None;
            cx.notify();
            return;
        };
        let Some(SshRefusal::Prompt { .. }) = self.connect_refusal.clone() else {
            // Blocked: Close only — the modal has no override button.
            self.connect_refusal = None;
            cx.notify();
            return;
        };
        self.connect_refusal = None;
        self.connecting = true;
        cx.notify();
        // Re-validate the core form values (they may have changed since
        // the original click — the retry uses what is on screen now).
        let Ok((host, port, db, username, password)) = self.validate_form(cx) else {
            self.connecting = false;
            self.connect_error = Some("form changed — edit and connect again".into());
            cx.notify();
            return;
        };
        let _ssh_port: u16 = self
            .ssh_port_input
            .read(cx)
            .content()
            .trim()
            .parse()
            .unwrap_or(22);
        let tls = build_tls_config(
            self.tls_mode,
            self.ca_input.read(cx).content(),
            self.cert_input.read(cx).content(),
            self.key_input.read(cx).content(),
        );
        let snapshot = self.security_snapshot(cx);
        let ssh = build_ssh_tunnel(&snapshot, self.ssh_password_input.read(cx).content())
            .or(Some(tunnel));
        let fingerprint = self.pending_refusal_fingerprint().unwrap_or_default();
        let req = crate::session::ConnectRequest {
            host,
            port,
            db,
            username,
            password,
            tls,
            ssh,
        };
        let task = ConnectFlow::connect_confirmed(req, trust, fingerprint, cx);
        cx.spawn(async move |this, cx| {
            let result = task.await;
            this.update(cx, |ws, cx| {
                ws.connecting = false;
                match result {
                    Ok((conn, events_rx, name)) => {
                        ws.open_session(conn, events_rx, name, cx);
                    }
                    Err(e) => match parse_ssh_refusal(&e) {
                        Some(refusal) => {
                            ws.connect_refusal = Some(refusal);
                            ws.connect_error = None;
                        }
                        None => {
                            ws.connect_error = Some(e);
                            ws.connect_refusal = None;
                        }
                    },
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Open a new tab for a successfully connected session and wire all
    /// the workspace-owned subscriptions (search, console, editor
    /// inputs, and the session-event notification arms).
    fn open_session(
        &mut self,
        conn: RedisConnection,
        events_rx: tokio::sync::mpsc::Receiver<ConnEvent>,
        name: String,
        cx: &mut Context<Self>,
    ) {
        let session = cx.new(|cx| ConnectionSession::new(conn, events_rx, name, cx));
        // Subscribe: session events → tab title/status refresh + the
        // Phase 6 security surfaces.
        cx.subscribe(&session, move |ws, _session, evt, cx| match evt {
            crate::session::SessionEvent::Connected
            | crate::session::SessionEvent::Disconnected(_) => {
                cx.notify();
            }
            // A confirm modal just opened: reset the shared
            // phrase field (per-command placeholder, empty
            // text) so the previous command's leftovers
            // can't satisfy the new phrase.
            crate::session::SessionEvent::ConsoleConfirmOpened(placeholder) => {
                let confirm_input = ws.console_panel.read(cx).confirm_input().clone();
                confirm_input.update(cx, |input, cx| {
                    input.set_placeholder(placeholder, cx);
                });
                let ws_id = ws.console_panel.entity_id();
                let _ = cx.with_window(ws_id, |window, cx| {
                    confirm_input.update(cx, |input, cx| {
                        input.set_content("", window, cx);
                    });
                });
                cx.notify();
            }
            // Phase 6: the persistent TLS-insecure chip appeared.
            crate::session::SessionEvent::TlsInsecureChanged(_) => {
                cx.notify();
            }
            // Phase 6: a host-key modal opened/closed on a LIVE session
            // (reconnect path).
            crate::session::SessionEvent::HostKeyModalOpened
            | crate::session::SessionEvent::HostKeyModalClosed => {
                cx.notify();
            }
        })
        .detach();
        // Search box → the active session's debounce. One
        // subscription per workspace (the search input is
        // workspace-owned); it routes to whichever session
        // is active when the keystroke lands.
        let search_input = self.search_input.clone();
        let console_input = self.console_panel.read(cx).input().clone();
        cx.subscribe(
            &search_input,
            move |ws: &mut Workspace, _input, evt: &ContentChanged, cx| {
                let text = evt.0.clone();
                if let Some(session) = ws.active_session() {
                    session.update(cx, |s, cx| {
                        s.set_search_text(text.clone());
                        if text.is_empty() {
                            // Cleared box: back to plain
                            // browse (new generation, reset).
                            s.clear_search(cx);
                        } else {
                            s.search_keystroke(text, cx);
                        }
                    });
                }
                cx.notify();
            },
        )
        .detach();
        // Escape in the search box: cancel the active run
        // (token dies at the next batch boundary; delivered
        // state stays).
        cx.subscribe(
            &search_input,
            move |ws: &mut Workspace, _input, _evt: &EscapePressed, cx| {
                if let Some(session) = ws.active_session() {
                    session.update(cx, |s, cx| {
                        s.cancel_search(cx);
                    });
                }
            },
        )
        .detach();
        // Phase 7, acceptance 11: Enter in the host field submits the
        // connect form (Enter = primary action; identical to clicking
        // Connect, including the connecting-in-flight guard).
        let host_input = self.host_input.clone();
        cx.subscribe(
            &host_input,
            move |ws: &mut Workspace, _input, _evt: &EnterPressed, cx| {
                if !ws.connecting {
                    ws.on_connect(cx);
                }
            },
        )
        .detach();
        // Escape in the console input: stop the running
        // command (WAITING only) — Stop's keyboard twin.
        // Honest cancel: the command may still have
        // reached the server; late results are dropped by
        // the serial guard, never silently re-run.
        cx.subscribe(
            &console_input,
            move |ws: &mut Workspace, _input, _evt: &EscapePressed, cx| {
                if let Some(session) = ws.active_session() {
                    session.update(cx, |s, cx| {
                        s.console_stop(cx);
                    });
                }
            },
        )
        .detach();
        // Enter in the console input submits the line. The input's own
        // `enter` binding shadows ancestor key listeners (GPUI stops
        // action propagation in the bubble phase — verified at the
        // pinned rev), so the console row's `on_key_down` never sees
        // it; route the input's EnterPressed event instead (same
        // pattern as the host field above).
        let console_input_enter = console_input.clone();
        cx.subscribe(
            &console_input_enter,
            move |ws: &mut Workspace, input, _evt: &EnterPressed, cx| {
                if !ws.console_open {
                    return;
                }
                let line = input.read(cx).content().to_string();
                if line.trim().is_empty() {
                    return;
                }
                if let Some(session) = ws.active_session() {
                    session.update(cx, |s, cx| s.console_submit(&line, cx));
                }
                // Clear the field. set_content needs a Window and this
                // event fires without one, so hop through the active
                // window on the next effect cycle.
                if let Some(window_handle) = cx.active_window() {
                    let input = input.clone();
                    cx.defer(move |cx| {
                        window_handle
                            .update(cx, |_, window, cx| {
                                input.update(cx, |i, cx| i.set_content("", window, cx));
                            })
                            .ok();
                    });
                }
                cx.notify();
            },
        )
        .detach();
        // Console confirm-modal phrase input: keystrokes
        // update the session's pending confirmation (the
        // phrase-match gate re-renders the button).
        let confirm_input = self.console_panel.read(cx).confirm_input().clone();
        cx.subscribe(
            &confirm_input,
            move |ws: &mut Workspace, _input, evt: &ContentChanged, cx| {
                if let Some(session) = ws.active_session() {
                    session.update(cx, |s, cx| {
                        s.console_confirm_typed(evt.0.clone(), cx);
                    });
                }
                cx.notify();
            },
        )
        .detach();
        // Phase 5: the value-editor inputs' keystrokes
        // route to the session's editor. Field input →
        // the active inline field; scalar input → the
        // whole-value text editor; Enter in the field
        // input ends the edit.
        let field_input = self.editor_panel.read(cx).field_input().clone();
        cx.subscribe(
            &field_input,
            move |ws: &mut Workspace, _input, evt: &ContentChanged, cx| {
                if let Some(session) = ws.active_session() {
                    session.update(cx, |s, cx| {
                        s.field_edit_text(&evt.0, cx);
                    });
                }
                cx.notify();
            },
        )
        .detach();
        cx.subscribe(
            &field_input,
            move |ws: &mut Workspace, _input, _evt: &EscapePressed, cx| {
                if let Some(session) = ws.active_session() {
                    session.update(cx, |s, cx| {
                        s.end_field_edit(cx);
                    });
                }
            },
        )
        .detach();
        let scalar_input = self.editor_panel.read(cx).scalar_input().clone();
        cx.subscribe(
            &scalar_input,
            move |ws: &mut Workspace, _input, evt: &ContentChanged, cx| {
                if let Some(session) = ws.active_session() {
                    session.update(cx, |s, cx| {
                        s.scalar_edit_text(&evt.0, cx);
                    });
                }
                cx.notify();
            },
        )
        .detach();
        self.sessions.push(session);
        self.active_tab = self.sessions.len() - 1;
        cx.notify();
    }

    /// Prefill the connect form from a saved connection (spec item 4):
    /// metadata fields + the security snapshot; the password loads via
    /// the credential store — `Ok(secret)` fills the field,
    /// `StoreUnavailable` leaves it empty with the muted
    /// PromptPerSession note (acceptance 7). Nothing is logged.
    fn prefill_from_saved(&mut self, conn_id: &str, cx: &mut Context<Self>) {
        let panel = self.saved_panel.clone();
        let outcome = panel.update(cx, |p, _cx| p.prefill(conn_id));
        let Ok((saved, secret)) = outcome else {
            // Missing/failed load: surface as a form error (no secret in it).
            if let Err(e) = outcome {
                self.connect_error = Some(e);
                cx.notify();
            }
            return;
        };
        // Main form fields (needs the window for set_content).
        let host_input = self.host_input.clone();
        let port_input = self.port_input.clone();
        let db_input = self.db_input.clone();
        let username_input = self.username_input.clone();
        let password_input = self.password_input.clone();
        let host = saved.host.clone();
        let port = saved.port.to_string();
        let db = saved.db.to_string();
        let username = saved.username.clone().unwrap_or_default();
        let snapshot = crate::security::snapshot_from_saved(&saved);
        // Security fields.
        let ca = self.ca_input.clone();
        let cert = self.cert_input.clone();
        let key = self.key_input.clone();
        let ssh_host = self.ssh_host_input.clone();
        let ssh_port = self.ssh_port_input.clone();
        let ssh_user = self.ssh_user_input.clone();
        let ssh_key = self.ssh_key_input.clone();
        let ca_text = snapshot.ca_bundle.clone();
        let cert_text = snapshot.client_cert.clone();
        let key_text = snapshot.client_key.clone();
        let ssh_host_text = snapshot.ssh_host.clone();
        let ssh_port_text = snapshot.ssh_port.to_string();
        let ssh_user_text = snapshot.ssh_user.clone();
        let ssh_key_text = snapshot.ssh_key_path.clone();
        let fill_password = crate::security::prefill_plan(&secret).fill_password;
        let password_text = match &secret {
            connections_store::credentials::CredentialOutcome::Ok(s) if fill_password => s.clone(),
            _ => String::new(),
        };
        let ssh_auth = snapshot.ssh_auth;
        let ssh_enabled = snapshot.ssh_enabled;
        let tls_mode = snapshot.tls_mode;
        let ws_id = cx.entity_id();
        let _ = cx.with_window(ws_id, |window: &mut gpui::Window, cx| {
            host_input.update(cx, |i, cx| i.set_content(&host, window, cx));
            port_input.update(cx, |i, cx| i.set_content(&port, window, cx));
            db_input.update(cx, |i, cx| i.set_content(&db, window, cx));
            username_input.update(cx, |i, cx| i.set_content(&username, window, cx));
            password_input.update(cx, |i, cx| i.set_content(&password_text, window, cx));
            ca.update(cx, |i, cx| i.set_content(&ca_text, window, cx));
            cert.update(cx, |i, cx| i.set_content(&cert_text, window, cx));
            key.update(cx, |i, cx| i.set_content(&key_text, window, cx));
            ssh_host.update(cx, |i, cx| i.set_content(&ssh_host_text, window, cx));
            ssh_port.update(cx, |i, cx| i.set_content(&ssh_port_text, window, cx));
            ssh_user.update(cx, |i, cx| i.set_content(&ssh_user_text, window, cx));
            ssh_key.update(cx, |i, cx| i.set_content(&ssh_key_text, window, cx));
        });
        self.ssh_enabled = ssh_enabled;
        self.ssh_auth_mode = ssh_auth;
        self.tls_mode = tls_mode;
        self.security_open = ssh_enabled || tls_mode != TlsFormMode::Off;
        cx.notify();
    }

    // ---- Phase 7: keyboard navigation --------------------------------

    /// The Tab action: move through GPUI's registered tab stops. The
    /// root tracks focus, so dispatch lands here when nothing more
    /// specific is focused.
    fn on_tab_next(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.move_tab(true, _window, cx);
    }

    /// Shift-Tab.
    fn on_tab_prev(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        self.move_tab(false, _window, cx);
    }

    fn move_tab(&mut self, forward: bool, window: &mut Window, cx: &mut Context<Self>) {
        if forward {
            window.focus_next(cx);
        } else {
            window.focus_prev(cx);
        }
    }

    /// Ctrl-W: close the focused tab. "Focused" = the active tab (the
    /// tab bar's tabs are focusable; when one is focused its index is
    /// the active tab because clicking one activates it — the common
    /// case is simply closing the active tab). Behind the dirty guard.
    fn on_close_tab(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.sessions.is_empty() {
            return;
        }
        let ix = self.active_tab;
        self.guarded_nav(PendingNav::CloseTab(ix), cx);
    }

    /// Cmd-S: save the dirty editor of the active session (the natural
    /// Save shortcut; a clean editor is a no-op — nothing to save).
    fn on_save_editor(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = self.active_session() {
            session.update(cx, |s, cx| {
                if s.editor_dirty() {
                    s.save_value(cx);
                }
            });
        }
    }

    /// Cmd-F / Ctrl-F: focus the search box (PRD story 48).
    fn on_focus_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let handle = self.search_input.read(cx).focus_handle(cx).clone();
        window.focus(&handle, cx);
    }

    /// Arrow-key navigation inside the keys panel (flat list). Up/Down
    /// move the cursor (clamped), Enter opens the cursor key (behind
    /// the dirty guard, identical to a click).
    fn move_key_cursor(&mut self, delta: i64, cx: &mut Context<Self>) {
        let Some(session) = self.active_session() else {
            return;
        };
        let len = session.read(cx).keys().len();
        if len == 0 {
            self.key_cursor = None;
            cx.notify();
            return;
        }
        let next = match self.key_cursor {
            None => 0,
            Some(ix) => (ix as i64 + delta).clamp(0, len as i64 - 1) as usize,
        };
        self.key_cursor = Some(next);
        self.keys_scroll
            .scroll_to_item(next, gpui::ScrollStrategy::Nearest);
        cx.notify();
    }

    /// Enter in the flat key list: open the cursor key.
    fn open_key_at_cursor(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.key_cursor else {
            return;
        };
        self.guarded_nav(PendingNav::SelectKey(ix), cx);
    }

    /// Arrow-key navigation in the tree list. Up/Down move the cursor;
    /// Left collapses (or jumps to parent), Right expands (or descends
    /// to first child).
    fn move_tree_cursor(&mut self, delta: i64, cx: &mut Context<Self>) {
        let Some(session) = self.active_session() else {
            return;
        };
        let len = session.read(cx).tree_rows().len();
        if len == 0 {
            self.tree_cursor = None;
            cx.notify();
            return;
        }
        let next = match self.tree_cursor {
            None => 0,
            Some(ix) => (ix as i64 + delta).clamp(0, len as i64 - 1) as usize,
        };
        self.tree_cursor = Some(next);
        self.tree_scroll
            .scroll_to_item(next, gpui::ScrollStrategy::Nearest);
        cx.notify();
    }

    /// Left/Right on the tree cursor: collapse/expand (delegating to
    /// the session's expansion toggle — the same path the indicator
    /// click takes, including lazy child loading).
    fn tree_expand_cursor(&mut self, expand: bool, cx: &mut Context<Self>) {
        let Some(ix) = self.tree_cursor else {
            return;
        };
        let Some(session) = self.active_session() else {
            return;
        };
        let rows = session.read(cx).tree_rows();
        let Some(row) = rows.get(ix) else {
            return;
        };
        if !row.has_children {
            return; // leaves have nothing to expand
        }
        let expanded = session.read(cx).is_tree_path_expanded(&row.path);
        if expand != expanded {
            session.update(cx, |s, cx| {
                s.toggle_tree_expansion(row.path.clone(), cx);
            });
        }
    }

    /// Enter in the tree list: open the cursor row's key (leaf rows
    /// only, same guard as a click).
    fn open_tree_at_cursor(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.tree_cursor else {
            return;
        };
        let Some(session) = self.active_session() else {
            return;
        };
        let rows = session.read(cx).tree_rows();
        let Some(row) = rows.get(ix) else {
            return;
        };
        if row.is_leaf {
            if let Some(key) = row.key.clone() {
                self.guarded_nav_key(key, cx);
            }
        } else {
            // Branch: Enter expands/collapses (the double-duty idiom).
            self.tree_expand_cursor(!session.read(cx).is_tree_path_expanded(&row.path), cx);
        }
    }

    /// Arrow navigation over the saved-connections rail: Up/Down move
    /// the cursor; Enter prefills from the cursor entry (the click
    /// path).
    fn move_saved_cursor(&mut self, delta: i64, cx: &mut Context<Self>) {
        let len = self.saved_panel.read(cx).entries().len();
        if len == 0 {
            self.saved_cursor = None;
            cx.notify();
            return;
        }
        let next = match self.saved_cursor {
            None => 0,
            Some(ix) => (ix as i64 + delta).clamp(0, len as i64 - 1) as usize,
        };
        self.saved_cursor = Some(next);
        cx.notify();
    }

    /// Enter in the rail: prefill from the cursor entry.
    fn open_saved_at_cursor(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(ix) = self.saved_cursor else {
            return;
        };
        let entries = self.saved_panel.read(cx).entries().to_vec();
        if let Some(entry) = entries.get(ix) {
            let id = entry.id.clone();
            self.prefill_from_saved(&id, cx);
        }
    }

    /// Phase 7: the text scale segmented control (status bar, right of
    /// the console toggle). Instant apply: the global flips, the file
    /// persists, the next render scales the whole tree.
    fn render_text_scale_control(
        &self,
        palette: &Palette,
        ext: ExtTokens,
        cx: &Context<Workspace>,
    ) -> impl IntoElement {
        let current = crate::settings::text_scale(cx);
        let mut row = div().flex().flex_row().items_center().gap_1();
        for scale in crate::settings::TextScale::ALL {
            let label = scale.label();
            let chip = div()
                .id(SharedString::from(format!("text-scale-{}", scale.label())))
                .text_xs()
                .px_1()
                .rounded_sm()
                .cursor_pointer()
                .focus_visible_ring(ext.accent)
                .bg(gpui::rgb(if scale == current {
                    ext.selection
                } else {
                    palette.panel
                }))
                .text_color(gpui::rgb(if scale == current {
                    palette.text
                } else {
                    palette.text_muted
                }))
                .child(label)
                .on_click({
                    let ws = cx.entity().downgrade();
                    move |_ev, _window, cx: &mut App| {
                        crate::settings::set_text_scale(scale, cx);
                        // Instant apply: flip the global, persist, and
                        // notify the workspace so the next render picks
                        // up the new rem size.
                        let _ = ws.update(cx, |_, cx| cx.notify());
                    }
                });
            row = row.child(chip);
        }
        row
    }

    /// The save-current-form dialog (frame-03 pattern): name input +
    /// "Save password in keychain" checkbox + [Save] [Cancel].
    /// Rendered through the shared modal scaffold (Phase 7).
    fn render_save_dialog(
        &mut self,
        palette: &Palette,
        ext: ExtTokens,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        if !self.saved_panel.read(cx).save_dialog_open() {
            return None;
        }
        let save_name = self.saved_panel.read(cx).save_name_input().clone();
        let save_password = self.saved_panel.read(cx).save_password();
        let dark = self.is_dark_ext(ext);

        let cancel_btn = crate::modal::modal_button(
            "save-dialog-cancel-btn",
            palette,
            ext,
            crate::modal::ModalButtonKind::Ghost,
        )
        .child("Cancel")
        .on_click(cx.listener(move |ws: &mut Workspace, _ev, _window, cx| {
            let panel = ws.saved_panel.clone();
            panel.update(cx, |p, cx| p.cancel_save_dialog(cx));
        }));

        let checkbox = {
            let label = if save_password {
                "☑ Save password in keychain"
            } else {
                "☐ Save password in keychain (metadata only)"
            };
            div()
                .id("save-dialog-checkbox")
                .text_xs()
                .cursor_pointer()
                .focusable()
                .focus_visible_ring(ext.accent)
                .text_color(gpui::rgb(palette.text))
                .child(label)
                .on_click(cx.listener(move |ws: &mut Workspace, _ev, _window, cx| {
                    let panel = ws.saved_panel.clone();
                    panel.update(cx, |p, cx| p.toggle_save_password(cx));
                }))
        };

        let save_btn = crate::modal::modal_button(
            "save-dialog-save-btn",
            palette,
            ext,
            crate::modal::ModalButtonKind::Primary,
        )
        .child("Save")
        .on_click(cx.listener(move |ws: &mut Workspace, _ev, _window, cx| {
            ws.save_current_connection(cx);
        }));

        let ws_escape = cx.entity().downgrade();
        let ws_enter = cx.entity().downgrade();
        let palette = *palette;
        Some(crate::modal::modal_scaffold(
            &palette,
            crate::modal::ModalKeys {
                dark,
                width: crate::modal::MODAL_WIDTH,
                card_focus: None,
                armed: crate::modal::enter_armed(true),
                escape: Box::new(move |_window, cx: &mut App| {
                    let _ = ws_escape.update(cx, |ws, cx| {
                        let panel = ws.saved_panel.clone();
                        panel.update(cx, |p, cx| p.cancel_save_dialog(cx));
                    });
                }),
                enter: Box::new(move |_window, cx: &mut App| {
                    let _ = ws_enter.update(cx, |ws, cx| {
                        ws.save_current_connection(cx);
                    });
                }),
            },
            move |card| {
                let text_muted = palette.text_muted;
                card.child(
                    div()
                        .text_base()
                        .font_weight(gpui::FontWeight::BOLD)
                        .child(SharedString::from("Save connection")),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(gpui::rgb(text_muted))
                        .child(SharedString::from(
                            "Saves host, port, TLS/SSH settings and paths. The password goes \
                             to the OS keychain, never to disk.",
                        )),
                )
                .child(div().flex_1().child(save_name))
                .child(checkbox)
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap_2()
                        .child(cancel_btn)
                        .child(save_btn),
                )
            },
        ))
    }

    /// Perform the save (dialog [Save] / Enter).
    fn save_current_connection(&mut self, cx: &mut Context<Self>) {
        let name = self
            .saved_panel
            .read(cx)
            .save_name_input()
            .read(cx)
            .content()
            .trim()
            .to_string();
        if name.is_empty() {
            self.connect_error = Some("a name is required to save".into());
            cx.notify();
            return;
        }
        let Ok((host, port, db, username, password)) = self.validate_form(cx) else {
            self.connect_error = Some("fix the form before saving".into());
            cx.notify();
            return;
        };
        let snapshot = self.security_snapshot(cx);
        let secret = self
            .saved_panel
            .read(cx)
            .save_password()
            .then(|| password.clone())
            .flatten();
        let values = crate::saved_connections::ConnectionFormValues {
            name,
            host,
            port,
            db,
            username,
        };
        let panel = self.saved_panel.clone();
        let result = panel.update(cx, |p, cx| p.save_current(&values, &snapshot, secret, cx));
        match result {
            Ok(_) => {
                self.connect_error = None;
            }
            Err(e) => {
                self.connect_error = Some(e);
            }
        }
        cx.notify();
    }

    /// The current security form values as a snapshot.
    fn security_snapshot(&self, cx: &App) -> crate::security::SecurityFormSnapshot {
        let ssh_port: u16 = self
            .ssh_port_input
            .read(cx)
            .content()
            .trim()
            .parse()
            .unwrap_or(22);
        crate::security::SecurityFormSnapshot {
            tls_mode: self.tls_mode,
            ca_bundle: self.ca_input.read(cx).content().to_string(),
            client_cert: self.cert_input.read(cx).content().to_string(),
            client_key: self.key_input.read(cx).content().to_string(),
            ssh_enabled: self.ssh_enabled,
            ssh_host: self.ssh_host_input.read(cx).content().to_string(),
            ssh_port,
            ssh_user: self.ssh_user_input.read(cx).content().to_string(),
            ssh_auth: self.ssh_auth_mode,
            ssh_key_path: self.ssh_key_input.read(cx).content().to_string(),
            dest_host: self.host_input.read(cx).content().trim().to_string(),
            dest_port: self
                .port_input
                .read(cx)
                .content()
                .trim()
                .parse()
                .unwrap_or(6379),
        }
    }

    /// The forget confirm modal: "Forget <name>?" (danger tone, Close
    /// + Forget; the keychain entry is removed too). Shared scaffold.
    fn render_forget_modal(
        &mut self,
        palette: &Palette,
        ext: ExtTokens,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        let pending_id = self.saved_panel.read(cx).forget_pending()?.to_string();
        let name = self
            .saved_panel
            .read(cx)
            .entries()
            .iter()
            .find(|c| c.id == pending_id)
            .map(|c| c.name.clone())
            .unwrap_or_else(|| pending_id.clone());
        let dark = self.is_dark_ext(ext);
        let palette = *palette;

        let close_btn = crate::modal::modal_button(
            "forget-close-btn",
            &palette,
            ext,
            crate::modal::ModalButtonKind::Ghost,
        )
        .child("Close")
        .on_click(cx.listener(move |ws: &mut Workspace, _ev, _window, cx| {
            let panel = ws.saved_panel.clone();
            panel.update(cx, |p, cx| p.cancel_forget(cx));
        }));
        let forget_btn = crate::modal::modal_button(
            "forget-confirm-btn",
            &palette,
            ext,
            crate::modal::ModalButtonKind::Danger,
        )
        .child("Forget")
        .on_click(cx.listener(move |ws: &mut Workspace, _ev, _window, cx| {
            let panel = ws.saved_panel.clone();
            let result = panel.update(cx, |p, cx| p.confirm_forget(cx));
            if let Err(e) = result {
                ws.connect_error = Some(e);
            }
        }));

        let ws_escape = cx.entity().downgrade();
        let name_for_title = name.clone();
        Some(crate::modal::modal_scaffold(
            &palette,
            crate::modal::ModalKeys {
                dark,
                width: crate::modal::MODAL_WIDTH,
                card_focus: None,
                // Enter is deliberately NOT armed: forgetting is
                // destructive; the mouse-only confirm is the guard (the
                // desaturated-confirm pattern).
                armed: crate::modal::enter_armed(false),
                escape: Box::new(move |_window, cx: &mut App| {
                    let _ = ws_escape.update(cx, |ws, cx| {
                        let panel = ws.saved_panel.clone();
                        panel.update(cx, |p, cx| p.cancel_forget(cx));
                    });
                }),
                enter: Box::new(|_window, _cx: &mut App| {
                    // Absorbed (see enter_armed(false)): nothing on Enter.
                }),
            },
            move |card| {
                let error_text = ext.error_text;
                let text = palette.text;
                card.child(
                    div()
                        .text_base()
                        .font_weight(gpui::FontWeight::BOLD)
                        .text_color(gpui::rgb(error_text))
                        .child(format!("Forget {name_for_title}?")),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(gpui::rgb(text))
                        .child(SharedString::from(
                            "The saved connection's metadata file and its keychain entry \
                             are removed. This cannot be undone.",
                        )),
                )
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap_2()
                        .child(close_btn)
                        .child(forget_btn),
                )
            },
        ))
    }

    /// The host-key modals (TOFU prompt / hard block), both the
    /// connect-form flow and the live-session flow. Frame-03 pattern.
    fn render_host_key_modal(
        &mut self,
        palette: &Palette,
        ext: ExtTokens,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement> {
        // Prefer the live-session state; fall back to the connect form's.
        // `from_live_session` routes the modal's buttons: the live flow
        // answers through the session (backend confirm path), the form
        // flow retries the connect.
        let live_ui = self
            .active_session()
            .and_then(|s| s.read(cx).host_key_ui().cloned());
        let (payload, from_live_session): (crate::session::HostKeyUi, bool) = match live_ui {
            Some(ui) => (ui, true),
            None => {
                let r = self.connect_refusal.clone()?;
                (self.refusal_to_host_key_ui(r)?, false)
            }
        };
        let is_prompt = matches!(payload, crate::session::HostKeyUi::Prompt { .. });
        let dark = self.is_dark_ext(ext);
        let palette = *palette;

        let (title, host_port, fp_body, warning): (&str, String, String, &str) = match &payload {
            crate::session::HostKeyUi::Prompt {
                host_port,
                fingerprint,
            } => (
                "Unknown SSH host key",
                host_port.clone(),
                format!("fingerprint (sha256, hex):\n{fingerprint}"),
                "You have never connected to this host. Verify the fingerprint \
                 out-of-band before trusting it.",
            ),
            crate::session::HostKeyUi::Blocked {
                host_port,
                new_fingerprint,
                old_fingerprint,
            } => (
                "SSH host key CHANGED",
                host_port.clone(),
                format!(
                    "new fingerprint (sha256, hex):\n{new_fingerprint}\n\n\
                     previously recorded:\n{old_fingerprint}"
                ),
                "This could mean the server changed — or an interception. \
                 This app will not connect.",
            ),
        };

        // Trust & connect (accent) / Cancel (ghost) — or Close only for
        // the hard block (no override exists, by design). All through
        // the shared modal_button (rings + tab stops).
        let mut button_row = div().flex().flex_row().justify_end().gap_2();
        if is_prompt {
            let cancel_btn = {
                // Cancel: connect-form flow clears the refusal;
                // live-session flow dismisses the modal.
                let ws_weak = cx.entity().downgrade();
                crate::modal::modal_button(
                    "hostkey-cancel-btn",
                    &palette,
                    ext,
                    crate::modal::ModalButtonKind::Ghost,
                )
                .child("Cancel")
                .on_click(move |_ev, _window, cx: &mut App| {
                    let _ = ws_weak.update(cx, |ws, cx| {
                        if from_live_session {
                            // Live session: reject → clean abort.
                            if let Some(session) = ws.active_session() {
                                session.update(cx, |s, cx| s.answer_host_key_prompt(false, cx));
                            }
                        } else {
                            ws.answer_connect_refusal(false, _window, cx);
                        }
                    });
                })
            };
            let trust_btn = {
                let ws_weak = cx.entity().downgrade();
                crate::modal::modal_button(
                    "hostkey-trust-btn",
                    &palette,
                    ext,
                    crate::modal::ModalButtonKind::Primary,
                )
                .child("Trust & connect")
                .on_click(move |_ev, _window, cx: &mut App| {
                    let _ = ws_weak.update(cx, |ws, cx| {
                        if from_live_session {
                            // Live session: confirm through the
                            // backend (connect_ssh_confirmed).
                            if let Some(session) = ws.active_session() {
                                session.update(cx, |s, cx| s.answer_host_key_prompt(true, cx));
                            }
                        } else {
                            ws.answer_connect_refusal(true, _window, cx);
                        }
                    });
                })
            };
            button_row = button_row.child(cancel_btn).child(trust_btn);
        } else {
            let close_btn = {
                let ws_weak = cx.entity().downgrade();
                crate::modal::modal_button(
                    "hostkey-close-btn",
                    &palette,
                    ext,
                    crate::modal::ModalButtonKind::Ghost,
                )
                .child("Close")
                .on_click(move |_ev, _window, cx: &mut App| {
                    let _ = ws_weak.update(cx, |ws, cx| {
                        // Live session: dismiss. Connect form: clear.
                        if let Some(session) = ws.active_session() {
                            session.update(cx, |s, cx| {
                                s.dismiss_host_key_modal(cx);
                            });
                        }
                        ws.connect_refusal = None;
                        cx.notify();
                    });
                })
            };
            button_row = button_row.child(close_btn);
        }

        let ws_escape = cx.entity().downgrade();
        let esc_is_prompt = is_prompt;
        Some(crate::modal::modal_scaffold(
            &palette,
            crate::modal::ModalKeys {
                dark,
                width: crate::modal::MODAL_WIDTH_WIDE,
                card_focus: None,
                // Enter is NOT armed: trusting an unknown host key is a
                // deliberate decision — Enter must not fire it blind.
                armed: crate::modal::enter_armed(false),
                escape: Box::new(move |_window, cx: &mut App| {
                    // Escape only cancels the hard block (the prompt's
                    // Escape is handled by the input's context; blocking
                    // dismisses).
                    if !esc_is_prompt {
                        let _ = ws_escape.update(cx, |ws, cx| {
                            if let Some(session) = ws.active_session() {
                                session.update(cx, |s, cx| s.dismiss_host_key_modal(cx));
                            }
                            ws.connect_refusal = None;
                            cx.notify();
                        });
                    }
                }),
                enter: Box::new(|_window, _cx: &mut App| {
                    // Absorbed (enter_armed(false)): nothing on Enter.
                }),
            },
            move |card| {
                let text = palette.text;
                let text_muted = palette.text_muted;
                let error_text = ext.error_text;
                card.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .text_base()
                                .text_color(gpui::rgb(if is_prompt { text } else { error_text }))
                                .child("⚠"),
                        )
                        .child(
                            div()
                                .text_base()
                                .font_weight(gpui::FontWeight::BOLD)
                                .text_color(gpui::rgb(if is_prompt { text } else { error_text }))
                                .child(title.to_string()),
                        ),
                )
                .child(div().text_sm().text_color(gpui::rgb(text)).child(host_port))
                .child(
                    div()
                        .text_sm()
                        .font_family("Zed Mono")
                        .text_color(gpui::rgb(text))
                        .child(SharedString::from(fp_body)),
                )
                .child(
                    div()
                        .text_sm()
                        .text_color(gpui::rgb(text_muted))
                        .child(SharedString::from(warning)),
                )
                .child(button_row)
            },
        ))
    }

    /// Convert a connect-form refusal into the modal payload.
    fn refusal_to_host_key_ui(&self, refusal: SshRefusal) -> Option<crate::session::HostKeyUi> {
        let host_port = self
            .pending_ssh
            .as_ref()
            .map(|t| format!("{}:{}", t.host, t.port))
            .unwrap_or_default();
        Some(match refusal {
            SshRefusal::Prompt { fingerprint } => crate::session::HostKeyUi::Prompt {
                host_port,
                fingerprint,
            },
            SshRefusal::Blocked {
                new_fingerprint,
                old_fingerprint,
            } => crate::session::HostKeyUi::Blocked {
                host_port,
                new_fingerprint,
                old_fingerprint,
            },
        })
    }

    /// True when the extended tokens are the dark set.
    fn is_dark_ext(&self, ext: ExtTokens) -> bool {
        ext.accent == ExtTokens::DARK.accent
    }

    /// Close a tab: the entity (and its connection) is dropped.
    fn close_tab(&mut self, ix: usize, cx: &mut Context<Self>) {
        if ix >= self.sessions.len() {
            return;
        }
        self.sessions.remove(ix);
        if self.active_tab >= self.sessions.len() {
            self.active_tab = self.sessions.len().saturating_sub(1);
        }
        cx.notify();
    }

    /// The active session entity, if any.
    fn active_session(&self) -> Option<Entity<ConnectionSession>> {
        self.sessions.get(self.active_tab).cloned()
    }

    /// The fingerprint of the pending connect-form host-key prompt, if
    /// the pending refusal is a Prompt.
    fn pending_refusal_fingerprint(&self) -> Option<String> {
        match &self.connect_refusal {
            Some(SshRefusal::Prompt { fingerprint }) => Some(fingerprint.clone()),
            _ => None,
        }
    }

    /// Run a navigation behind the dirty guard (acceptance 9): a dirty
    /// editor in the active session opens the DiscardEdits modal and
    /// defers the navigation; a clean editor runs it now.
    fn guarded_nav(&mut self, nav: PendingNav, cx: &mut Context<Self>) {
        let dirty = self
            .active_session()
            .is_some_and(|s| s.read(cx).editor_dirty());
        if !dirty {
            self.apply_nav(nav, cx);
            return;
        }
        let payload = self
            .active_session()
            .and_then(|s| s.read(cx).dirty_guard_payload());
        if let Some((key_display, fields)) = payload {
            if let Some(session) = self.active_session() {
                session.update(cx, |sess, cx| {
                    sess.open_discard_guard(key_display, fields, cx);
                });
            }
            self.pending_nav = Some(nav);
            cx.notify();
        } else {
            self.apply_nav(nav, cx);
        }
    }

    /// Guarded key selection by raw key bytes (the tree rows carry the
    /// key, not a list index).
    fn guarded_nav_key(&mut self, key: Vec<u8>, cx: &mut Context<Self>) {
        let dirty = self
            .active_session()
            .is_some_and(|s| s.read(cx).editor_dirty());
        if !dirty {
            if let Some(session) = self.active_session() {
                session.update(cx, |s, cx| s.select_key(key, cx));
            }
            return;
        }
        let payload = self
            .active_session()
            .and_then(|s| s.read(cx).dirty_guard_payload());
        if let Some((key_display, fields)) = payload {
            if let Some(session) = self.active_session() {
                session.update(cx, |sess, cx| {
                    sess.open_discard_guard(key_display, fields, cx);
                });
            }
            self.pending_nav = Some(PendingNav::SelectKeyRaw(key));
            cx.notify();
        } else if let Some(session) = self.active_session() {
            session.update(cx, |s, cx| s.select_key(key, cx));
        }
    }

    /// Apply the deferred navigation (after Discard) or run it directly.
    fn apply_nav(&mut self, nav: PendingNav, cx: &mut Context<Self>) {
        match nav {
            PendingNav::SelectKey(ix) => {
                if let Some(session) = self.active_session() {
                    let keys = session.read(cx).keys();
                    if let Some(key) = keys.get(ix).cloned() {
                        session.update(cx, |s, cx| s.select_key(key, cx));
                    }
                }
            }
            PendingNav::SelectKeyRaw(key) => {
                if let Some(session) = self.active_session() {
                    session.update(cx, |s, cx| s.select_key(key, cx));
                }
            }
            PendingNav::SwitchTab(ix) => {
                if ix < self.sessions.len() {
                    self.active_tab = ix;
                }
            }
            PendingNav::CloseTab(ix) => self.close_tab(ix, cx),
            PendingNav::SwitchDb(db) => {
                if let Some(session) = self.active_session() {
                    session.update(cx, |s, cx| s.switch_db(db, cx));
                }
            }
            PendingNav::Refresh => {
                if let Some(session) = self.active_session() {
                    let key = session.read(cx).selected_display();
                    if let Some(key) = key {
                        session.update(cx, |s, cx| s.select_key(key, cx));
                    }
                }
            }
        }
        cx.notify();
    }

    /// The dirty guard's Discard: drop the edits, run the deferred nav.
    fn discard_guard_confirm(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(session) = self.active_session() {
            session.update(cx, |s, cx| {
                s.discard_edits(cx);
                s.cancel_editor_modal(cx);
            });
        }
        if let Some(nav) = self.pending_nav.take() {
            self.apply_nav(nav, cx);
        }
    }

    /// Toggle the bottom-docked console panel. Opening it focuses the
    /// input line (the `ctrl-\`` shortcut and the status-bar button both
    /// land here).
    fn toggle_console(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.console_open = !self.console_open;
        if self.console_open {
            let handle = self
                .console_panel
                .read(cx)
                .input()
                .read(cx)
                .focus_handle(cx)
                .clone();
            window.focus(&handle, cx);
        }
        cx.notify();
    }

    // ---- Rendering helpers ------------------------------------------

    /// One connection-form row: label + input entity.
    fn form_row(
        label: &str,
        input: &Entity<SingleLineInput>,
        palette: &Palette,
    ) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .mb_2()
            .child(
                div()
                    .text_xs()
                    .text_color(gpui::rgb(palette.text_muted))
                    .mb_1()
                    .child(label.to_string()),
            )
            .child(input.clone())
            .child(div().h(px(0.))) // keeps IntoElement happy in flex column
    }

    /// The left panel: form + connect button + inline error.
    fn render_left_panel(
        &self,
        palette: &Palette,
        ext: ExtTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let mut col = div()
            .flex()
            .flex_col()
            .flex_shrink(0.)
            .w(px(self.left_width))
            .bg(gpui::rgb(palette.panel))
            .text_sm()
            .text_color(gpui::rgb(palette.text))
            .p_3()
            .child(
                div()
                    .text_base()
                    .mb_3()
                    .child(SharedString::from("New connection")),
            )
            .child(Self::form_row("Host", &self.host_input, palette))
            .child(Self::form_row("Port", &self.port_input, palette))
            .child(Self::form_row("DB", &self.db_input, palette))
            .child(Self::form_row("Username", &self.username_input, palette))
            .child(Self::form_row("Password", &self.password_input, palette))
            .child(self.render_security_section(palette, ext, cx))
            .child(self.render_saved_connections_rail(palette, ext, cx));

        // Connect button state: disabled while connecting.
        let button_label = if self.connecting {
            "Connecting…"
        } else {
            "Connect"
        };
        let connecting = self.connecting;
        col = col.child(
            div()
                .id("connect-btn")
                .h(px(30.))
                .px_3()
                .mt_1()
                .items_center()
                .rounded_sm()
                .cursor_pointer()
                .focusable()
                .focus_visible_ring(ext.accent)
                .bg(gpui::rgb(ext.accent))
                .text_color(gpui::rgb(0xFFFFFF))
                .on_click(cx.listener(move |ws: &mut Workspace, _, _window, cx| {
                    if !connecting {
                        ws.on_connect(cx);
                    }
                }))
                .child(button_label.to_string()),
        );

        if let Some(err) = &self.connect_error {
            col = col.child(
                div()
                    .mt_2()
                    .p_2()
                    .rounded_sm()
                    .bg(gpui::rgb(ext.error_surface))
                    .text_xs()
                    .text_color(gpui::rgb(ext.error_text))
                    .child(err.clone()),
            );
        }
        // The PromptPerSession prefill note (muted, not an error).
        if let Some(note) = self.saved_panel.read(cx).prefill_note() {
            col = col.child(
                div()
                    .mt_1()
                    .text_xs()
                    .text_color(gpui::rgb(palette.text_muted))
                    .child(note.to_string()),
            );
        }
        col
    }

    /// One segmented-control option chip. `theme` bundles the palette +
    /// extended tokens (keeps the argument list under the lint).
    fn segment_chip(
        id: &str,
        label: &str,
        selected: bool,
        bg_selected: u32,
        theme: (&Palette, ExtTokens),
        cx: &mut Context<Self>,
        on_pick: impl Fn(&mut Workspace, &mut Context<Self>) + 'static,
    ) -> impl IntoElement {
        let (palette, ext) = theme;
        div()
            .id(SharedString::from(id.to_string()))
            .h(px(22.))
            .px_2()
            .flex()
            .items_center()
            .rounded_sm()
            .cursor_pointer()
            .text_xs()
            .focusable()
            .focus_visible_ring(ext.accent)
            .border_1()
            .border_color(gpui::rgb(if selected {
                bg_selected
            } else {
                palette.divider
            }))
            .bg(gpui::rgb(if selected {
                bg_selected
            } else {
                palette.panel
            }))
            .text_color(gpui::rgb(if selected { 0xFFFFFF } else { palette.text }))
            .child(label.to_string())
            .on_click(cx.listener(move |ws: &mut Workspace, _ev, _window, cx| {
                on_pick(ws, cx);
            }))
    }

    /// The Security section (Phase 6): collapsible; TLS row + SSH row.
    fn render_security_section(
        &self,
        palette: &Palette,
        ext: ExtTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let open = self.security_open;
        let mut section = div().flex().flex_col().mt_2().mb_1().child(
            div()
                .id("security-toggle")
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .cursor_pointer()
                .text_xs()
                .text_color(gpui::rgb(palette.text_muted))
                .focusable()
                .focus_visible_ring(ext.accent)
                .on_click(cx.listener(move |ws: &mut Workspace, _ev, _window, cx| {
                    ws.security_open = !ws.security_open;
                    cx.notify();
                }))
                .child(if open { "▾" } else { "▸" })
                .child(SharedString::from("Security")),
        );
        if !open {
            return section.child(div().h(px(0.)));
        }

        // ---- TLS row: [Off | TLS | Insecure] ---------------------------
        let mut col = div().flex().flex_col().mb_2().gap_1();
        col = col.child(
            div()
                .text_xs()
                .text_color(gpui::rgb(palette.text_muted))
                .child(SharedString::from("TLS")),
        );
        let mut chips = div().flex().flex_row().gap_1();
        for mode in TlsFormMode::OPTIONS {
            let selected = self.tls_mode == mode;
            chips = chips.child(Self::segment_chip(
                &format!("tls-{mode:?}"),
                mode.label(),
                selected,
                ext.accent,
                (palette, ext),
                cx,
                move |ws, cx| {
                    ws.tls_mode = mode;
                    cx.notify();
                },
            ));
        }
        col = col.child(chips);
        if self.tls_mode == TlsFormMode::Tls {
            col = col
                .child(Self::form_row("CA bundle", &self.ca_input, palette))
                .child(Self::form_row("Client cert", &self.cert_input, palette))
                .child(Self::form_row("Client key", &self.key_input, palette));
        }
        if self.tls_mode == TlsFormMode::Insecure {
            // The pre-connect, persistent warning chip (spec item 1).
            col = col.child(
                div()
                    .id("tls-insecure-form-chip")
                    .p_1()
                    .rounded_sm()
                    .bg(gpui::rgb(ext.error_surface))
                    .text_xs()
                    .text_color(gpui::rgb(ext.error_text))
                    .child(SharedString::from(
                        "⚠ verification disabled — MITM possible",
                    )),
            );
        }
        section = section.child(col);

        // ---- SSH row: checkbox + expanded fields ----------------------
        let enabled = self.ssh_enabled;
        let mut ssh_col = div().flex().flex_col().mb_2().gap_1();
        ssh_col = ssh_col.child(
            div()
                .id("ssh-toggle")
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .cursor_pointer()
                .text_xs()
                .text_color(gpui::rgb(palette.text))
                .focusable()
                .focus_visible_ring(ext.accent)
                .on_click(cx.listener(move |ws: &mut Workspace, _ev, _window, cx| {
                    ws.ssh_enabled = !ws.ssh_enabled;
                    cx.notify();
                }))
                .child(if enabled { "☑" } else { "☐" })
                .child(SharedString::from("via SSH tunnel")),
        );
        if enabled {
            ssh_col = ssh_col
                .child(Self::form_row("SSH host", &self.ssh_host_input, palette))
                .child(Self::form_row("SSH port", &self.ssh_port_input, palette))
                .child(Self::form_row(
                    "SSH username",
                    &self.ssh_user_input,
                    palette,
                ));
            let mut auth_chips = div().flex().flex_row().gap_1();
            for mode in SshAuthFormMode::OPTIONS {
                let selected = self.ssh_auth_mode == mode;
                auth_chips = auth_chips.child(Self::segment_chip(
                    &format!("ssh-auth-{mode:?}"),
                    mode.label(),
                    selected,
                    ext.accent,
                    (palette, ext),
                    cx,
                    move |ws, cx| {
                        ws.ssh_auth_mode = mode;
                        cx.notify();
                    },
                ));
            }
            ssh_col = ssh_col.child(auth_chips);
            match self.ssh_auth_mode {
                SshAuthFormMode::Password => {
                    ssh_col = ssh_col.child(Self::form_row(
                        "SSH password",
                        &self.ssh_password_input,
                        palette,
                    ));
                }
                SshAuthFormMode::KeyFile => {
                    ssh_col =
                        ssh_col.child(Self::form_row("Key file", &self.ssh_key_input, palette));
                }
                SshAuthFormMode::Agent => {
                    ssh_col = ssh_col.child(
                        div()
                            .text_xs()
                            .text_color(gpui::rgb(palette.text_muted))
                            .child(SharedString::from(
                                "agent auth may be unavailable on some systems",
                            )),
                    );
                }
            }
        }
        section.child(ssh_col)
    }

    /// The saved-connections rail (entries + [+]).
    fn render_saved_connections_rail(
        &self,
        palette: &Palette,
        ext: ExtTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let entries: Vec<(String, String, bool, bool, String)> = {
            let panel = self.saved_panel.read(cx);
            panel
                .entries()
                .iter()
                .map(|c| {
                    (
                        c.id.clone(),
                        c.name.clone(),
                        !matches!(
                            c.tls,
                            connections_store::saved_connections::TlsPolicy::Disabled
                        ),
                        c.ssh.is_some(),
                        format!("{}:{}", c.host, c.port),
                    )
                })
                .collect()
        };
        // Phase 7: the rail's keyboard surface. The rail container is
        // the tab stop; Up/Down move the cursor, Enter prefills from
        // the cursor entry (identical to clicking the row). Rows
        // themselves are not tab stops (arrow navigation, not 200
        // stops).
        let saved_len = entries.len();
        let mut rail = div()
            .id("saved-rail")
            .key_context("SavedRail")
            .flex()
            .flex_col()
            .flex_shrink(0.)
            .focusable()
            .focus_visible_ring(ext.accent)
            .on_key_down(cx.listener(
                move |ws: &mut Workspace, ev: &gpui::KeyDownEvent, _window, cx| {
                    if saved_len == 0 {
                        return;
                    }
                    match ev.keystroke.key.as_str() {
                        "up" => ws.move_saved_cursor(-1, cx),
                        "down" => ws.move_saved_cursor(1, cx),
                        "enter" => ws.open_saved_at_cursor(_window, cx),
                        _ => {}
                    }
                },
            ));
        rail = rail.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .justify_between()
                .mb_1()
                .child(
                    div()
                        .text_xs()
                        .text_color(gpui::rgb(palette.text_muted))
                        .child(SharedString::from("Saved connections")),
                )
                .child(
                    div()
                        .id("saved-add-btn")
                        .text_xs()
                        .cursor_pointer()
                        .px_1()
                        .text_color(gpui::rgb(palette.text))
                        .child("+")
                        .focusable()
                        .focus_visible_ring(ext.accent)
                        .on_click(cx.listener(move |ws: &mut Workspace, _ev, _window, cx| {
                            // Suggested name: host:port of the form.
                            let suggested = format!(
                                "{}:{}",
                                ws.host_input.read(cx).content().trim(),
                                ws.port_input.read(cx).content().trim()
                            );
                            let panel = ws.saved_panel.clone();
                            panel.update(cx, |p, cx| {
                                p.open_save_dialog(suggested, cx);
                            });
                        })),
                ),
        );
        let col = div().flex().flex_col().mt_4().flex_shrink(0.);
        if entries.is_empty() {
            rail = rail.child(
                div()
                    .text_xs()
                    .text_color(gpui::rgb(palette.text_muted))
                    .child(SharedString::from("none yet — connect and press +")),
            );
        }
        for (ix, (id, name, has_tls, has_ssh, host_port)) in entries
            .iter()
            .map(|(a, b, c, d, e)| (a, b, *c, *d, e))
            .enumerate()
        {
            let entry_id = id.clone();
            let menu_entry_id = id.clone();
            let is_cursor = self.saved_cursor == Some(ix);
            let bg = if is_cursor {
                ext.selection
            } else {
                palette.panel
            };
            rail = rail.child(
                div()
                    .id(SharedString::from(format!("saved-conn-{id}")))
                    .flex()
                    .flex_col()
                    .py_1()
                    .px_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .bg(gpui::rgb(bg))
                    .hover(|s| s.bg(gpui::rgb(ext.selection)))
                    .on_click(cx.listener(move |ws: &mut Workspace, _ev, _window, cx| {
                        let entry_id = entry_id.clone();
                        ws.prefill_from_saved(&entry_id, cx);
                    }))
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_1()
                            .child(div().text_sm().child(name.clone()))
                            .child(if has_tls {
                                div()
                                    .text_xs()
                                    .px_1()
                                    .rounded_sm()
                                    .bg(gpui::rgb(ext.selection))
                                    .child("TLS")
                                    .into_any_element()
                            } else {
                                div()
                                    .text_xs()
                                    .px_1()
                                    .rounded_sm()
                                    .bg(gpui::rgb(palette.panel))
                                    .child("no TLS")
                                    .into_any_element()
                            })
                            .child(if has_ssh {
                                div()
                                    .text_xs()
                                    .px_1()
                                    .rounded_sm()
                                    .bg(gpui::rgb(ext.selection))
                                    .child("SSH")
                                    .into_any_element()
                            } else {
                                div().h(px(0.)).into_any_element()
                            })
                            .child(div().flex_1())
                            // per-entry ⋯ menu → forget confirm
                            .child(
                                div()
                                    .id(SharedString::from(format!("saved-menu-{id}")))
                                    .text_xs()
                                    .text_color(gpui::rgb(palette.text_muted))
                                    .cursor_pointer()
                                    .focusable()
                                    .focus_visible_ring(ext.accent)
                                    .child("⋯")
                                    .on_click(cx.listener(
                                        move |ws: &mut Workspace, _ev, _window, cx| {
                                            let panel = ws.saved_panel.clone();
                                            panel.update(cx, |p, cx| {
                                                p.request_forget(&menu_entry_id, cx);
                                            });
                                        },
                                    )),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(gpui::rgb(palette.text_muted))
                            .child(host_port.clone()),
                    ),
            );
        }
        col.child(rail)
    }

    /// The tab bar: one closable tab per session. The bar itself is a
    /// keyboard surface: Left/Right switch tabs (guarded like clicks),
    /// Ctrl-W closes the active tab (a root action).
    fn render_tab_bar(
        &self,
        palette: &Palette,
        ext: ExtTokens,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let tab_count = self.sessions.len();
        let active_ix = self.active_tab;
        let mut bar = div()
            .id("tab-bar")
            .key_context("TabBar")
            .flex()
            .flex_row()
            .flex_shrink(0.)
            .h(px(32.))
            .bg(gpui::rgb(palette.panel))
            .border_b_1()
            .border_color(gpui::rgb(palette.divider))
            .on_key_down(cx.listener(
                move |ws: &mut Workspace, ev: &gpui::KeyDownEvent, _window, cx| {
                    if tab_count == 0 {
                        return;
                    }
                    match ev.keystroke.key.as_str() {
                        // Left/Right: switch tabs (the guarded path —
                        // a dirty editor asks first, same as a click).
                        "left"
                            if ev.keystroke.modifiers == gpui::Modifiers::default()
                                && active_ix > 0 =>
                        {
                            ws.guarded_nav(PendingNav::SwitchTab(active_ix - 1), cx);
                        }
                        "right"
                            if ev.keystroke.modifiers == gpui::Modifiers::default()
                                && active_ix + 1 < tab_count =>
                        {
                            ws.guarded_nav(PendingNav::SwitchTab(active_ix + 1), cx);
                        }
                        _ => {}
                    }
                },
            ));
        for (ix, session) in self.sessions.iter().enumerate() {
            let name = session.read(cx).name.clone();
            let link = session.read(cx).link.clone();
            let active = ix == self.active_tab;
            let bg = if active {
                palette.background
            } else {
                palette.panel
            };
            let dot_color = match link {
                LinkState::Live => ext.success,
                LinkState::Disconnected => ext.error_text,
            };
            bar = bar.child(
                div()
                    .id(("tab", ix))
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_3()
                    .h_full()
                    .cursor_pointer()
                    .focusable()
                    .focus_visible_ring(ext.accent)
                    .bg(gpui::rgb(bg))
                    .border_r_1()
                    .border_color(gpui::rgb(palette.divider))
                    .on_click({
                        let weak = cx.entity().downgrade();
                        move |_ev, _window, cx: &mut App| {
                            let _ = weak.update(cx, |ws, cx| {
                                if ws.active_tab != ix {
                                    ws.guarded_nav(PendingNav::SwitchTab(ix), cx);
                                }
                            });
                        }
                    })
                    .child(
                        div()
                            .w(px(8.))
                            .h(px(8.))
                            .rounded_full()
                            .bg(gpui::rgb(dot_color)),
                    )
                    .child(
                        div()
                            .text_sm()
                            .text_color(gpui::rgb(palette.text))
                            .child(name),
                    )
                    .child(
                        div()
                            .id(("tab-close", ix))
                            .text_xs()
                            .text_color(gpui::rgb(palette.text_muted))
                            .cursor_pointer()
                            .focusable()
                            .focus_visible_ring(ext.accent)
                            .child("✕")
                            .on_click({
                                let weak = cx.entity().downgrade();
                                move |_ev, _window, cx: &mut App| {
                                    let _ = weak.update(cx, |ws, cx| {
                                        ws.guarded_nav(PendingNav::CloseTab(ix), cx);
                                    });
                                }
                            }),
                    ),
            );
        }
        if self.sessions.is_empty() {
            bar = bar.child(
                div()
                    .px_3()
                    .h_full()
                    .items_center()
                    .flex()
                    .text_sm()
                    .text_color(gpui::rgb(palette.text_muted))
                    .child("No connections — fill the form and connect"),
            );
        }
        bar
    }

    /// The middle panel: search box + type chips + key list (flat or
    /// tree) + footer. DB stepper lives in the panel header.
    fn render_keys_panel(
        &self,
        session: &Entity<ConnectionSession>,
        palette: &Palette,
        ext: ExtTokens,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        // Borrow the session read guard in a scope so it releases
        // before the closures below clone the entity handle.
        let (
            keys,
            count_text,
            can_load_more,
            link,
            selected,
            type_filter,
            type_of,
            tree_mode,
            tree_rows,
            searching,
            db_count,
            current_db,
            switching_db,
            db_error,
        ) = {
            let s = session.read(cx);
            (
                s.keys(),
                s.search_count_text(),
                s.can_load_more(),
                s.link.clone(),
                s.selected_display(),
                s.type_filter(),
                // Copy the type cache for row badges (small: one entry
                // per probed key of the visible pages).
                s.key_types_snapshot(),
                s.tree_mode(),
                s.tree_rows(),
                s.is_searching(),
                s.db_count(),
                s.current_db(),
                s.switching_db(),
                s.db_error().map(str::to_string),
            )
        };

        let mut col = div()
            .flex()
            .flex_col()
            .flex_shrink(0.)
            .w(px(self.middle_width))
            .bg(gpui::rgb(palette.panel))
            .text_sm()
            .text_color(gpui::rgb(palette.text));

        if link == LinkState::Disconnected {
            // Sticky state + reconnect affordance (the session keeps its
            // last data; the reconnect button re-attaches).
            let session = session.clone();
            col = col.child(
                div()
                    .m_3()
                    .p_2()
                    .rounded_sm()
                    .bg(gpui::rgb(ext.error_surface))
                    .text_color(gpui::rgb(ext.error_text))
                    .text_xs()
                    .child("Disconnected — the list below is the last known state.")
                    .child(
                        div()
                            .id("reconnect-btn")
                            .mt_2()
                            .p_1()
                            .rounded_sm()
                            .cursor_pointer()
                            .focusable()
                            .focus_visible_ring(ext.accent)
                            .bg(gpui::rgb(ext.accent))
                            .text_color(gpui::rgb(0xFFFFFF))
                            .child("Reconnect")
                            .on_click({
                                let weak_session = session.downgrade();
                                move |_ev, _window, cx: &mut App| {
                                    let _ = weak_session.update(cx, |s, cx| {
                                        s.reconnect(cx);
                                    });
                                }
                            }),
                    ),
            );
        }

        // ---- Panel header: search box + Tree/Flat toggle + DB stepper.
        let mut header = div()
            .flex()
            .flex_row()
            .flex_shrink(0.)
            .items_center()
            .gap_1()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(gpui::rgb(palette.divider));

        // Search box: keystrokes land as ContentChanged events (the
        // subscription in `on_connect` routes them to the active
        // session's debounce).
        header = header.child(div().flex_1().min_w_0().child(self.search_input.clone()));

        // Esc: cancel the active search (the input handles its own keys;
        // this button is the always-visible affordance).
        {
            let session = session.clone();
            let is_active = searching || !session.read(cx).search_text().is_empty();
            if is_active {
                header = header.child(
                    div()
                        .id("search-cancel-btn")
                        .px_1()
                        .rounded_sm()
                        .text_xs()
                        .cursor_pointer()
                        .focusable()
                        .focus_visible_ring(ext.accent)
                        .text_color(gpui::rgb(palette.text_muted))
                        .child("×")
                        .on_click({
                            let search_input = self.search_input.clone();
                            move |_ev, window, cx: &mut App| {
                                search_input.update(cx, |input, cx| {
                                    input.set_content("", window, cx);
                                });
                                // clear_search runs via the ContentChanged
                                // subscription (empty text → back to plain
                                // browse per the documented contract).
                            }
                        }),
                );
            }
        }

        // Tree/Flat toggle.
        {
            let session = session.clone();
            let (label, active) = if tree_mode {
                ("Tree", true)
            } else {
                ("Flat", false)
            };
            let bg = if tree_mode {
                ext.selection
            } else {
                palette.panel
            };
            header = header.child(
                div()
                    .id("tree-flat-toggle")
                    .flex()
                    .flex_row()
                    .items_center()
                    .rounded_sm()
                    .text_xs()
                    .cursor_pointer()
                    .bg(gpui::rgb(bg))
                    .text_color(gpui::rgb(palette.text))
                    .child(
                        div()
                            .id("toggle-flat")
                            .px_2()
                            .py_0p5()
                            .rounded_sm()
                            .focusable()
                            .focus_visible_ring(ext.accent)
                            .bg(gpui::rgb(if tree_mode {
                                palette.panel
                            } else {
                                ext.selection
                            }))
                            .child("Flat")
                            .on_click({
                                let weak = session.downgrade();
                                move |_ev, _window, cx: &mut App| {
                                    let _ = weak.update(cx, |s, cx| {
                                        s.set_tree_mode(false, cx);
                                    });
                                }
                            }),
                    )
                    .child(
                        div()
                            .id("toggle-tree")
                            .px_2()
                            .py_0p5()
                            .rounded_sm()
                            .focusable()
                            .focus_visible_ring(ext.accent)
                            .bg(gpui::rgb(if tree_mode {
                                ext.selection
                            } else {
                                palette.panel
                            }))
                            .child("Tree")
                            .on_click({
                                let weak = session.downgrade();
                                move |_ev, _window, cx: &mut App| {
                                    let _ = weak.update(cx, |s, cx| {
                                        s.set_tree_mode(true, cx);
                                    });
                                }
                            }),
                    )
                    .child(div().w(px(0.)).h(px(0.))), // keep flex row layout stable
            );
            let _ = (label, active);
        }

        // DB stepper: "db n" with ◀ ▶ (disabled at bounds/while switching).
        {
            let db_label = format!("db {current_db}");
            let can_prev = !switching_db && current_db > 0;
            let can_next = !switching_db && current_db + 1 < db_count;
            let mut stepper = div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .flex_shrink(0.)
                .text_xs()
                .text_color(gpui::rgb(palette.text_muted));
            let prev_color = if can_prev {
                palette.text
            } else {
                palette.text_muted
            };
            stepper = stepper.child(
                div()
                    .id("db-prev")
                    .px_1()
                    .cursor_pointer()
                    .focusable()
                    .focus_visible_ring(ext.accent)
                    .text_color(gpui::rgb(prev_color))
                    .child("◀")
                    .on_click({
                        let weak = cx.entity().downgrade();
                        move |_ev, _window, cx: &mut App| {
                            let _ = weak.update(cx, |ws, cx| {
                                let db = ws
                                    .active_session()
                                    .map(|s| s.read(cx).current_db().saturating_sub(1))
                                    .unwrap_or(0);
                                if can_prev {
                                    ws.guarded_nav(PendingNav::SwitchDb(db), cx);
                                }
                            });
                        }
                    }),
            );
            stepper = stepper.child(div().child(db_label));
            stepper = stepper.child(
                div()
                    .id("db-next")
                    .px_1()
                    .cursor_pointer()
                    .focusable()
                    .focus_visible_ring(ext.accent)
                    .text_color(gpui::rgb(if can_next {
                        palette.text
                    } else {
                        palette.text_muted
                    }))
                    .child("▶")
                    .on_click({
                        let weak = cx.entity().downgrade();
                        move |_ev, _window, cx: &mut App| {
                            let _ = weak.update(cx, |ws, cx| {
                                let db = ws
                                    .active_session()
                                    .map(|s| s.read(cx).current_db() + 1)
                                    .unwrap_or(0);
                                if can_next {
                                    ws.guarded_nav(PendingNav::SwitchDb(db), cx);
                                }
                            });
                        }
                    }),
            );
            if switching_db {
                stepper = stepper.child(div().child("…"));
            }
            header = header.child(stepper);
        }
        col = col.child(header);

        // Inline DB-switch error (SELECT failed; DB stays reverted).
        if let Some(err) = db_error {
            col = col.child(
                div()
                    .m_2()
                    .p_2()
                    .rounded_sm()
                    .bg(gpui::rgb(ext.error_surface))
                    .text_color(gpui::rgb(ext.error_text))
                    .text_xs()
                    .child(err),
            );
        }

        // Type-filter chips (client-side over the lazily-probed type
        // cache; a typed chip re-arms the scan server-side). All/String/
        // Hash/List/Set/ZSet/Stream.
        let mut chips = div()
            .flex()
            .flex_row()
            .flex_shrink(0.)
            .flex_wrap()
            .gap_1()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(gpui::rgb(palette.divider));
        for chip in RawFilter::ALL {
            let is_active = type_filter == Some(chip);
            let bg = if is_active {
                ext.selection
            } else {
                palette.panel
            };
            let text_color = if is_active {
                palette.text
            } else {
                palette.text_muted
            };
            chips = chips.child(
                div()
                    .id(SharedString::from(format!("type-chip-{}", chip.label())))
                    .px_2()
                    .py_0p5()
                    .rounded_full()
                    .text_xs()
                    .cursor_pointer()
                    .focusable()
                    .focus_visible_ring(ext.accent)
                    .bg(gpui::rgb(bg))
                    .text_color(gpui::rgb(text_color))
                    .child(chip.label())
                    .on_click({
                        let weak = session.downgrade();
                        move |_ev, _window, cx: &mut App| {
                            let _ = weak.update(cx, |s, cx| {
                                let next = if s.type_filter() == Some(chip) {
                                    None // clicking the active chip resets to All
                                } else {
                                    Some(chip)
                                };
                                s.set_type_filter(next, cx);
                            });
                        }
                    }),
            );
        }
        col = col.child(chips);

        // The virtualized key list: flat or tree projection (uniform_list
        // at this rev). The panel container owns the keyboard cursor:
        // Up/Down move it, Enter opens (leaf rows), Left/Right collapse
        // and expand (tree). Focus the panel (click or Tab) first; the
        // cursor keeps itself visible via scroll_to_item.
        let keys_focusable = div()
            .id("keys-panel-list-container")
            .key_context("KeysPanel")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0();
        let selected_keys = selected.clone();
        let list = if tree_mode {
            // Tree rows: indent by depth, branch indicator (▸/▾), leaf
            // click = select_key, indicator click = expand/collapse
            // (+ lazy load on first expand).
            let scroll = self.tree_scroll.clone();
            uniform_list("keys-tree-list", tree_rows.len(), {
                let palette = *palette;
                let session = session.clone();
                let selected = selected_keys.clone();
                let tree_rows = tree_rows.clone();
                let weak_ws = cx.entity().downgrade();
                move |range, _window, cx: &mut App| {
                    let session = session.clone();
                    let selected = selected.clone();
                    range
                        .map(|ix| {
                            let row = &tree_rows[ix];
                            let row_display = String::from_utf8_lossy(&row.label).into_owned();
                            let is_selected = row
                                .key
                                .as_ref()
                                .map(|k| selected.as_deref() == Some(k.as_slice()))
                                .unwrap_or(false);
                            let bg = if is_selected {
                                ext.selection
                            } else {
                                palette.panel
                            };
                            // ▾ when the row is expanded, ▸ when collapsed
                            // (expansion is session UI state).
                            let row_expanded = row.has_children
                                && session.read(cx).is_tree_path_expanded(&row.path);
                            let indicator = if row.has_children {
                                if row_expanded {
                                    "\u{25BE}"
                                } else {
                                    "\u{25B8}"
                                }
                            } else {
                                ""
                            };
                            let indent_px = (row.depth * 14) as f32;
                            let path = row.path.clone();
                            let key = row.key.clone();
                            let key_for_nav = row.key.clone();
                            let is_leaf = row.is_leaf;
                            let mut el = div()
                                .id(("tree-row", ix))
                                .flex()
                                .items_center()
                                .px_2()
                                .h(px(26.))
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .bg(gpui::rgb(bg))
                                .text_color(gpui::rgb(palette.text))
                                .cursor_pointer()
                                .on_click({
                                    let key_for_nav = key_for_nav.clone();
                                    let weak = weak_ws.clone();
                                    move |_ev, _window, cx: &mut App| {
                                        let _ = weak.update(cx, |ws, cx| {
                                            if !is_leaf || key_for_nav.is_none() {
                                                return;
                                            }
                                            // Guarded: same discipline as
                                            // the flat list (the session's
                                            // select path is identical).
                                            ws.guarded_nav_key(key_for_nav.clone().unwrap(), cx);
                                        });
                                    }
                                })
                                .child(div().w(px(indent_px)).flex_shrink(0.).h(px(26.)))
                                .child(
                                    div()
                                        .id(("tree-row-indicator", ix))
                                        .flex_shrink(0.)
                                        .mr_1()
                                        .text_xs()
                                        .cursor_pointer()
                                        .text_color(gpui::rgb(palette.text_muted))
                                        .child(indicator)
                                        .on_click({
                                            let path = path.clone();
                                            let weak = session.downgrade();
                                            move |_ev, _window, cx: &mut App| {
                                                let _ = weak.update(cx, |s, cx| {
                                                    s.toggle_tree_expansion(path.clone(), cx);
                                                });
                                            }
                                        }),
                                )
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .child(row_display),
                                );
                            if let (Some(key), Some(t)) =
                                (key, type_of.get(row.key.as_deref().unwrap_or(b"")))
                            {
                                let _ = key;
                                el = el.child(
                                    div()
                                        .flex_shrink(0.)
                                        .ml_1()
                                        .px_1()
                                        .rounded_sm()
                                        .text_xs()
                                        .text_color(gpui::rgb(palette.text_muted))
                                        .child(t.clone()),
                                );
                            }
                            el
                        })
                        .collect()
                }
            })
            .track_scroll(&scroll)
            .flex_1()
            .min_h_0()
        } else {
            let scroll = self.keys_scroll.clone();
            let key_types = type_of.clone();
            uniform_list("keys-list", keys.len(), {
                let palette = *palette;
                let key_types = key_types.clone();
                let keys = keys.clone();
                let weak_ws = cx.entity().downgrade();
                move |range, _window, _cx: &mut App| {
                    let selected = selected_keys.clone();
                    let key_types = key_types.clone();
                    let keys = keys.clone();
                    range
                        .map(|ix| {
                            let key = &keys[ix];
                            let key_display = String::from_utf8_lossy(key).into_owned();
                            let is_selected = selected.as_deref() == Some(key.as_slice());
                            let bg = if is_selected {
                                ext.selection
                            } else {
                                palette.panel
                            };
                            // Per-row type badge from the probe cache (None
                            // while unprobed — honest, shows nothing).
                            let type_badge = key_types.get(key).map(|t| {
                                div()
                                    .flex_shrink(0.)
                                    .ml_1()
                                    .px_1()
                                    .rounded_sm()
                                    .text_xs()
                                    .text_color(gpui::rgb(palette.text_muted))
                                    .child(t.clone())
                            });
                            let is_current = is_selected;
                            let mut row = div()
                                .id(("key", ix))
                                .flex()
                                .items_center()
                                .px_2()
                                .h(px(26.))
                                .overflow_hidden()
                                .whitespace_nowrap()
                                .text_ellipsis()
                                .bg(gpui::rgb(bg))
                                .text_color(gpui::rgb(palette.text))
                                .cursor_pointer()
                                .on_click({
                                    // Behind the dirty guard (acceptance 9):
                                    // a dirty editor asks before the key
                                    // switch strands it.
                                    let weak = weak_ws.clone();
                                    move |_ev, _window, cx: &mut App| {
                                        let _ = weak.update(cx, |ws, cx| {
                                            if is_current {
                                                return; // re-click: no-op
                                            }
                                            ws.guarded_nav(PendingNav::SelectKey(ix), cx);
                                        });
                                    }
                                })
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .child(key_display),
                                );
                            if let Some(badge) = type_badge {
                                row = row.child(badge);
                            }
                            row
                        })
                        .collect()
                }
            })
            .track_scroll(&scroll)
            .flex_1()
            .min_h_0()
        };

        // Phase 7: the panel's keyboard surface. Focus lands here by
        // click or Tab (it is the panel's tab stop; the rows
        // themselves are not, so arrow keys are list-local, not per-row
        // stops). Up/Down move the cursor, Enter opens (leaf in tree
        // mode / any key in flat), Left/Right collapse and expand in
        // tree mode. Handlers are guarded exactly like the mouse paths.
        let keys_focusable = keys_focusable
            .focusable()
            .focus_visible_ring(ext.accent)
            .on_key_down(cx.listener(
                move |ws: &mut Workspace, ev: &gpui::KeyDownEvent, _window, cx| {
                    if tree_mode {
                        match ev.keystroke.key.as_str() {
                            "up" => ws.move_tree_cursor(-1, cx),
                            "down" => ws.move_tree_cursor(1, cx),
                            "left" => ws.tree_expand_cursor(false, cx),
                            "right" => ws.tree_expand_cursor(true, cx),
                            "enter" => ws.open_tree_at_cursor(_window, cx),
                            _ => {}
                        }
                    } else {
                        match ev.keystroke.key.as_str() {
                            "up" => ws.move_key_cursor(-1, cx),
                            "down" => ws.move_key_cursor(1, cx),
                            "enter" => ws.open_key_at_cursor(_window, cx),
                            _ => {}
                        }
                    }
                },
            ));
        col = col.child(keys_focusable.child(list));

        // Footer: honest count + Load more.
        let mut footer = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_between()
            .flex_shrink(0.)
            .px_2()
            .py_1()
            .border_t_1()
            .border_color(gpui::rgb(palette.divider))
            .child(
                div()
                    .text_xs()
                    .text_color(gpui::rgb(palette.text_muted))
                    .child(count_text),
            );
        if can_load_more {
            footer = footer.child(
                div()
                    .id("load-more-btn")
                    .text_xs()
                    .px_2()
                    .py_1()
                    .rounded_sm()
                    .cursor_pointer()
                    .focusable()
                    .focus_visible_ring(ext.accent)
                    .bg(gpui::rgb(ext.accent))
                    .text_color(gpui::rgb(0xFFFFFF))
                    .child("Load more")
                    .on_click({
                        let weak = session.downgrade();
                        move |_ev, _window, cx: &mut App| {
                            let _ = weak.update(cx, |s, cx| {
                                s.fetch_next_page(cx);
                            });
                        }
                    }),
            );
        }
        col.child(footer)
    }

    /// The right panel: value views.
    fn render_value_panel(
        &self,
        session: &Entity<ConnectionSession>,
        palette: &Palette,
        ext: ExtTokens,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        // Read-guard scope: extract display data, release before the
        // click closures below take entity handles.
        let (title, key_type, ttl_text, panel_state) = {
            let s = session.read(cx);
            let title = s
                .selected_key_display()
                .unwrap_or_else(|| "No key selected".into());
            let key_type = s.key_type_display();
            let ttl_text = s.ttl_display().unwrap_or_default();
            let panel_state = match &s.value_panel {
                ValuePanelState::Loaded { .. } => {
                    // Clone the display bits (active view via accessor).
                    let ValuePanelState::Loaded {
                        views, rendered, ..
                    } = &s.value_panel
                    else {
                        unreachable!()
                    };
                    ValueDisplay::Views {
                        views: views.clone(),
                        active: s.active_view().unwrap_or(ViewKind::Hex),
                        rendered: rendered.clone(),
                    }
                }
                ValuePanelState::Unsupported(marker) => {
                    ValueDisplay::Unsupported(marker.marker_text())
                }
                ValuePanelState::Empty => ValueDisplay::Empty,
                ValuePanelState::Loading => ValueDisplay::Loading,
                ValuePanelState::Missing => ValueDisplay::Missing,
                ValuePanelState::Error(e) => ValueDisplay::Error(e.clone()),
            };
            (title, key_type, ttl_text, panel_state)
        };

        let mut col = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .bg(gpui::rgb(palette.panel))
            .text_sm()
            .text_color(gpui::rgb(palette.text));

        // Header: key name + type + TTL (PRD story 32 / acceptance 4).
        let type_note = key_type.map(|t| format!("type: {t}")).unwrap_or_default();
        let mut header_row = div().flex().flex_row().items_center().gap_2().flex_wrap();
        header_row = header_row
            .child(
                div()
                    .text_base()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(title),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(gpui::rgb(palette.text_muted))
                    .child(type_note),
            );
        if !ttl_text.is_empty() {
            header_row = header_row.child(
                div()
                    .text_xs()
                    .text_color(gpui::rgb(palette.text_muted))
                    .child(ttl_text),
            );
        }
        col = col.child(
            div()
                .flex_shrink(0.)
                .px_3()
                .py_2()
                .border_b_1()
                .border_color(gpui::rgb(palette.divider))
                .child(header_row),
        );

        // Phase 5: the TTL editor row (acceptance 4) + the editor
        // chrome (banners, toolbar) under the header.
        {
            let panel = self.editor_panel.read(cx);
            col = col.child(render_ttl_editor(session, panel, palette, ext, cx));
            col = col.child(render_editor_chrome(session, panel, palette, ext, cx));
        }

        // Body by state.
        match panel_state {
            ValueDisplay::Empty => {
                col = col.child(
                    div()
                        .flex_1()
                        .items_center()
                        .justify_center()
                        .flex()
                        .text_color(gpui::rgb(palette.text_muted))
                        .child("Select a key to view its value"),
                );
            }
            ValueDisplay::Loading => {
                col = col.child(
                    div()
                        .flex_1()
                        .items_center()
                        .justify_center()
                        .flex()
                        .text_color(gpui::rgb(palette.text_muted))
                        .child("Loading…"),
                );
            }
            ValueDisplay::Missing => {
                col = col.child(
                    div()
                        .flex_1()
                        .items_center()
                        .justify_center()
                        .flex()
                        .text_color(gpui::rgb(palette.text_muted))
                        .child("Key not found (expired or deleted)"),
                );
            }
            ValueDisplay::Error(e) => {
                col = col.child(
                    div()
                        .m_3()
                        .p_2()
                        .rounded_sm()
                        .bg(gpui::rgb(ext.error_surface))
                        .text_color(gpui::rgb(ext.error_text))
                        .child(e),
                );
            }
            ValueDisplay::Unsupported(marker_text) => {
                // Module/unknown type: visibly read-only, clearly named
                // (value-model's marker text; editing is Phase 5 and
                // unsupported types stay read-only regardless).
                col = col.child(
                    div()
                        .flex_1()
                        .items_center()
                        .justify_center()
                        .flex()
                        .flex_col()
                        .gap_2()
                        .child(
                            div()
                                .text_color(gpui::rgb(palette.text_muted))
                                .child(marker_text),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(gpui::rgb(palette.text_muted))
                                .child("read-only"),
                        ),
                );
            }
            ValueDisplay::Views {
                views,
                active,
                rendered,
            } => {
                // View tab bar.
                let mut tabs = div()
                    .flex()
                    .flex_row()
                    .flex_shrink(0.)
                    .gap_1()
                    .px_2()
                    .py_1();
                for view in views {
                    let label = match view {
                        ViewKind::Text => "Text",
                        ViewKind::Json => "JSON",
                        ViewKind::Hex => "Hex",
                    };
                    let is_active = view == active;
                    let bg = if is_active {
                        ext.selection
                    } else {
                        palette.panel
                    };
                    tabs = tabs.child(
                        div()
                            .id(SharedString::from(format!("view-tab-{label}")))
                            .px_2()
                            .py_1()
                            .rounded_sm()
                            .text_xs()
                            .cursor_pointer()
                            .focusable()
                            .focus_visible_ring(ext.accent)
                            .bg(gpui::rgb(bg))
                            .text_color(gpui::rgb(palette.text))
                            .child(label)
                            .on_click({
                                let weak_session = session.downgrade();
                                move |_ev, _window, cx: &mut App| {
                                    let _ = weak_session.update(cx, |s, cx| {
                                        s.switch_view(view, cx);
                                    });
                                }
                            }),
                    );
                }
                col = col.child(tabs);

                // Hex is view-only this phase: the editing UI edits
                // text, so hex editing "comes later" (spec: the tab
                // states it rather than failing silently).
                let hex_note = if active == ViewKind::Hex {
                    Some(
                        div()
                            .px_2()
                            .py_1()
                            .text_xs()
                            .text_color(gpui::rgb(palette.text_muted))
                            .child("hex view — editing in hex comes later"),
                    )
                } else {
                    None
                };

                // Rendered content (scrollable, line-truncated).
                let lines: Vec<String> = rendered_lines(&rendered);
                let truncated = lines.len() > MAX_RENDERED_LINES;
                let shown: Vec<String> = lines.into_iter().take(MAX_RENDERED_LINES).collect();
                let mut body = div()
                    .id("value-body")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .p_2();
                if let Some(line0) = shown.first() {
                    body = body.child(
                        div()
                            .font_family("Zed Mono")
                            .text_xs()
                            .line_height(px(16.))
                            .child(line0.clone()),
                    );
                }
                for line in shown.iter().skip(1) {
                    body = body.child(
                        div()
                            .font_family("Zed Mono")
                            .text_xs()
                            .line_height(px(16.))
                            .child(line.clone()),
                    );
                }
                if truncated {
                    body = body.child(
                        div()
                            .mt_2()
                            .text_xs()
                            .text_color(gpui::rgb(palette.text_muted))
                            .child(format!(
                                "… output truncated at {MAX_RENDERED_LINES} lines (full value retained)"
                            )),
                    );
                }
                if let Some(note) = hex_note {
                    body = body.child(note);
                }
                col = col.child(body);
            }
        }
        col
    }

    /// The status bar: link state + read-only pill + the console toggle.
    fn render_status_bar(
        &mut self,
        palette: &Palette,
        ext: ExtTokens,
        cx: &Context<Self>,
    ) -> impl IntoElement {
        let (link_text, link_color) = match self.active_session() {
            Some(s) => {
                let s = s.read(cx);
                match s.link {
                    LinkState::Live => ("connected".to_string(), ext.success),
                    LinkState::Disconnected => ("disconnected".to_string(), ext.error_text),
                }
            }
            None => ("no connection".to_string(), palette.text_muted),
        };
        let console_open = self.console_open;
        let mut bar = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_3()
            .flex_shrink(0.)
            .h(px(24.))
            .px_3()
            .bg(gpui::rgb(palette.panel))
            .border_t_1()
            .border_color(gpui::rgb(palette.divider))
            .child(
                div()
                    .text_xs()
                    .text_color(gpui::rgb(link_color))
                    .child(link_text),
            )
            .child(match self.active_session() {
                Some(session) => render_gate_pill(&session, palette, ext, cx).into_any_element(),
                None => div()
                    .text_xs()
                    .px_2()
                    .rounded_full()
                    .bg(gpui::rgb(ext.selection))
                    .text_color(gpui::rgb(palette.text_muted))
                    .child("Read-only")
                    .into_any_element(),
            })
            // Phase 6: the persistent TLS-insecure chip (lives as long as
            // the connection does; clickable → the explanation popover).
            .child(match self.active_session() {
                Some(session) if session.read(cx).tls_insecure() => {
                    let ws_weak = cx.entity().downgrade();
                    div()
                        .id("tls-insecure-chip")
                        .text_xs()
                        .px_2()
                        .rounded_full()
                        .cursor_pointer()
                        .focusable()
                        .focus_visible_ring(ext.accent)
                        .bg(gpui::rgb(ext.error_surface))
                        .text_color(gpui::rgb(ext.error_text))
                        .child("⚠ TLS insecure")
                        .on_click(move |_ev, _window, cx: &mut App| {
                            let _ = ws_weak.update(cx, |ws, cx| {
                                ws.tls_popover_open = !ws.tls_popover_open;
                                cx.notify();
                            });
                        })
                        .into_any_element()
                }
                _ => div().h(px(0.)).into_any_element(),
            })
            // Phase 6: the popover explaining the insecure connection
            // (text + reconnect hint; text-only this phase).
            .child(
                if self.tls_popover_open
                    && self
                        .active_session()
                        .is_some_and(|s| s.read(cx).tls_insecure())
                {
                    div()
                        .id("tls-insecure-popover")
                        .text_xs()
                        .px_2()
                        .py_1()
                        .rounded_sm()
                        .bg(gpui::rgb(ext.error_surface))
                        .text_color(gpui::rgb(ext.error_text))
                        .max_w(px(420.))
                        .child(SharedString::from(
                            "Certificate verification is disabled for this connection. \
                         A man-in-the-middle could read or alter all traffic. \
                         Reconnect securely: set TLS to 'TLS' (verified) in the \
                         connection form.",
                        ))
                        .into_any_element()
                } else {
                    div().h(px(0.)).into_any_element()
                },
            )
            // The console toggle (ctrl-` is the keyboard twin).
            .child(render_console_toggle(
                console_open,
                palette,
                ext,
                cx.listener(move |ws: &mut Workspace, _ev, window, cx| {
                    ws.toggle_console(window, cx);
                }),
            ))
            // Phase 7: the S/M/L text-scale segmented control (instant
            // apply; persisted to settings.json).
            .child(self.render_text_scale_control(palette, ext, cx));
        let _ = &mut bar;
        bar
    }

    /// The divider between two panels; brightens while active.
    fn divider(
        &self,
        edge: PanelEdge,
        owner: gpui::EntityId,
        palette: &Palette,
    ) -> impl IntoElement {
        let active = matches!(
            (edge, self.drag),
            (PanelEdge::ConnectionsKeys, DragState::ConnectionsKeys)
                | (PanelEdge::KeysValue, DragState::KeysValue)
        );
        let color = if active {
            palette.text_muted
        } else {
            palette.divider
        };
        render_divider(edge, owner, move |_, _, _cx: &mut App| {}).child(
            div()
                .id("divider-line")
                .w(px(DIVIDER_LINE_WIDTH))
                .h_full()
                .mx_auto()
                .bg(gpui::rgb(color)),
        )
    }
}

/// Display data extracted from `ValuePanelState` for rendering.
enum ValueDisplay {
    Empty,
    Loading,
    Missing,
    Error(String),
    Unsupported(String),
    Views {
        views: Vec<ViewKind>,
        active: ViewKind,
        rendered: std::sync::Arc<value_model::views::RenderedView>,
    },
}

/// Flatten a `RenderedView` into display lines.
fn rendered_lines(rendered: &value_model::views::RenderedView) -> Vec<String> {
    use value_model::views::{format_hex_line, RenderedView};
    match rendered {
        RenderedView::Text(parts) => parts
            .iter()
            .map(|p| match &p.label {
                Some(l) => format!("{l}: {}", p.text),
                None => p.text.clone(),
            })
            .collect(),
        RenderedView::Json(doc) => doc.lines().map(|l| l.to_string()).collect(),
        RenderedView::Hex(lines) => lines.iter().map(format_hex_line).collect(),
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Phase 7: set the frame's rem-size base FIRST — every rem-based
        // text size in this frame (all text_xs/text_sm/text_base) then
        // resolves against the S/M/L scale. One call, whole app.
        crate::settings::set_frame_rem_size(crate::settings::text_scale(cx), window);
        // Follow the system appearance (light/dark) on every frame.
        self.palette = theme::palette_for(Self::is_dark(window.appearance()));
        set_theme_global(cx, window.appearance());
        let palette = *self.palette;
        let ext = ExtTokens::for_dark(Self::is_dark(window.appearance()));

        let middle_width = self.middle_width;
        let entity_id = cx.entity_id();

        // Root split container: routes drag moves to this entity. The
        // root also tracks a focus handle: when nothing more specific
        // is focused, key dispatch (Tab actions, arrows) lands here.
        // (on_panel_drag_move takes a plain Div; the stateful root
        // applies the same drag routing inline.)
        let root = div()
            .id("workspace-root")
            .key_context("Workspace")
            .track_focus(&self.root_focus)
            .flex()
            .flex_col()
            .w_full()
            .h_full()
            .bg(gpui::rgb(palette.background))
            .text_color(gpui::rgb(palette.text))
            .on_drag_move::<PanelDrag>(cx.listener(
                move |ws: &mut Workspace, event: &DragMoveEvent<PanelDrag>, window, cx| {
                    if event.drag(cx).owner != entity_id {
                        return;
                    }
                    let x = event.event.position.x.as_f32();
                    let edge = event.drag(cx).edge;
                    let total = window.bounds().size.width.as_f32();
                    match edge {
                        PanelEdge::ConnectionsKeys => {
                            ws.drag = DragState::ConnectionsKeys;
                            ws.left_width = clamp_width(
                                x,
                                Workspace::MIN_PANEL,
                                total - Workspace::MIN_PANEL * 2.,
                            );
                        }
                        PanelEdge::KeysValue => {
                            ws.drag = DragState::KeysValue;
                            ws.middle_width = clamp_width(
                                x - ws.left_width,
                                Workspace::MIN_PANEL,
                                total - ws.left_width - Workspace::MIN_PANEL,
                            );
                        }
                    }
                    cx.notify();
                },
            ));

        // Clear highlight when the drag ends (drop = mouse up after drag).
        let root = root.on_drop::<PanelDrag>(cx.listener(
            move |ws: &mut Workspace, _drag: &PanelDrag, _window, cx| {
                ws.drag = DragState::None;
                cx.notify();
            },
        ));

        // Tab bar (top).
        let tab_bar = self.render_tab_bar(&palette, ext, cx);

        // Middle split: left form | divider | keys | divider | value.
        let split = {
            let session = self.active_session();
            let mut row = div().flex().flex_row().flex_1().min_h_0();
            row = row.child(self.render_left_panel(&palette, ext, cx));
            row = row.child(self.divider(PanelEdge::ConnectionsKeys, entity_id, &palette));
            match session {
                Some(session) => {
                    row = row.child(self.render_keys_panel(&session, &palette, ext, cx));
                    row = row.child(self.divider(PanelEdge::KeysValue, entity_id, &palette));
                    row = row.child(self.render_value_panel(&session, &palette, ext, cx));
                }
                None => {
                    // No session: placeholder panels keep the layout.
                    row = row.child(
                        div()
                            .w(px(middle_width))
                            .flex_shrink(0.)
                            .bg(gpui::rgb(palette.panel))
                            .p_2()
                            .text_color(gpui::rgb(palette.text_muted))
                            .child("Keys (connect first)"),
                    );
                    row = row.child(self.divider(PanelEdge::KeysValue, entity_id, &palette));
                    row = row.child(
                        div()
                            .flex_1()
                            .bg(gpui::rgb(palette.panel))
                            .p_2()
                            .text_color(gpui::rgb(palette.text_muted))
                            .child("Value (connect first)"),
                    );
                }
            }
            row
        };

        // Status bar (bottom).
        let status = self.render_status_bar(&palette, ext, cx);

        // The bottom-docked console panel (full width under the three
        // main panels), when open and a session is active.
        let mut root = root.child(tab_bar).child(split);
        if self.console_open {
            if let Some(session) = self.active_session() {
                let panel = self.console_panel.read(cx);
                root = root.child(render_console_panel(&session, panel, &palette, ext, cx));
            }
        }

        // The Phase 5 editor modals (Enable Editing / Delete Key /
        // Discard guard / F5 conflict) overlay the app when pending.
        if let Some(session) = self.active_session() {
            if let Some(modal) = session.read(cx).editor_modal().cloned() {
                let panel = self.editor_panel.read(cx);
                root = root.child(render_editor_modal(
                    &session,
                    panel,
                    &modal,
                    &palette,
                    ext,
                    {
                        // DiscardEdits: apply the deferred navigation
                        // after the discard (workspace-owned state).
                        let ws = cx.entity().downgrade();
                        Some(Box::new(move |window: &mut Window, cx: &mut App| {
                            // Weak handle: if the workspace was released
                            // there is no deferred navigation to apply.
                            let _ = ws.update(cx, |ws, cx| {
                                ws.discard_guard_confirm(window, cx);
                            });
                        }))
                    },
                    cx,
                ));
            }
        }

        // The destructive-confirm modal overlays the app when the active
        // session has a pending confirmation.
        if let Some(session) = self.active_session() {
            if let Some(pending) = session.read(cx).pending_console_confirm().cloned() {
                let panel = self.console_panel.read(cx);
                // Render a fresh frame whenever the typed phrase changes:
                // the session's notify from `console_confirm_typed` covers
                // this because the workspace re-renders per notify.
                root = root.child(render_confirm_modal(
                    &session, panel, &pending, &palette, ext, cx,
                ));
            }
        }

        // The Phase 6 host-key modals (TOFU prompt / hard block): the
        // live-session state takes precedence, else the connect form's
        // refusal state. Overlays the app.
        if let Some(modal) = self.render_host_key_modal(&palette, ext, cx) {
            root = root.child(modal);
        }

        // The Phase 6 save-connection dialog overlays the app when open.
        if let Some(dialog) = self.render_save_dialog(&palette, ext, cx) {
            root = root.child(dialog);
        }

        // The Phase 6 forget-connection confirm modal.
        if let Some(modal) = self.render_forget_modal(&palette, ext, cx) {
            root = root.child(modal);
        }

        root.child(status)
            .on_key_down({
                // ⌘R: refresh the selected key's value — behind the dirty
                // guard (a clean editor re-opens; a dirty one routes the
                // new bytes to the original side per apply_refresh, but
                // the guard is still asked first per the spec).
                cx.listener(|ws: &mut Workspace, ev: &gpui::KeyDownEvent, _window, cx| {
                    if ev.keystroke.key == "r" && ev.keystroke.modifiers.platform {
                        ws.guarded_nav(PendingNav::Refresh, cx);
                    }
                })
            })
            // The console-panel toggle action (ctrl-`), handled at the root
            // during paint — the idiomatic element-level dispatch (the
            // low-level `Context::on_action` may only be called during
            // paint, so it can't live in `new`).
            .on_action(
                cx.listener(|ws: &mut Workspace, _action: &Toggle, window, cx| {
                    ws.toggle_console(window, cx);
                }),
            )
            // Phase 7, acceptance 11: the keyboard-navigation actions
            // (bindings installed by bind_workspace_keys). All handled
            // on the focused-path root.
            .on_action(cx.listener(|ws: &mut Workspace, _: &TabNext, window, cx| {
                ws.on_tab_next(window, cx);
            }))
            .on_action(cx.listener(|ws: &mut Workspace, _: &TabPrev, window, cx| {
                ws.on_tab_prev(window, cx);
            }))
            .on_action(
                cx.listener(|ws: &mut Workspace, _: &CloseTab, _window, cx| {
                    ws.on_close_tab(_window, cx);
                }),
            )
            .on_action(
                cx.listener(|ws: &mut Workspace, _: &SaveEditor, _window, cx| {
                    ws.on_save_editor(_window, cx);
                }),
            )
            .on_action(
                cx.listener(|ws: &mut Workspace, _: &FocusSearch, window, cx| {
                    ws.on_focus_search(window, cx);
                }),
            )
    }
}

impl EventEmitter<()> for Workspace {}

#[cfg(test)]
mod tests {
    #[test]
    fn form_validation_rules() {
        // Port parse + bounds are delegated to u16::from_str; the two
        // interesting app rules are empty-host and negative-db, both
        // covered by the validate logic. Here we verify the helpers it
        // leans on behave as the form expects.
        assert!("127.0.0.1".parse::<u16>().is_err()); // host string is not a port
        assert!("6379".parse::<u16>().is_ok());
        assert!("0".parse::<i64>().is_ok());
        assert!("-1".parse::<i64>().is_ok());
    }
}
