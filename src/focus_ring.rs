//! Phase 7, acceptance 11 (the visible-focus half): one shared focus
//! ring so every interactive element shows the same keyboard-focus
//! indicator in dark AND light themes.
//!
//! **Design (the pinned GPUI rev's own idiom):** Zed's `button_like.rs`
//! and `toggle.rs` draw keyboard focus by refining `border_color`
//! inside a `.focus_visible(...)` closure — GPUI's `:focus-visible`
//! semantics: the style fires only when the element is focused AND the
//! last input was the keyboard (Tab traversal), never on mouse clicks.
//! There is no `outline` in `StyleRefinement` at this revision, so the
//! ring IS the border: a 1px "reserved" border that is transparent at
//! rest (so adding it never shifts layout) and accent-colored when
//! keyboard-focused.
//!
//! **One rule for the ring color:** the accent token from
//! [`crate::palette_ext::ExtTokens`], which already encodes the
//! dark/light variants (dark `0x3B82F6`, light `0x2563EB`).
//!
//! # Usage
//!
//! The element must be a `Stateful<Div>` (have an `.id()`) that is
//! focusable. Two paths:
//!
//! - `.track_focus(&handle)` — the element shares an existing focus
//!   handle (the pattern `text_input.rs` uses), or
//! - `.focusable()` — GPUI creates an implicit handle from the element
//!   state; this is what plain clickable buttons/rows/chips want.
//!   Click-to-focus comes free: GPUI auto-focuses any element that
//!   owns/tracks a focus handle on mouse-down (verified in the
//!   vendored `div.rs`'s `paint_mouse_listeners`).
//!
//! Then append `.focus_visible_ring(accent)`:
//!
//! ```ignore
//! div().id("my-btn").focusable().focus_visible_ring(ext.accent)
//!     .on_click(...)
//! ```
//!
//! The helper reserves the border at rest (transparent, 1px), so the
//! focused ring never changes element size — the standard "no layout
//! shift on focus" accessibility rule.
//!
//! Requires `gpui::prelude::*` (the rest of the app already has it).

use gpui::StyleRefinement;

/// Attach the standard keyboard-focus ring to this element.
///
/// 1px accent border at keyboard focus, transparent at rest (no layout
/// shift). Applies ONLY on keyboard focus (GPUI's `:focus-visible`).
pub trait FocusRingExt: gpui::StatefulInteractiveElement + gpui::Styled + Sized {
    /// The standard focus ring: accent border, keyboard focus only,
    /// transparent at rest so geometry never shifts.
    fn focus_visible_ring(self, accent: u32) -> Self {
        self.border_1()
            .border_color(gpui::rgba(TRANSPARENT_BORDER))
            .focus_visible(move |style: StyleRefinement| ring_style(style, accent))
    }
}

impl FocusRingExt for gpui::Stateful<gpui::Div> {}

/// The fully transparent border color used to reserve the ring's 1px
/// at rest (pub for tests and custom one-offs).
pub const TRANSPARENT_BORDER: u32 = 0x00000000;

/// The ring refinement (pub so modals / custom elements reuse the
/// exact same geometry: accent border color, nothing else).
pub fn ring_style(mut style: StyleRefinement, accent: u32) -> StyleRefinement {
    style.border_color = Some(gpui::rgba(accent).into());
    style
}

/// A ready-made refinement for tests and custom elements.
#[cfg(test)]
pub fn ring_refinement(accent: u32) -> StyleRefinement {
    ring_style(gpui::StyleRefinement::default(), accent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_refinement_sets_only_the_border_color() {
        let r = ring_refinement(0x3B82F6);
        assert_eq!(r.border_color, Some(gpui::rgba(0x3B82F6).into()));
        // Nothing else in the refinement is touched: the ring must not
        // change geometry (border width comes from the element's own
        // `.border_1()`).
        assert!(r.background.is_none());
        assert!(r.box_shadow.is_none());
    }
}
