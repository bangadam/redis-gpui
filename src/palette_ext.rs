//! Small extensions over the `theme` crate tokens for form/status UI
//! (Phase 2). Kept in one file so a GPUI/theme change touches one place.

use gpui::{App, Global, WindowAppearance};

use theme::Palette;

/// Extended tokens derived from the base palette (Phase 2 UI needs
/// accent/selection/error surfaces that `crates/theme` doesn't define
/// yet). Derived here, app-side, so the theme crate stays untouched;
/// when theme grows these tokens, move them there.
#[derive(Clone, Copy, Debug)]
pub struct ExtTokens {
    /// Buttons and active affordances.
    pub accent: u32,
    /// Selected row/tab background.
    pub selection: u32,
    /// Inline-error surface background.
    pub error_surface: u32,
    /// Inline-error text.
    pub error_text: u32,
    /// Success/live indicator (status dot, "connected").
    pub success: u32,
}

impl ExtTokens {
    /// Tokens for the dark palette (frame 01).
    pub const DARK: ExtTokens = ExtTokens {
        accent: 0x3B82F6,
        selection: 0x2A3348,
        error_surface: 0x3A2020,
        error_text: 0xE06C75,
        success: 0x4ADE80,
    };

    /// Tokens for the light palette (frame 04).
    pub const LIGHT: ExtTokens = ExtTokens {
        accent: 0x2563EB,
        selection: 0xD8E0F5,
        error_surface: 0xF5D8D8,
        error_text: 0xB91C1C,
        success: 0x15803D,
    };

    /// Pick by dark flag.
    pub const fn for_dark(dark: bool) -> ExtTokens {
        if dark {
            Self::DARK
        } else {
            Self::LIGHT
        }
    }
}

/// The theme palette plus system appearance, installed as a GPUI global
/// at window creation (the workspace refreshes it each render from
/// `Window::appearance()`; this is the cached copy for non-render
/// contexts like text-input styling).
#[derive(Clone, Copy, Debug)]
pub struct ThemeGlobal {
    /// The palette.
    pub palette: Palette,
    /// True when the window appearance is dark.
    pub dark: bool,
}

impl Global for ThemeGlobal {}

/// Install/refresh the theme global (cheap; called per render).
pub fn set_theme_global(cx: &mut App, appearance: WindowAppearance) {
    let dark = matches!(
        appearance,
        WindowAppearance::Dark | WindowAppearance::VibrantDark
    );
    cx.set_global(ThemeGlobal {
        palette: *theme::palette_for(dark),
        dark,
    });
}

/// Accessors for form field colors, derived from the palette.
pub trait PaletteExt {
    /// Field background (slightly raised from the panel).
    fn field_background(&self) -> u32;
    /// Field text.
    fn field_text(&self) -> u32;
}

impl PaletteExt for App {
    fn field_background(&self) -> u32 {
        let g = self.global::<ThemeGlobal>();
        if g.dark {
            0x1D2027
        } else {
            0xEEF0F3
        }
    }

    fn field_text(&self) -> u32 {
        let g = self.global::<ThemeGlobal>();
        g.palette.text
    }
}
