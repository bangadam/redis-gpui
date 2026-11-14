//! The console panel UI (PLAN.md Phase 4; the docked panel of the approved
//! design, frame 02 + the frame-03 confirm modal).
//!
//! **This module renders; it never decides or executes.** Every flow goes
//! through `ConnectionSession`'s `console_*` methods (see `session.rs`);
//! the panel is a projection of `ConsoleState` plus input plumbing:
//!
//! - **Docked panel** — bottom, full width under the three main panels,
//!   collapsible (toggle in the status bar + the `ctrl-\`` shortcut, the
//!   same key Zed binds to `terminal_panel::Toggle` at our pinned
//!   revision — a familiar idiom for a bottom terminal-like panel).
//!   Dark surface from the palette, output log monospace-flavored and
//!   colored by kind (success/error/info), auto-scrolled to the newest
//!   entry, input line at the bottom with the `›` prompt glyph.
//! - **History nav** — Up/Down in the input walk the per-connection
//!   in-memory history (down past the newest restores the draft).
//! - **Confirm modal** — centered over a scrim: title in the error color
//!   with a warning glyph (the command name), the write-safety
//!   scope_description as the body, a typed-phrase input (must match
//!   case-insensitively), a danger Confirm button that stays desaturated
//!   until the phrase matches, and an always-active Cancel. Declining
//!   means nothing is sent.

use gpui::{
    actions, div, prelude::*, px, App, FocusHandle, Focusable, KeyBinding, ScrollHandle, Window,
};

use theme::Palette;

use crate::console::LogEntry;
use crate::focus_ring::FocusRingExt;
use crate::palette_ext::ExtTokens;
use crate::session::ConnectionSession;
use crate::text_input::SingleLineInput;

// Toggle the console panel (bound to `ctrl-\`` — Zed's own
// `terminal_panel::Toggle` binding at the pinned revision, so the idiom
// is familiar; also clickable in the status bar).
actions!(console_panel, [Toggle]);

/// Bind the console-panel keys (call once at app start).
pub fn bind_console_keys(cx: &mut App) {
    cx.bind_keys([KeyBinding::new("ctrl-`", Toggle, None)]);
}

/// The workspace-owned console panel view.
///
/// One instance per workspace (like the search box). It holds only UI
/// state — the input entities and the scroll handle; all command and
/// decision state lives in the session entity, and every action here
/// delegates to it.
pub struct ConsolePanel {
    /// The console input line (the `›` prompt row).
    pub(crate) input: gpui::Entity<SingleLineInput>,
    /// The confirm modal's typed-phrase input.
    pub(crate) confirm_input: gpui::Entity<SingleLineInput>,
    /// Scroll state of the output log (auto-scroll to bottom on new
    /// output).
    log_scroll: ScrollHandle,
    /// The panel's own focus scope, so the toggle can focus the input.
    focus_handle: FocusHandle,
}

impl ConsolePanel {
    /// Create the panel (placeholder inputs; the workspace routes their
    /// events to the active session).
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            input: cx.new(|cx| SingleLineInput::new("type a Redis command…", "", cx)),
            confirm_input: cx
                .new(|cx| SingleLineInput::new("type the command name to confirm", "", cx)),
            log_scroll: ScrollHandle::new(),
            focus_handle: cx.focus_handle(),
        }
    }

    /// The console input entity (for the workspace's event subscription).
    pub fn input(&self) -> &gpui::Entity<SingleLineInput> {
        &self.input
    }

    /// The confirm-modal input entity (for the workspace's event
    /// subscription).
    pub fn confirm_input(&self) -> &gpui::Entity<SingleLineInput> {
        &self.confirm_input
    }
}

impl Focusable for ConsolePanel {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ConsolePanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // The panel's own render is a pass-through container; the
        // workspace composes the real content (log + input + modal) in
        // `render_console_panel` below, because it owns the session and
        // the toggle state. This impl keeps ConsolePanel a proper
        // focusable view for the shortcut.
        div().size_full()
    }
}

/// True when the extended tokens are the dark set (a cheap dark-mode
/// check local to the console rendering, mirroring how `ExtTokens` is
/// picked per-appearance).
fn is_dark_ext(ext: ExtTokens) -> bool {
    ext.accent == ExtTokens::DARK.accent
}

/// The docked console panel content, rendered by the workspace.
///
/// `session` is the active connection; the log/running/pending state is
/// read from its `ConsoleState` here (display data only).
pub fn render_console_panel(
    session: &gpui::Entity<ConnectionSession>,
    panel: &ConsolePanel,
    palette: &Palette,
    ext: ExtTokens,
    cx: &App,
) -> impl IntoElement {
    let log: Vec<LogEntry> = session.read(cx).console.log().to_vec();
    let running = session.read(cx).console_running();

    // The console surface: slightly darker than panels, per the design.
    let console_bg = if is_dark_ext(ext) { 0x101216 } else { 0xECEEF2 };

    let mut col = div()
        .flex()
        .flex_col()
        .flex_shrink(0.)
        .h(px(200.))
        .bg(gpui::rgb(console_bg))
        .border_t_1()
        .border_color(gpui::rgb(palette.divider))
        .text_sm();

    // Header strip: "Console" + the running state + Stop.
    {
        let mut header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .flex_shrink(0.)
            .px_3()
            .h(px(24.))
            .border_b_1()
            .border_color(gpui::rgb(palette.divider))
            .child(
                div()
                    .text_xs()
                    .text_color(gpui::rgb(palette.text_muted))
                    .child("Console"),
            );
        if running {
            let session_stop = session.clone();
            header = header
                .child(
                    div()
                        .text_xs()
                        .text_color(gpui::rgb(palette.text_muted))
                        .child("running…"),
                )
                .child(
                    div()
                        .id("console-stop-btn")
                        .px_2()
                        .rounded_sm()
                        .text_xs()
                        .cursor_pointer()
                        .focusable()
                        .focus_visible_ring(ext.accent)
                        .bg(gpui::rgb(ext.error_surface))
                        .text_color(gpui::rgb(ext.error_text))
                        .child("Stop")
                        .on_click(move |_ev, _window, cx: &mut App| {
                            session_stop.update(cx, |s, cx| s.console_stop(cx));
                        }),
                );
        }
        col = col.child(header);
    }

    // Output log: monospace-flavored, colored by kind, auto-scrolled.
    {
        // Auto-scroll to the newest entry whenever the log grows. The pin
        // is re-issued per render, matching the "auto-scroll on new
        // output" spec; a manual scroll away from the bottom simply
        // resumes pinning on the next append.
        panel.log_scroll.scroll_to_bottom();
        let mut body = div()
            .id("console-log")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&panel.log_scroll)
            .px_3()
            .py_1();
        for (ix, entry) in log.iter().enumerate() {
            let color = match entry.kind {
                console_engine::OutputKind::Success => ext.success,
                console_engine::OutputKind::Error => ext.error_text,
                console_engine::OutputKind::Info => palette.text_muted,
            };
            body = body.child(
                div()
                    .id(("console-log-line", ix))
                    .font_family("Zed Mono")
                    .text_xs()
                    .line_height(px(16.))
                    .text_color(gpui::rgb(color))
                    .child(entry.text.clone()),
            );
        }
        col = col.child(body);
    }

    // Input line: `›` glyph + the input entity. Enter submits; Up/Down
    // walk history (the workspace attaches the key handler to this
    // container; the SingleLineInput binds editing keys only).
    {
        let session = session.clone();
        let input_entity = panel.input.clone();
        let mut row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .flex_shrink(0.)
            .px_3()
            .py_1()
            .border_t_1()
            .border_color(gpui::rgb(palette.divider))
            .child(
                div()
                    .font_family("Zed Mono")
                    .text_xs()
                    .text_color(gpui::rgb(palette.text_muted))
                    .child("›"),
            )
            .child(div().flex_1().min_w_0().child(input_entity.clone()));
        row = row.on_key_down(move |ev: &gpui::KeyDownEvent, window, cx: &mut App| {
            handle_input_key(&ev.keystroke, &session, &input_entity, window, cx);
        });
        col = col.child(row);
    }

    col
}

/// The destructive-confirm modal (approved design, frame 03), centered
/// over a dark scrim overlaying the app.
///
/// `pending` carries the title/body/phrase state from the session; the
/// Confirm button stays desaturated (visibly disabled) until the typed
/// phrase matches case-insensitively. Cancel is a ghost button
/// (transparent, light border), always active, left of Confirm.
/// Declining = no-op: nothing is sent.
pub fn render_confirm_modal(
    session: &gpui::Entity<ConnectionSession>,
    panel: &ConsolePanel,
    pending: &crate::console::PendingConfirm,
    palette: &Palette,
    ext: ExtTokens,
    _cx: &App,
) -> impl IntoElement {
    let dark = is_dark_ext(ext);
    let phrase_matches = pending.phrase_matches();
    let palette = *palette;

    // Cancel (ghost, always active): decline — nothing is sent, the modal
    // closes, the honest decline line lands in the log.
    let session_cancel = session.clone();
    let confirm_input_entity = panel.confirm_input.clone();
    let cancel_btn = crate::modal::modal_button(
        "confirm-cancel-btn",
        &palette,
        ext,
        crate::modal::ModalButtonKind::Ghost,
    )
    .child("Cancel")
    .on_click(move |_ev, window, cx: &mut App| {
        session_cancel.update(cx, |s, cx| s.console_decline(cx));
        confirm_input_entity.update(cx, |input, cx| {
            input.set_content("", window, cx);
        });
    });

    // Confirm (danger): the error-color bg with white text once the
    // phrase matches; desaturated (and the click guarded) until then.
    // GPUI has no true `disabled` state on a div at this revision, so
    // desaturation + the in-session WrongPhrase guard keep it honest.
    let session_confirm = session.clone();
    let confirm_btn = if phrase_matches {
        crate::modal::modal_button(
            "confirm-ok-btn",
            &palette,
            ext,
            crate::modal::ModalButtonKind::Danger,
        )
        .child("Confirm")
        .on_click(move |_ev, _window, cx: &mut App| {
            session_confirm.update(cx, |s, cx| s.console_confirm(cx));
        })
    } else {
        // Desaturated twin (same geometry, muted bg; absorbs clicks —
        // the session's WrongPhrase guard is the second line of defense).
        div()
            .id("confirm-ok-btn")
            .h(px(30.))
            .px_3()
            .flex()
            .items_center()
            .rounded_sm()
            .cursor_default()
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .bg(gpui::rgb(if dark { 0x4A3844 } else { 0xD8C2C6 }))
            .text_color(gpui::rgb(0xFFFFFF))
            .child("Confirm")
            .on_click(move |_ev, _window, _cx: &mut App| {
                // Desaturated = disabled: absorb the click.
            })
    };

    // Escape declines; Enter confirms only when the phrase matches
    // (the session guard is authoritative).
    let session_escape = session.clone();
    let session_enter = session.clone();
    let title = pending.title().to_string();
    let body = pending.body().to_string();
    let confirm_input = panel.confirm_input.clone();
    crate::modal::modal_scaffold(
        &palette,
        crate::modal::ModalKeys {
            dark,
            width: crate::modal::MODAL_WIDTH_WIDE,
            card_focus: None,
            armed: crate::modal::enter_armed(phrase_matches),
            escape: Box::new(move |_window, cx: &mut App| {
                session_escape.update(cx, |s, cx| s.console_decline(cx));
                // The workspace's Escape subscription clears the phrase input.
            }),
            enter: Box::new(move |_window, cx: &mut App| {
                session_enter.update(cx, |s, cx| s.console_confirm(cx));
            }),
        },
        move |card| {
            let error_text = ext.error_text;
            let text = palette.text;
            card
                // Title: the command name in the error/danger color + warning glyph.
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .text_base()
                                .text_color(gpui::rgb(error_text))
                                .child("\u{26A0}"), // ⚠
                        )
                        .child(
                            div()
                                .text_base()
                                .font_weight(gpui::FontWeight::BOLD)
                                .text_color(gpui::rgb(error_text))
                                .child(title),
                        ),
                )
                // Body: plain light text — the write-safety scope description.
                .child(div().text_sm().text_color(gpui::rgb(text)).child(body))
                // The typed-phrase input.
                .child(div().flex_1().child(confirm_input))
                // Button row: Cancel left, Confirm right.
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .justify_end()
                        .gap_2()
                        .child(cancel_btn)
                        .child(confirm_btn),
                )
        },
    )
}

/// The status-bar console toggle button (click handled by the caller's
/// own closure — it owns the toggle state).
pub fn render_console_toggle(
    active: bool,
    palette: &Palette,
    ext: ExtTokens,
    on_toggle: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
) -> impl IntoElement {
    let label = if active { "Console ▾" } else { "Console ▴" };
    let bg = if active { ext.selection } else { palette.panel };
    let color = if active {
        palette.text
    } else {
        palette.text_muted
    };
    div()
        .id("console-toggle-btn")
        .text_xs()
        .px_2()
        .rounded_full()
        .cursor_pointer()
        .focusable()
        .focus_visible_ring(ext.accent)
        .bg(gpui::rgb(bg))
        .text_color(gpui::rgb(color))
        .child(label)
        .on_click(move |ev, window, cx| on_toggle(ev, window, cx))
}

/// Console key handling for the input row: Enter submits, Up/Down walk
/// history. Called from the input container's `on_key_down` (the
/// `SingleLineInput` binds editing keys only).
fn handle_input_key(
    keystroke: &gpui::Keystroke,
    session: &gpui::Entity<ConnectionSession>,
    input: &gpui::Entity<SingleLineInput>,
    window: &mut Window,
    cx: &mut App,
) {
    match keystroke.key.as_str() {
        // "enter" is handled by the console input's EnterPressed
        // subscription in `Workspace::new`: the input's own binding
        // shadows this listener (GPUI stops action propagation during
        // the bubble phase), so an arm here would never run.
        "up" => {
            let current = input.read(cx).content().to_string();
            session.update(cx, |s, cx| {
                if let Some(text) = s.console_history_up(&current) {
                    input.update(cx, |i, cx| i.set_content(&text, window, cx));
                }
            });
        }
        "down" => {
            session.update(cx, |s, cx| {
                if let Some(text) = s.console_history_down() {
                    input.update(cx, |i, cx| i.set_content(&text, window, cx));
                }
            });
        }
        _ => {}
    }
}
