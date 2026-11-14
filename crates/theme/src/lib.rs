//! Light/dark tokens, system-appearance wrapper, text-size and focus tokens.
//!
//! Keeps views dumb: panels consume tokens, never hard-coded colors, so
//! light/dark parity becomes a checkable token table (PLAN.md section 3).
//! System-appearance following is wired by the app crate via GPUI's
//! `Window::appearance()`; this crate only maps appearance → palette.
//!
//! Dark palette below follows design frame 01 in PLAN.md/assets.

/// Dark theme tokens (frame 01: #0D0F12 background, #16181D panels, #EDEEF2
/// text). Returned by [`palette_for`] when the system reports a dark
/// appearance.
pub const DARK: Palette = Palette {
    background: 0x0D0F12,
    panel: 0x16181D,
    divider: 0x2A2D35,
    text: 0xEDEEF2,
    text_muted: 0x8B8F9A,
};

/// Light theme tokens (mirrors frame 04).
pub const LIGHT: Palette = Palette {
    background: 0xF4F5F7,
    panel: 0xFFFFFF,
    divider: 0xD8DAE0,
    text: 0x1E2128,
    text_muted: 0x6B6F7A,
};

/// One theme palette: plain RGB tokens for background, panel, divider, text.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    /// App-wide background color.
    pub background: u32,
    /// Panel surface color.
    pub panel: u32,
    /// 1px separator color between panels.
    pub divider: u32,
    /// Primary text color.
    pub text: u32,
    /// Secondary/muted text color.
    pub text_muted: u32,
}

/// Chooses the palette for an appearance flag (`true` = dark).
///
/// The app passes `window.appearance().is_dark()`; this stays the single
/// place that maps appearance → tokens.
pub const fn palette_for(dark: bool) -> &'static Palette {
    if dark {
        &DARK
    } else {
        &LIGHT
    }
}
