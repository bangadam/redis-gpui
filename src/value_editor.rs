//! The Phase 5 value-editor UI: edit affordances, the save-flow banners,
//! TTL CRUD, and the delete-key button (PLAN.md section 5, PRD stories
//! 26–31; the discipline contract is `app-workflows`' `SaveWorkflow`).
//!
//! **This module renders; it never decides or executes.** Every flow
//! goes through `ConnectionSession`'s Phase 5 methods (`save_value`,
//! `set_ttl`, `confirm_delete_key`, `toggle_editing`, …); the value
//! panel is a projection of `EditorState` + `EditorFlow` + the gate:
//!
//! - **Edit affordances** appear only when the gate is `WriteEnabled`:
//!   per-field inline editing for hash/list/set/zset members (the
//!   shared field input), a plain-text editor for strings/JSON when the
//!   view is Text and the bytes are valid UTF-8 (hex editing is out of
//!   scope this phase — the Hex tab shows the "comes later" note).
//! - **Save/Discard** appear only when dirty. Save = the session's
//!   `save_value` (gate check → precheck → issue once → tri-state).
//! - **OutcomeUnknown banner** (acceptance 3): warning tone, "check
//!   before retrying" text, [Check now] [Retry save] — retry only after
//!   a reconnect, and it is a user action, never automatic.
//! - **Conflict modal** (F5): server changed the value since load —
//!   [Show current] [Overwrite anyway] [Cancel].
//! - **Dirty guards** (acceptance 9): the workspace consults
//!   `editor_dirty` before key/tab/DB switches and refreshes route
//!   through `apply_refresh` (the working buffer survives).
//!
//! Modals reuse the console's frame-03 pattern (scrim, centered card,
//! danger-tinted title, desaturated-until-armed confirm). No phrase gate
//! for Enable Editing or Delete Key (neither is destructive-class per
//! D3: single-key DEL is `Write`; the modal names what's lost).

use gpui::{div, prelude::*, px, App, SharedString, Window};

use theme::Palette;

use crate::focus_ring::FocusRingExt;
use crate::palette_ext::ExtTokens;
use crate::session::{ConnectionSession, EditorFlow, EditorModal};
use crate::text_input::SingleLineInput;

/// The value-panel editing state the workspace owns (UI-only state; the
/// editor truth lives in the session entity).
pub struct ValueEditorPanel {
    /// The shared inline field input (hash values, list items, set
    /// members, zset members/scores). Seeded on `begin_field_edit`;
    /// keystrokes route to the session's `field_edit_text`.
    pub(crate) field_input: gpui::Entity<SingleLineInput>,
    /// The string/JSON multi-line text editor body input (a
    /// single-line input used as the text editor for this phase; Enter
    /// inserts through the session's edit path).
    pub(crate) scalar_input: gpui::Entity<SingleLineInput>,
    /// The TTL input (seconds; empty = PERSIST).
    pub(crate) ttl_input: gpui::Entity<SingleLineInput>,
}

impl ValueEditorPanel {
    /// Create the panel's inputs.
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            field_input: cx.new(|cx| SingleLineInput::new("edit field…", "", cx)),
            scalar_input: cx.new(|cx| SingleLineInput::new("edit value…", "", cx)),
            ttl_input: cx.new(|cx| SingleLineInput::new("seconds", "", cx)),
        }
    }

    /// The inline field input entity (for the workspace's subscription).
    pub fn field_input(&self) -> &gpui::Entity<SingleLineInput> {
        &self.field_input
    }

    /// The scalar text editor input entity.
    pub fn scalar_input(&self) -> &gpui::Entity<SingleLineInput> {
        &self.scalar_input
    }
}

impl gpui::EventEmitter<()> for ValueEditorPanel {}

impl gpui::Focusable for ValueEditorPanel {
    fn focus_handle(&self, _cx: &App) -> gpui::FocusHandle {
        self.field_input.read(_cx).focus_handle(_cx).clone()
    }
}

impl gpui::Render for ValueEditorPanel {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        // Pass-through container: the workspace composes the real content
        // (it owns the session and the layout), same pattern as
        // ConsolePanel.
        div().size_full()
    }
}

/// The status-bar gate pill: Read-only (clickable → Enable Editing
/// modal) or ✏️ Editing (clickable → disable, instant).
///
/// Rendered by the workspace's status bar; the click routes to the
/// session's `toggle_editing`.
pub fn render_gate_pill(
    session: &gpui::Entity<ConnectionSession>,
    palette: &Palette,
    ext: ExtTokens,
    cx: &App,
) -> impl IntoElement {
    let read_only = session.read(cx).gate_state() == write_safety::GateState::ReadOnly;
    let live = session.read(cx).link == crate::session::LinkState::Live;
    let (label, bg, text_color) = if read_only {
        ("🔓 Read-only", ext.selection, palette.text)
    } else {
        ("✏️ Editing", ext.accent, 0xFFFFFF)
    };
    let session_click = session.clone();
    div()
        .id("gate-pill")
        .text_xs()
        .px_2()
        .rounded_full()
        .cursor_pointer()
        .focusable()
        .focus_visible_ring(ext.accent)
        .bg(gpui::rgb(if live { bg } else { ext.selection }))
        .text_color(gpui::rgb(if live {
            text_color
        } else {
            palette.text_muted
        }))
        .child(label)
        .on_click(move |_ev, _window, cx: &mut App| {
            if live {
                session_click.update(cx, |s, cx| s.toggle_editing(cx));
            }
        })
}

/// The edit toolbar + banners rendered under the value panel's view tabs
/// (only when a value is loaded). Returns the column of editor chrome;
/// the caller appends the normal rendered view below it.
pub fn render_editor_chrome(
    session: &gpui::Entity<ConnectionSession>,
    panel: &ValueEditorPanel,
    palette: &Palette,
    ext: ExtTokens,
    cx: &App,
) -> impl IntoElement {
    let s = session.read(cx);
    let write_enabled = s.gate_state() == write_safety::GateState::WriteEnabled;
    let flow = s.editor_flow().clone();
    let dirty = s.editor_dirty();
    let diverged = s.editor_diverged();
    let editing_field = s.editing_field().cloned();
    let has_editor = s.editor.is_some();

    let mut col = div().flex().flex_col().flex_shrink(0.);

    // ---- The save-flow banners (state-driven) ------------------------
    match &flow {
        EditorFlow::OutcomeUnknown => {
            col = col.child(render_outcome_unknown_banner(session, palette, ext, cx));
        }
        EditorFlow::Failed(msg) => {
            col = col.child(render_banner(
                "⚠",
                &format!(
                    "Save failed — {msg}. Your edits are kept; retry \
                     when you choose."
                ),
                ext.error_surface,
                ext.error_text,
            ));
        }
        EditorFlow::Refused(msg) => {
            col = col.child(render_banner("⚠", msg, ext.error_surface, ext.error_text));
        }
        EditorFlow::Saving => {
            col = col.child(render_banner(
                "",
                "Saving…",
                ext.selection,
                palette.text_muted,
            ));
        }
        _ => {}
    }

    // ---- Divergence note (acceptance 9) --------------------------------
    if diverged {
        col = col.child(render_banner(
            "",
            "server value changed — reload to see it (your edits are kept)",
            palette.panel,
            palette.text_muted,
        ));
    }

    // ---- The edit toolbar (only when editing is enabled) ---------------
    if !write_enabled || !has_editor {
        if dirty {
            // A dirty editor with the gate back to read-only (disconnect
            // or disable): honest note, edits preserved.
            col = col.child(render_banner(
                "🔒",
                "editing is read-only now — your unsaved edits are kept",
                palette.panel,
                palette.text_muted,
            ));
        }
        return col.child(div().h(px(0.)));
    }

    let mut toolbar = div()
        .flex()
        .flex_row()
        .flex_wrap()
        .items_center()
        .gap_2()
        .px_2()
        .py_1()
        .flex_shrink(0.);

    // Dirty indicator + Save/Discard (only when dirty).
    if dirty {
        toolbar = toolbar
            .child(
                div()
                    .text_xs()
                    .text_color(gpui::rgb(ext.accent))
                    .child("● edited"),
            )
            .child({
                let session_save = session.clone();
                div()
                    .id("editor-save-btn")
                    .h(px(24.))
                    .px_2()
                    .flex()
                    .items_center()
                    .rounded_sm()
                    .cursor_pointer()
                    .focusable()
                    .focus_visible_ring(ext.accent)
                    .bg(gpui::rgb(ext.accent))
                    .text_color(gpui::rgb(0xFFFFFF))
                    .text_xs()
                    .child("Save")
                    .on_click(move |_ev, _window, cx: &mut App| {
                        session_save.update(cx, |s, cx| s.save_value(cx));
                    })
            })
            .child({
                let session_discard = session.clone();
                div()
                    .id("editor-discard-btn")
                    .h(px(24.))
                    .px_2()
                    .flex()
                    .items_center()
                    .rounded_sm()
                    .cursor_pointer()
                    .focusable()
                    .focus_visible_ring(ext.accent)
                    .border_1()
                    .border_color(gpui::rgb(palette.divider))
                    .text_color(gpui::rgb(palette.text))
                    .text_xs()
                    .child("Discard")
                    .on_click(move |_ev, _window, cx: &mut App| {
                        session_discard.update(cx, |s, cx| s.discard_edits(cx));
                    })
            });
    }

    // Delete key (danger outline) — opens the naming modal.
    toolbar = toolbar.child({
        let session_delete = session.clone();
        let label = "Delete key".to_string();
        div()
            .id("editor-delete-key-btn")
            .h(px(24.))
            .px_2()
            .flex()
            .items_center()
            .rounded_sm()
            .cursor_pointer()
            .focusable()
            .focus_visible_ring(ext.accent)
            .border_1()
            .border_color(gpui::rgb(ext.error_text))
            .text_color(gpui::rgb(ext.error_text))
            .text_xs()
            .child(label)
            .on_click(move |_ev, _window, cx: &mut App| {
                session_delete.update(cx, |s, cx| s.request_delete_key(cx));
            })
    });

    col = col.child(toolbar);

    // ---- The inline field editor row (hash/list/set/zset fields) ------
    if let Some(path) = editing_field {
        let session_field = session.clone();
        let field_label = field_path_label(&path);
        col = col.child(
            div()
                .id("editor-field-row")
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .px_2()
                .py_1()
                .flex_shrink(0.)
                .child(
                    div()
                        .text_xs()
                        .text_color(gpui::rgb(palette.text_muted))
                        .child(field_label),
                )
                .child(div().flex_1().min_w_0().child(panel.field_input.clone()))
                .child({
                    let session_done = session_field.clone();
                    div()
                        .id("editor-field-done-btn")
                        .text_xs()
                        .px_2()
                        .rounded_sm()
                        .cursor_pointer()
                        .focusable()
                        .focus_visible_ring(ext.accent)
                        .border_1()
                        .border_color(gpui::rgb(palette.divider))
                        .text_color(gpui::rgb(palette.text))
                        .child("Done")
                        .on_click(move |_ev, _window, cx: &mut App| {
                            session_done.update(cx, |s, cx| s.end_field_edit(cx));
                        })
                }),
        );
    }

    // ---- The editable fields list (hash/list/set/zset) -----------------
    // One row per field with an Edit button; clicking it seeds the shared
    // field input (via begin_field_edit) and shows the field editor row.
    if write_enabled {
        if let Some(editor) = session.read(cx).editor.as_ref() {
            let fields = editable_fields(editor);
            if !fields.is_empty() {
                let mut rows = div()
                    .id("editor-fields-list")
                    .flex()
                    .flex_col()
                    .flex_shrink(0.)
                    .px_2()
                    .py_1()
                    .max_h(px(180.))
                    .overflow_y_scroll();
                for (ix, (label, current, path)) in fields.iter().enumerate() {
                    let session_edit = session.clone();
                    let field_input = panel.field_input.clone();
                    rows = rows.child(
                        div()
                            .id(("editor-field", ix))
                            .flex()
                            .flex_row()
                            .items_center()
                            .gap_2()
                            .py_0p5()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .overflow_hidden()
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .text_xs()
                                    .text_color(gpui::rgb(palette.text))
                                    .child(format!("{label}: {current}")),
                            )
                            .child(
                                div()
                                    .id(("editor-field-edit-btn", ix))
                                    .text_xs()
                                    .px_2()
                                    .rounded_sm()
                                    .cursor_pointer()
                                    .focusable()
                                    .focus_visible_ring(ext.accent)
                                    .border_1()
                                    .border_color(gpui::rgb(palette.divider))
                                    .text_color(gpui::rgb(palette.text))
                                    .child("Edit")
                                    .on_click({
                                        let weak_input = field_input.clone();
                                        let path = path.clone();
                                        move |_ev, window, cx: &mut App| {
                                            let initial = session_edit.update(cx, |s, _cx| {
                                                s.begin_field_edit(path.clone())
                                            });
                                            if let Some(text) = initial {
                                                weak_input.update(cx, |i, cx| {
                                                    i.set_content(&text, window, cx);
                                                });
                                            }
                                        }
                                    }),
                            ),
                    );
                }
                col = col.child(rows);
            }
        }
    }

    // ---- The scalar text editor (strings/JSON, Text view) --------------
    // The multi-line-ish editor: the shared scalar input when the value
    // is a valid-UTF-8 string/JSON (hex editing is out of scope this
    // phase — the Hex tab shows the note).
    if write_enabled {
        if let Some(editor) = session.read(cx).editor.as_ref() {
            let is_scalar = matches!(
                editor.original,
                value_model::raw::RawValue::String(_) | value_model::raw::RawValue::Json(_)
            );
            let valid_utf8 = editor.original.is_valid_utf8();
            if is_scalar && valid_utf8 {
                let current = working_scalar_text(editor);
                let scalar_input = panel.scalar_input.clone();
                let session_scalar = session.clone();
                col = col.child(
                    div()
                        .id("editor-scalar-row")
                        .flex()
                        .flex_col()
                        .gap_1()
                        .px_2()
                        .py_1()
                        .flex_shrink(0.)
                        .child(
                            div()
                                .text_xs()
                                .text_color(gpui::rgb(palette.text_muted))
                                .child("value (text)"),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap_2()
                                .child(div().flex_1().min_w_0().child(scalar_input.clone()))
                                .child(
                                    div()
                                        .id("editor-scalar-seed-btn")
                                        .text_xs()
                                        .px_2()
                                        .rounded_sm()
                                        .cursor_pointer()
                                        .focusable()
                                        .focus_visible_ring(ext.accent)
                                        .border_1()
                                        .border_color(gpui::rgb(palette.divider))
                                        .text_color(gpui::rgb(palette.text))
                                        .child("Load current")
                                        .on_click(move |ev, window, cx: &mut App| {
                                            let _ = ev;
                                            let _ = &session_scalar;
                                            scalar_input.update(cx, |i, cx| {
                                                i.set_content(&current, window, cx);
                                            });
                                        }),
                                ),
                        ),
                );
            }
        }
    }

    // ---- The conflict-model disclosure (containers) --------------------
    // value-model's honesty: types without CAS carry the disclosure line.
    if let Some(editor) = session.read(cx).editor.as_ref() {
        use value_model::conflict::ConflictModel;
        let model = ConflictModel::for_type(&editor.original.kind());
        if let Some(text) = model.disclosure_text() {
            col = col.child(
                div()
                    .px_2()
                    .py_1()
                    .text_xs()
                    .flex_shrink(0.)
                    .text_color(gpui::rgb(palette.text_muted))
                    .child(text.to_string()),
            );
        }
    }

    col
}

/// The OutcomeUnknown banner (acceptance 3): the dedicated
/// "check before retrying" state. [Check now] re-reads the key
/// (read-only); [Retry save] is user-explicit.
fn render_outcome_unknown_banner(
    session: &gpui::Entity<ConnectionSession>,
    palette: &Palette,
    ext: ExtTokens,
    cx: &App,
) -> impl IntoElement {
    let session_check = session.clone();
    let session_retry = session.clone();
    let live = session.read(cx).link == crate::session::LinkState::Live;
    div()
        .id("editor-outcome-unknown-banner")
        .flex()
        .flex_col()
        .gap_1()
        .m_2()
        .p_2()
        .rounded_sm()
        .flex_shrink(0.)
        .bg(gpui::rgb(ext.error_surface))
        .border_1()
        .border_color(gpui::rgb(ext.error_text))
        .child(
            div()
                .flex()
                .flex_row()
                .gap_2()
                .text_sm()
                .text_color(gpui::rgb(ext.error_text))
                .child("⚠")
                .child(div().text_color(gpui::rgb(palette.text)).child(
                    "Save result unknown — the connection dropped after sending. \
                                Check the current value before retrying; nothing was re-sent.",
                )),
        )
        .child(
            div()
                .flex()
                .flex_row()
                .gap_2()
                .child(
                    div()
                        .id("editor-check-btn")
                        .h(px(24.))
                        .px_2()
                        .flex()
                        .items_center()
                        .rounded_sm()
                        .cursor_pointer()
                        .focusable()
                        .focus_visible_ring(ext.accent)
                        .border_1()
                        .border_color(gpui::rgb(palette.divider))
                        .text_color(gpui::rgb(palette.text))
                        .text_xs()
                        .child("Check now")
                        .on_click(move |_ev, _window, cx: &mut App| {
                            session_check.update(cx, |s, cx| s.check_after_unknown(cx));
                        }),
                )
                .child(if live {
                    // Retry only makes sense on a live link (the disconnect
                    // forced the gate ReadOnly; re-enable first).
                    div()
                        .id("editor-retry-btn")
                        .h(px(24.))
                        .px_2()
                        .flex()
                        .items_center()
                        .rounded_sm()
                        .cursor_pointer()
                        .focusable()
                        .focus_visible_ring(ext.accent)
                        .bg(gpui::rgb(ext.accent))
                        .text_color(gpui::rgb(0xFFFFFF))
                        .text_xs()
                        .child("Retry save")
                        .on_click(move |_ev, _window, cx: &mut App| {
                            session_retry.update(cx, |s, cx| s.retry_save(cx));
                        })
                        .into_any_element()
                } else {
                    div()
                        .id("editor-retry-btn")
                        .h(px(24.))
                        .px_2()
                        .flex()
                        .items_center()
                        .rounded_sm()
                        .cursor_default()
                        .text_xs()
                        .text_color(gpui::rgb(palette.text_muted))
                        .child("Retry save (reconnect first)")
                        .into_any_element()
                }),
        )
}

/// A simple text banner (icon + text on a tinted surface).
fn render_banner(icon: &str, text: &str, bg: u32, text_color: u32) -> impl IntoElement {
    let has_icon = !icon.is_empty();
    let mut row = div()
        .flex()
        .flex_row()
        .gap_2()
        .m_2()
        .p_2()
        .rounded_sm()
        .flex_shrink(0.)
        .bg(gpui::rgb(bg))
        .text_color(gpui::rgb(text_color))
        .text_xs();
    if has_icon {
        row = row.child(div().child(icon.to_string()));
    }
    row.child(div().child(text.to_string()))
}

/// The label for an inline field edit row.
fn field_path_label(path: &value_model::editor::FieldPath) -> SharedString {
    use value_model::editor::FieldPath;
    SharedString::from(match path {
        FieldPath::Scalar => "value".to_string(),
        FieldPath::HashField(i) => format!("field #{i}"),
        FieldPath::ListItem(i) => format!("item #{i}"),
        FieldPath::SetMember(i) => format!("member #{i}"),
        FieldPath::ZSetMember(i) => format!("member #{i}"),
        FieldPath::ZSetScore(i) => format!("score #{i}"),
        FieldPath::StreamEntry(i) => format!("entry #{i} (read-only)"),
    })
}

/// The TTL editor row (acceptance 4): shown when editing is enabled and
/// a key is selected. [Set] = EXPIRE with the parsed seconds; empty or
/// [Remove] = PERSIST. Hidden (with a muted note) when read-only.
pub fn render_ttl_editor(
    session: &gpui::Entity<ConnectionSession>,
    panel: &ValueEditorPanel,
    palette: &Palette,
    ext: ExtTokens,
    cx: &App,
) -> gpui::AnyElement {
    let s = session.read(cx);
    let write_enabled = s.gate_state() == write_safety::GateState::WriteEnabled;
    let has_key = s.selected_display().is_some();
    let ttl_text = s.ttl_display().unwrap_or_default();

    if !has_key {
        return div().h(px(0.)).into_any_element();
    }
    if !write_enabled {
        // Read-only: the TTL display alone (no controls), with the
        // honest "enable editing" hint.
        let mut row = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .flex_shrink(0.)
            .text_xs()
            .text_color(gpui::rgb(palette.text_muted));
        if !ttl_text.is_empty() {
            row = row.child(div().child(format!("⏳ {ttl_text}")));
        }
        return row
            .child(div().child("(enable editing to change the TTL)"))
            .into_any_element();
    }

    let session_set = session.clone();
    let session_clear = session.clone();
    div()
        .id("ttl-editor-row")
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .px_2()
        .py_1()
        .flex_shrink(0.)
        .text_xs()
        .text_color(gpui::rgb(palette.text))
        .child(if ttl_text.is_empty() {
            div()
                .text_color(gpui::rgb(palette.text_muted))
                .child("⏳ TTL unknown")
        } else {
            div().child(format!("⏳ {ttl_text}"))
        })
        .child(div().w(px(90.)).child(panel.ttl_input.clone()))
        .child({
            let session_set = session_set.clone();
            let ttl_input = panel.ttl_input.clone();
            div()
                .id("ttl-set-btn")
                .h(px(22.))
                .px_2()
                .flex()
                .items_center()
                .rounded_sm()
                .cursor_pointer()
                .focusable()
                .focus_visible_ring(ext.accent)
                .bg(gpui::rgb(ext.accent))
                .text_color(gpui::rgb(0xFFFFFF))
                .child("Set")
                .on_click(move |_ev, _window, cx: &mut App| {
                    // Empty input → PERSIST (remove the TTL); a number →
                    // EXPIRE. Parse at click time from the input entity.
                    let text = ttl_input.read(cx).content().trim().to_string();
                    let parsed = text.parse::<i64>().ok();
                    session_set.update(cx, |s, cx| match parsed {
                        Some(secs) if secs >= 0 => s.set_ttl(secs, cx),
                        _ => s.clear_ttl(cx),
                    });
                })
        })
        .child({
            div()
                .id("ttl-clear-btn")
                .h(px(22.))
                .px_2()
                .flex()
                .items_center()
                .rounded_sm()
                .cursor_pointer()
                .focusable()
                .focus_visible_ring(ext.accent)
                .border_1()
                .border_color(gpui::rgb(palette.divider))
                .text_color(gpui::rgb(palette.text))
                .child("Remove")
                .on_click(move |_ev, _window, cx: &mut App| {
                    session_clear.update(cx, |s, cx| s.clear_ttl(cx));
                })
        })
        .into_any_element()
}

/// The Phase 5 modals (frame-03 pattern): Enable Editing, Delete Key,
/// Discard Edits guard, and the F5 Conflict modal.
///
/// Rendered as an overlay by the workspace when the session holds a
/// pending `EditorModal`.
/// A workspace-supplied post-discard hook (applies deferred navigation).
pub type WindowFn = Box<dyn Fn(&mut Window, &mut App) + 'static>;

pub fn render_editor_modal(
    session: &gpui::Entity<ConnectionSession>,
    panel: &ValueEditorPanel,
    modal: &EditorModal,
    palette: &Palette,
    ext: ExtTokens,
    on_discard: Option<WindowFn>,
    _cx: &App,
) -> impl IntoElement {
    let session_confirm = session.clone();
    let session_cancel = session.clone();

    // Per-modal copy.
    type SessionFn = Box<dyn Fn(&mut ConnectionSession, &mut gpui::Context<ConnectionSession>)>;
    let (title, body, confirm_label, on_confirm): (&str, String, &str, SessionFn) = match modal {
        EditorModal::EnableEditing => (
            "Enable Editing",
            "Write commands will be executed against this connection. \
             Destructive commands still require their own confirmation. \
             The gate resets to Read-only on disconnect."
                .into(),
            "Enable Editing",
            Box::new(|s, cx| s.confirm_enable_editing(cx)),
        ),
        EditorModal::DeleteKey => (
            "Delete key",
            format!(
                "Delete key {} ({}? This cannot be undone.)",
                session.read(_cx).selected_key_display().unwrap_or_default(),
                session
                    .read(_cx)
                    .key_type_display()
                    .unwrap_or_else(|| "unknown type".into())
            ),
            "Delete",
            Box::new(|s, cx| s.confirm_delete_key(cx)),
        ),
        EditorModal::DiscardEdits {
            key_display,
            fields,
        } => (
            "Discard unsaved edits?",
            format!(
                "You have unsaved edits to {key_display} ({fields} field{}). \
                 Discarding them cannot be undone.",
                if *fields == 1 { "" } else { "s" }
            ),
            "Discard",
            // The workspace applies the deferred navigation itself (the
            // pending nav is workspace state, not session state).
            Box::new(|s, cx| {
                s.discard_edits(cx);
            }),
        ),
        EditorModal::Conflict => (
            "Value changed on the server",
            "The value changed on the server since you loaded it. \
             Overwriting will discard the server's version."
                .into(),
            "Overwrite anyway",
            Box::new(|s, cx| s.save_overwrite_conflict(cx)),
        ),
    };
    let _ = &session_confirm;

    let dark = is_dark(ext);
    let palette = *palette;

    // Cancel (ghost, always active).
    let cancel_btn = crate::modal::modal_button(
        "editor-modal-cancel-btn",
        &palette,
        ext,
        crate::modal::ModalButtonKind::Ghost,
    )
    .child("Cancel")
    .on_click(move |_ev, _window, cx: &mut App| {
        session_cancel.update(cx, |s, cx| s.cancel_editor_modal(cx));
    });

    // Confirm: accent for Enable/Overwrite/Discard, danger for Delete.
    let danger = matches!(modal, EditorModal::DeleteKey);
    let kind = if danger {
        crate::modal::ModalButtonKind::Danger
    } else {
        crate::modal::ModalButtonKind::Primary
    };
    // The confirm callback: DiscardEdits routes to the workspace's
    // callback (it owns the deferred navigation); every other variant
    // runs the session closure. Rides in a RefCell<Option<..>> so the Fn
    // click closure can take it exactly once. A second RefCell twin
    // serves the Enter path (each is taken exactly once).
    let is_discard = matches!(modal, EditorModal::DiscardEdits { .. });
    let confirm_fn: std::cell::RefCell<Option<WindowFn>> = if is_discard {
        let on_discard = on_discard.unwrap_or_else(|| {
            // No callback supplied: discard the edits only (no nav).
            Box::new(|_window, _cx: &mut App| {})
        });
        let session_discard = session.clone();
        std::cell::RefCell::new(Some(Box::new(move |window: &mut Window, cx: &mut App| {
            session_discard.update(cx, |s, cx| {
                s.discard_edits(cx);
                s.cancel_editor_modal(cx);
            });
            on_discard(window, cx);
        })
            as Box<dyn Fn(&mut Window, &mut App) + 'static>))
    } else {
        let session_confirm_fn = on_confirm;
        let session_go = session_confirm.clone();
        std::cell::RefCell::new(Some(Box::new(move |_window: &mut Window, cx: &mut App| {
            session_go.update(cx, |s, cx| session_confirm_fn(s, cx));
        })
            as Box<dyn Fn(&mut Window, &mut App) + 'static>))
    };
    let confirm_btn = crate::modal::modal_button("editor-modal-confirm-btn", &palette, ext, kind)
        .child(confirm_label.to_string())
        .on_click(move |_ev, window, cx: &mut App| {
            let taken = confirm_fn.borrow_mut().take();
            if let Some(f) = taken {
                f(window, cx);
            }
        });

    // The Conflict modal gains [Show current] between Cancel and
    // Overwrite: a read-only re-read + compare, surfaced as text.
    let mut button_row = div()
        .flex()
        .flex_row()
        .justify_end()
        .gap_2()
        .child(cancel_btn);
    if matches!(modal, EditorModal::Conflict) {
        let session_show = session.clone();
        button_row = button_row.child(
            crate::modal::modal_button(
                "conflict-show-current-btn",
                &palette,
                ext,
                crate::modal::ModalButtonKind::Ghost,
            )
            .child("Show current")
            .on_click(move |_ev, _window, cx: &mut App| {
                session_show.update(cx, |s, cx| s.check_after_unknown(cx));
            }),
        );
    }
    let button_row = button_row.child(confirm_btn);

    // Escape cancels. Enter is NOT armed on the destructive variants
    // (Delete/Discard): an accidental keypress must not delete data;
    // Enable/Conflict confirm via Enter (the primary-action contract).
    // Enter runs the same session logic through an Rc-shared twin of
    // the click closure (each firing is idempotent at the session
    // level: the modal state transitions exactly once).
    let enter_armed = !danger && !is_discard;
    let session_escape = session.clone();
    let session_enter = session.clone();
    let title_text = title.to_string();
    let body_text = body;

    // The Enter twin: same confirm logic the button runs, reached from
    // the session state (never the button's take-once cell — Enter and
    // click each fire the session action once).
    let confirm_session_fn: SessionFn = match modal {
        EditorModal::EnableEditing => Box::new(|s, cx| s.confirm_enable_editing(cx)),
        EditorModal::DeleteKey => Box::new(|s, cx| s.confirm_delete_key(cx)),
        EditorModal::DiscardEdits { .. } => Box::new(|s, cx| {
            s.discard_edits(cx);
        }),
        EditorModal::Conflict => Box::new(|s, cx| s.save_overwrite_conflict(cx)),
    };

    let _ = panel;
    crate::modal::modal_scaffold(
        &palette,
        crate::modal::ModalKeys {
            dark,
            width: crate::modal::MODAL_WIDTH,
            card_focus: None,
            armed: crate::modal::enter_armed(enter_armed),
            escape: Box::new(move |_window, cx: &mut App| {
                session_escape.update(cx, |s, cx| s.cancel_editor_modal(cx));
            }),
            enter: Box::new(move |_window, cx: &mut App| {
                if enter_armed {
                    session_enter.update(cx, |s, cx| confirm_session_fn(s, cx));
                }
            }),
        },
        move |card| {
            let text = palette.text;
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
                            .text_color(gpui::rgb(if danger { error_text } else { text }))
                            .child("⚠"),
                    )
                    .child(
                        div()
                            .text_base()
                            .font_weight(gpui::FontWeight::BOLD)
                            .text_color(gpui::rgb(if danger { error_text } else { text }))
                            .child(title_text),
                    ),
            )
            .child(div().text_sm().text_color(gpui::rgb(text)).child(body_text))
            .child(button_row)
        },
    )
}

/// True when the extended tokens are the dark set.
fn is_dark(ext: ExtTokens) -> bool {
    ext.accent == ExtTokens::DARK.accent
}

/// The per-field rows for the editable-fields list:
/// (label, current text, field path).
fn editable_fields(
    editor: &value_model::editor::EditorState,
) -> Vec<(String, String, value_model::editor::FieldPath)> {
    use value_model::editor::FieldPath;
    use value_model::editor::{EditedValue, Slot};
    let slot_text = |s: &Slot| -> String {
        slot_bytes(s)
            .map(|b| b.to_lossy_display().into_owned())
            .unwrap_or_default()
    };
    let mut out = Vec::new();
    match &editor.working {
        EditedValue::Hash(pairs) => {
            for (i, (field, slot)) in pairs.iter().enumerate() {
                out.push((
                    format!("{} = field #{}", field.to_lossy_display(), i),
                    slot_text(slot),
                    FieldPath::HashField(i),
                ));
            }
        }
        EditedValue::List(items) => {
            for (i, slot) in items.iter().enumerate() {
                out.push((
                    format!("item #{i}"),
                    slot_text(slot),
                    FieldPath::ListItem(i),
                ));
            }
        }
        EditedValue::Set(members) => {
            for (i, slot) in members.iter().enumerate() {
                out.push((
                    format!("member #{i}"),
                    slot_text(slot),
                    FieldPath::SetMember(i),
                ));
            }
        }
        EditedValue::ZSet(entries) => {
            for (i, e) in entries.iter().enumerate() {
                out.push((
                    format!("member #{i}"),
                    slot_text(&e.member),
                    FieldPath::ZSetMember(i),
                ));
                out.push((
                    format!("score #{i}"),
                    number_text_of(e),
                    FieldPath::ZSetScore(i),
                ));
            }
        }
        EditedValue::Scalar(_) | EditedValue::Json(_) => {}
        EditedValue::Stream => {}
    }
    out
}

/// The score slot's display text.
fn number_text_of(entry: &value_model::editor::EditedZSetEntry) -> String {
    match entry.score {
        value_model::editor::SlotScore::Original(s) | value_model::editor::SlotScore::Edited(s) => {
            format!("{s}")
        }
    }
}

/// The scalar editor's seed text (the working buffer's current bytes).
fn working_scalar_text(editor: &value_model::editor::EditorState) -> String {
    use value_model::editor::EditedValue;
    match &editor.working {
        EditedValue::Scalar(slot) | EditedValue::Json(slot) => slot_bytes(slot)
            .map(|b| b.to_lossy_display().into_owned())
            .unwrap_or_default(),
        _ => String::new(),
    }
}

/// The bytes a slot contributes.
fn slot_bytes(slot: &value_model::editor::Slot) -> Option<&value_model::raw::RawBytes> {
    match slot {
        value_model::editor::Slot::Original(b) | value_model::editor::Slot::Edited(b) => Some(b),
    }
}
