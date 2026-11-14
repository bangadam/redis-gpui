//! Phase 7: the app's user settings (non-secret, trivial JSON) and the
//! `TextScale` the acceptance-11 text-size control drives.
//!
//! **What is stored:** one JSON file, `settings.json`, in the
//! connections-store `config_dir()` (env override
//! `REDIS_GPUI_CONFIG_DIR`, then `~/.config/redis-gpui`). The file holds
//! exactly one field today — the text scale. No secret ever lands here
//! (PRD privacy: passwords live in the OS keyring only; this file is
//! display preference data).
//!
//! **How scaling works (the "one place" rule):** GPUI text sizes are
//! absolute `rems` resolved against the window's rem size (16px
//! default). Rather than multiplying 200 scattered `text_xs()`/
//! `text_sm()` literals, the workspace calls [`set_frame_rem_size`] at
//! the top of its render, which sets the *frame's rem-size base*: every
//! `rems(0.75)` painted this frame grows or shrinks with the setting.
//! One helper, one file, zero per-callsite changes — and the mechanism
//! is GPUI's own (`Window::set_rem_size`), so it is the
//! upgrade-stable surface. `rem_size()` still reads as the scaled
//! value for any code that resolves font sizes (e.g. the input
//! element's `style.font_size.to_pixels(window.rem_size())`).
//!
//! The scale is an in-memory [`TextScaleSetting`] GPUI global (cheap to
//! read per render), persisted on every change.

use connections_store::saved_connections::config_dir;
use gpui::{App, Global, Window};

/// The text size setting: Small / Medium / Large.
///
/// The multipliers are deliberately conservative (0.9 / 1.0 / 1.15):
/// the three-panel layout is density-oriented, and large must not
/// break the 26px key rows or the 28px input fields.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum TextScale {
    /// 0.9× — compact.
    Small,
    /// 1.0× — the default.
    #[default]
    Medium,
    /// 1.15× — comfortable.
    Large,
}

impl TextScale {
    /// All options in segmented-control order.
    pub const ALL: [TextScale; 3] = [TextScale::Small, TextScale::Medium, TextScale::Large];

    /// The short label for the segmented control.
    pub const fn label(self) -> &'static str {
        match self {
            TextScale::Small => "S",
            TextScale::Medium => "M",
            TextScale::Large => "L",
        }
    }

    /// The rem multiplier this scale applies.
    pub const fn multiplier(self) -> f32 {
        match self {
            TextScale::Small => 0.9,
            TextScale::Medium => 1.0,
            TextScale::Large => 1.15,
        }
    }

    /// Parse from the persisted JSON string (unknown values degrade to
    /// the default — a hand-edited file must not crash the app).
    pub fn from_persisted(s: &str) -> TextScale {
        match s {
            "small" => TextScale::Small,
            "large" => TextScale::Large,
            _ => TextScale::Medium,
        }
    }

    /// The persisted JSON string.
    pub const fn persisted(self) -> &'static str {
        match self {
            TextScale::Small => "small",
            TextScale::Medium => "medium",
            TextScale::Large => "large",
        }
    }
}

/// The GPUI global holding the current text scale (installed at boot,
/// updated by the status-bar control; render code reads it via
/// [`text_scale`]).
#[derive(Clone, Copy, Debug)]
pub struct TextScaleSetting {
    /// The active scale.
    pub scale: TextScale,
}

impl Global for TextScaleSetting {}

/// The base rem size GPUI uses (16px); the override multiplies it.
const BASE_REM: f32 = 16.;

/// Read the current text scale from the global (default when unset).
pub fn text_scale(cx: &App) -> TextScale {
    cx.try_global::<TextScaleSetting>()
        .map(|g| g.scale)
        .unwrap_or_default()
}

/// The rem-size override for the current scale: base rem × multiplier.
///
/// Pass to [`with_text_scale`]; do NOT hand this to random elements.
pub fn scaled_rem(scale: TextScale) -> gpui::Pixels {
    gpui::px(BASE_REM * scale.multiplier())
}

/// Set the rem-size base for this frame: every rem-based text size in
/// the window resolves against it.
///
/// Call at the very top of `Render::render` (view rendering happens
/// during the paint phase, and `Window::rem_size()` — the value every
/// text/element resolution reads — keeps returning this base for the
/// rest of the frame's layout/prepaint/paint). Verified against the
/// vendored rev: the base is only ever assigned by `set_rem_size`,
/// never reset per frame, and `with_rem_size`'s override stack is
/// popped before paint would read it — so a render-time `with_rem_size`
/// call would be a no-op for text, while `set_rem_size` is the frame
///-wide lever (this is exactly how the deferred-draw path replays a
/// captured rem size).
///
/// Also returns the old base so callers can restore it if needed.
pub fn set_frame_rem_size(scale: TextScale, window: &mut Window) -> gpui::Pixels {
    let new_rem = scaled_rem(scale);
    let old = window.rem_size();
    window.set_rem_size(new_rem);
    old
}

/// Install the global and return the loaded scale (called once from
/// `Workspace::new`; the global is the render-time read path).
pub fn install_text_scale(cx: &mut App) -> TextScale {
    let scale = load_settings().scale;
    cx.set_global(TextScaleSetting { scale });
    scale
}

/// Change the scale: update the global and persist (best-effort — a
/// read-only config dir must not break the in-memory setting).
pub fn set_text_scale(scale: TextScale, cx: &mut App) {
    cx.set_global(TextScaleSetting { scale });
    let settings = SettingsFile { scale };
    if let Err(e) = save_settings(&settings) {
        // Non-secret preference data; the error text is safe to show.
        // Logged nowhere else (privacy discipline: keep the message
        // about the file, not about the user's data).
        eprintln!("settings persistence failed: {e}");
    }
}

/// The persisted settings file shape.
#[derive(Clone, Debug, PartialEq)]
pub struct SettingsFile {
    scale: TextScale,
}

impl SettingsFile {
    const fn new(scale: TextScale) -> Self {
        Self { scale }
    }
}

/// The settings file's path: `<config_dir>/settings.json`.
fn settings_path() -> std::path::PathBuf {
    config_dir().join("settings.json")
}

/// Load settings; a missing or malformed file yields the defaults
/// (hand-edited garbage must never take the app down).
pub fn load_settings() -> SettingsFile {
    let Ok(raw) = std::fs::read_to_string(settings_path()) else {
        return SettingsFile::new(TextScale::default());
    };
    let scale = raw
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            (k.trim() == "\"text_scale\"")
                .then(|| TextScale::from_persisted(v.trim().trim_matches('"')))
        })
        .unwrap_or_default();
    SettingsFile { scale }
}

/// Persist settings (creates the config dir if needed).
pub fn save_settings(settings: &SettingsFile) -> std::io::Result<()> {
    use std::io::Write;
    let dir = config_dir();
    std::fs::create_dir_all(&dir)?;
    let body = format!(
        "{{\n  \"text_scale\": \"{}\"\n}}\n",
        settings.scale.persisted()
    );
    let mut f = std::fs::File::create(settings_path())?;
    f.write_all(body.as_bytes())
}

/// A process-wide lock serializing env-var scoping across ALL test
/// modules that set `REDIS_GPUI_CONFIG_DIR` (settings AND
/// saved-connections). The two modules must share ONE mutex — separate
/// per-module locks let concurrent tests in the same process clobber
/// each other's scope.
#[cfg(test)]
pub(crate) static SETTINGS_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    fn with_config_dir(f: impl FnOnce(&std::path::Path)) {
        // The SAME lock saved_connections' tests take: one env var, one
        // guard, no cross-module interference under the default
        // multi-threaded test runner.
        let _guard = SETTINGS_ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = tempfile::tempdir().expect("tempdir");
        std::env::set_var("REDIS_GPUI_CONFIG_DIR", dir.path());
        f(dir.path());
        std::env::remove_var("REDIS_GPUI_CONFIG_DIR");
    }

    #[test]
    fn scale_multipliers_match_the_labels() {
        assert!((TextScale::Small.multiplier() - 0.9).abs() < f32::EPSILON);
        assert!((TextScale::Medium.multiplier() - 1.0).abs() < f32::EPSILON);
        assert!((TextScale::Large.multiplier() - 1.15).abs() < f32::EPSILON);
        // The labels are the segmented control's literal text.
        assert_eq!(TextScale::ALL.map(|s| s.label()), ["S", "M", "L"]);
    }

    #[test]
    fn scaled_rem_multiplies_the_base() {
        assert_eq!(scaled_rem(TextScale::Medium), gpui::px(16.));
        assert_eq!(scaled_rem(TextScale::Small), gpui::px(16. * 0.9));
        assert_eq!(scaled_rem(TextScale::Large), gpui::px(16. * 1.15));
    }

    #[test]
    fn persisted_round_trip() {
        for scale in TextScale::ALL {
            assert_eq!(TextScale::from_persisted(scale.persisted()), scale);
        }
    }

    #[test]
    fn settings_file_round_trip() {
        with_config_dir(|_dir| {
            let original = SettingsFile::new(TextScale::Large);
            save_settings(&original).expect("save");
            assert_eq!(load_settings(), original);
            // Overwrite with a different scale.
            save_settings(&SettingsFile::new(TextScale::Small)).expect("save 2");
            assert_eq!(load_settings(), SettingsFile::new(TextScale::Small));
        });
    }

    #[test]
    fn missing_file_yields_defaults() {
        with_config_dir(|dir| {
            assert!(!dir.join("settings.json").exists());
            assert_eq!(load_settings(), SettingsFile::new(TextScale::default()));
        });
    }

    #[test]
    fn malformed_file_yields_defaults() {
        with_config_dir(|dir| {
            std::fs::write(dir.join("settings.json"), "not json at all").unwrap();
            assert_eq!(load_settings(), SettingsFile::new(TextScale::default()));
        });
    }

    #[test]
    fn unknown_scale_value_degrades_to_medium() {
        assert_eq!(TextScale::from_persisted("gigantic"), TextScale::Medium);
        assert_eq!(TextScale::from_persisted(""), TextScale::Medium);
    }
}
