//! Phase 7 polish: ONE shared modal scaffold (scrim + centered card +
//! the standard keyboard contract) for every overlay in the app.
//!
//! Before this, `workspace.rs` (save dialog / forget / host-key),
//! `console_panel.rs` (destructive confirm), and `value_editor.rs`
//! (editor modals) each hand-rolled the same ~30 lines: an absolute
//! scrim, a centered `w(420|480) rounded_xl p_6` card, and an
//! `on_key_down` Escape handler. This module is that shape, extracted
//! — same geometry, same tokens, plus the acceptance-11 keyboard
//! contract applied uniformly:
//!
//! - **Escape** cancels (the caller's `on_escape`).
//! - **Enter** activates the primary action (the caller's
//!   `on_enter`, only when the modal reports it armed — e.g. the
//!   console confirm is armed only when the typed phrase matches).
//! - **Tab** cycles the modal's buttons: every button rendered through
//!   [`modal_button`] is a real tab stop with the shared focus ring,
//!   so keyboard users can reach Cancel as well as Confirm.
//!
//! The card itself is a tab group (GPUI `tab_group`) so the modal's
//! stops are ordered inside it, and its own focus handle is tracked so
//! the first Tab from outside lands inside the dialog, not beyond it.

use gpui::{div, prelude::*, px, Window};

use theme::Palette;

use crate::focus_ring::FocusRingExt;

/// Modal card width for standard dialogs (save, forget, editor).
pub const MODAL_WIDTH: f32 = 420.;
/// Modal card width for wide dialogs (host-key, console confirm: shows
/// fingerprints / phrases).
pub const MODAL_WIDTH_WIDE: f32 = 480.;
/// The modal's scrim background (same value every hand-rolled modal
/// used; kept here so light/dark stay identical).
const SCRIM_BG: u32 = 0x00000080;

/// Build the full modal overlay: scrim + centered card.
///
/// `body` receives the styled card to extend with children (title,
/// message, buttons) and returns it.
///
/// `keys.escape` runs for the Escape key (cancel). `keys.enter` runs
/// for the Enter key (primary); the modal reports it "armed" only when
/// the caller passes [`enter_armed`] a truthy gate (the
/// desaturated-confirm pattern: nothing happens until the phrase
/// matches).
///
/// `keys.card_focus`: the modal's own focus handle — tracked on the
/// card so the modal is a real tab-group root (Tab from the app behind
/// the scrim enters the dialog). `None` skips tracking (the card is
/// then purely decorative; every button is still individually
/// tabbable).
pub fn modal_scaffold(
    palette: &Palette,
    keys: ModalKeys,
    body: impl FnOnce(gpui::Stateful<gpui::Div>) -> gpui::Stateful<gpui::Div>,
) -> impl IntoElement {
    let dialog_bg = if keys.dark { 0x16181D } else { 0xFFFFFF };
    let mut card = div()
        .id("modal-card")
        .tab_group()
        .w(px(keys.width))
        .rounded_xl()
        .p_6()
        .bg(gpui::rgb(dialog_bg))
        .border_1()
        .border_color(gpui::rgb(palette.divider))
        .shadow_lg()
        .flex()
        .flex_col()
        .gap_3()
        .on_key_down(move |ev: &gpui::KeyDownEvent, window, cx: &mut gpui::App| {
            match ev.keystroke.key.as_str() {
                "escape" => (keys.escape)(window, cx),
                "enter" if keys.armed => (keys.enter)(window, cx),
                _ => {}
            }
        });
    if let Some(handle) = keys.card_focus.as_ref() {
        card = card.track_focus(handle);
    }
    let card = body(card);

    div()
        .id("modal-scrim")
        .absolute()
        .inset_0()
        .size_full()
        .bg(gpui::rgba(SCRIM_BG))
        .flex()
        .items_center()
        .justify_center()
        .child(card)
}

/// The modal's key-handler callback type.
type KeyHandler = Box<dyn Fn(&mut Window, &mut gpui::App) + 'static>;

/// The modal's keyboard contract + chrome parameters (bundled so
/// [`modal_scaffold`] stays under the argument-count lint; fields are
/// documented on the scaffold).
pub struct ModalKeys {
    /// Dark-mode flag (card background token).
    pub dark: bool,
    /// Card width ([`MODAL_WIDTH`] / [`MODAL_WIDTH_WIDE`]).
    pub width: f32,
    /// The modal's own focus handle, if any (see the scaffold docs).
    pub card_focus: Option<gpui::FocusHandle>,
    /// Whether Enter fires the primary action.
    pub armed: bool,
    /// Escape handler (cancel).
    pub escape: KeyHandler,
    /// Enter handler (primary; runs only when armed).
    pub enter: KeyHandler,
}

/// A standard modal button: 30px tall, focusable, ringed, and a real
/// tab stop. `danger` picks the error-text background (Confirm-style
/// filled), `primary` the accent, otherwise a ghost (border + text).
///
/// The caller still attaches `.on_click` and the label child.
pub fn modal_button(
    id: impl Into<gpui::ElementId>,
    palette: &Palette,
    ext: crate::palette_ext::ExtTokens,
    kind: ModalButtonKind,
) -> gpui::Stateful<gpui::Div> {
    let div = div()
        .id(id)
        .h(px(30.))
        .px_3()
        .flex()
        .items_center()
        .rounded_sm()
        .focus_visible_ring(ext.accent);
    match kind {
        ModalButtonKind::Ghost => div
            .cursor_pointer()
            .border_1()
            .border_color(gpui::rgb(palette.divider))
            .text_color(gpui::rgb(palette.text)),
        ModalButtonKind::Primary => div
            .cursor_pointer()
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .bg(gpui::rgb(ext.accent))
            .text_color(gpui::rgb(0xFFFFFF)),
        ModalButtonKind::Danger => div
            .cursor_pointer()
            .font_weight(gpui::FontWeight::SEMIBOLD)
            .bg(gpui::rgb(ext.error_text))
            .text_color(gpui::rgb(0xFFFFFF)),
    }
}

/// The button flavor for [`modal_button`].
pub enum ModalButtonKind {
    /// Ghost: border + panel text (Cancel / Close / secondary).
    Ghost,
    /// Primary: accent background (Save / Trust / Enable).
    Primary,
    /// Danger: error background (Confirm / Delete / Forget).
    Danger,
}

/// The Enter/Escape contract's "armed" flag helper: a modal whose
/// confirm button is desaturated (e.g. phrase mismatch) reports
/// `false` so Enter is absorbed.
pub fn enter_armed(confirm_enabled: bool) -> bool {
    confirm_enabled
}
