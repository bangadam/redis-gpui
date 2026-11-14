# Accessibility — a written report (not a promise)

**Status: Phase 7 report.** Per PRD (story 50, acceptance 11) and PLAN
§6/§7, this is a **report of what was inspected and verified at the
code level**, and an explicit list of what remains **unverified**.
Nothing here claims screen-reader support works.

Date of review: 2026-11-08. GPUI pin:
`zed` rev `b5cd310b3b54dfc659fcc06453e6358843dfe216` (2026-11-07).

---

## 1. What AccessKit integration exists at the pinned GPUI rev

Findings below come from direct inspection of the checked-out source
at `~/.cargo/git/checkouts/zed-a70e2ad075855582/b5cd310/` (facts, not
documentation claims):

| Fact | Evidence (file at the pin) |
| --- | --- |
| GPUI integrates [AccessKit](https://accesskit.dev/) as a regular dependency | `crates/gpui/Cargo.toml` line 63: `accesskit.workspace = true` |
| Every platform backend wires a real AccessKit adapter | `crates/gpui_macos` → `accesskit_macos::SubclassingAdapter` (`src/window.rs` lines 2415–2422); `crates/gpui_windows` → `accesskit_windows`; `crates/gpui_linux` → `accesskit_unix` (both Wayland and X11 window.rs files define `deactivate_accessibility`) |
| The adapter is created for every window unless force-disabled | `crates/gpui/src/window.rs` lines ~1735–1800: `a11y_init` runs when `!accessibility_force_disabled`, with an activation handler, action handler, and deactivation handler |
| Elements opt in to the tree via `.id()` + `.role()` | `crates/gpui/src/_accessibility.rs` (a full doc guide) and `crates/gpui/src/elements/div.rs` lines 1357+ (`role`, `aria_toggled`, `aria_orientation`) |
| An official example exists | `crates/gpui/examples/a11y.rs`, plus `focus_visible.rs` for keyboard focus styling |
| Screen-reader support in Zed itself is still an **open issue** | [zed#41138](https://github.com/zed-industries/zed/issues/41138) (open at the time of PLAN approval) |

**Interpretation:** the plumbing (platform adapters, tree updates,
activation handlers) is present and active by default at our pin. What
GPUI does *not* give for free is a meaningful semantic tree: an app
must give interactive elements stable IDs and roles (`.id("…").role(Role::Button)`)
for assistive technology to see anything beyond an empty window.

## 2. What works in Redis GPUI today (verified claims only)

- **Full keyboard navigation (intent, code-verified).** The input
  stack binds keys via `bind_input_keys` (`src/text_input.rs`:
  backspace/delete/left/right/home/end, `cmd-a` select-all, escape,
  `cmd-v` paste, enter); the console toggles with `ctrl-`` (`src/console_panel.rs`,
  Zed's own terminal idiom); every modal is escape-dismissable and —
  when armed — enter-confirmable (`src/modal.rs`, the shared scaffold).
  The Phase 7 keyboard audit added: tab/shift-tab traversal at the
  workspace root, Enter-in-host submits the connect form, saved-rail
  arrow cursor + Enter prefill, tab-bar left/right + `ctrl-w`,
  keys-panel arrow cursor + tree Left/Right + Enter, `cmd-f`/`ctrl-f`
  focus search, `cmd-s`/`ctrl-s` save a dirty editor, and console
  Up/Down history. The complete key table is in `KEYS.md` (repo
  root) — cross-reference it there.
- **System light/dark following, per frame.** `src/workspace.rs` line
  ~3020 re-reads `window.appearance()` on every render and re-maps the
  theme; the token table itself is in `crates/theme/src/lib.rs`.
- **No color-only signaling (by design, token-audited).** The palette
  (`crates/theme/src/lib.rs`) exposes exactly five tokens
  (background/panel/divider/text/text_muted). Error and success states
  are never encoded as palette-color alone: read-only vs write mode is
  *text-labeled* (gate chips carry words), confirmations carry
  explicit sentences ("cannot be undone"), and destructive buttons
  carry text, not just red. Light and dark palettes are structurally
  symmetric — the same five slots, both themes — so parity is a
  checkable table, not an assertion.
- **Text remains readable at both appearances.** Both palettes meet
  roughly ≥ 4.5:1 body-text contrast against their backgrounds (text
  `0xEDEEF2` on `0x0D0F12` dark; `0x1E2128` on `0xF4F5F7` light).

## 3. What is UNKNOWN or unverified (stated plainly)

- **Screen-reader behavior on every OS.** zed#41138 is open; even with
  the adapters active, our app has not yet assigned roles/ARIA states
  (a grep of `src/` finds no `.role(` or `aria_*` calls yet — Phase 7
  polish work streams may add them; this report records the state at
  review time). Until a real screen reader is driven against a real
  window, screen-reader UX is **unverified** on macOS (VoiceOver),
  Windows (NVDA/Narrator) and Linux (Orca on both Wayland and X11).
- **Visible focus rings.** *Updated by the Phase 7 polish pass:* the
  app-wide keyboard-focus ring now ships — one shared helper,
  `FocusRingExt::focus_visible_ring` (`src/focus_ring.rs`), applied to
  every interactive element in both themes (1px accent border,
  `:focus-visible` semantics, transparent at rest so geometry never
  shifts). Visual verification remains the SMOKE.md checklist item
  (frame-by-frame at both appearances and all three text scales); code
  coverage is in `src/focus_ring.rs`'s unit test.
- **Adjustable text size.** *Updated by the Phase 7 polish pass:* the
  S/M/L text-scale preference ships (status-bar segmented control;
  0.9×/1.0×/1.15× via the frame rem-size lever, persisted to
  `<config_dir>/settings.json`; see `src/settings.rs`). PRD story 49 is
  now implemented; its SMOKE.md row graduates to the smoke checklist.
- **Reduced-motion respect.** No animations are shipped yet, so there
  is nothing to disable; no claim either way.

## 4. Per-OS screen-reader matrix (all defaults: unverified)

| OS | Assistive tech | Adapter present at pin | App semantic tree | Verified? |
| --- | --- | --- | --- | --- |
| macOS | VoiceOver | yes (`accesskit_macos`) | not yet populated (no `.role()` calls at review) | **unverified** |
| Windows | NVDA / Narrator | yes (`accesskit_windows`) | same | **unverified** |
| Linux/Wayland | Orca | yes (`accesskit_unix`) | same | **unverified** |
| Linux/X11 | Orca | yes (`accesskit_unix`) | same | **unverified** |

## 5. Follow-up list — concrete AccessKit adoption steps

If/when screen-reader support is attempted deliberately:

1. **Assign stable element IDs.** Follow the `_accessibility.rs` guide:
   every interactive element gets `.id("stable-name")`; IDs must be
   stable across frames so screen readers see continuity, not
   remove+add churn. List rows need per-row IDs (`.id(index)`), never
   a shared `text!` source location (the documented footgun).
2. **Assign roles.** Buttons → `Role::Button`; the key list →
   `Role::ListBox`/`Role::ListItem` (or GPUI's list primitives' roles
   if provided at the next pin); inputs → `Role::TextInput`; modals →
   `Role::Dialog`; the gate chips → `Role::ToggleButton` +
   `.aria_toggled(...)`.
3. **Label with text, not color.** The gate, TLS-insecure chip, and
   error surfaces already carry words — surface exactly those strings
   as the accessible labels so the semantics and the visuals cannot
   drift apart.
4. **Respect activation.** Handle AccessKit `Action` requests (GPUI
   already queues them into the app's action dispatch — see
   `window.rs` lines ~1780+) so voice-control clicks do the same thing
   as mouse clicks.
5. **Verify with a real screen reader per OS** — VoiceOver, NVDA, Orca
   (Wayland + X11) — and record results in SMOKE.md with real dates.
   Until that row is checked, no screen-reader claim may be made
   anywhere in this repo.
6. **Check prior art at each GPUI bump.** Zedis (vicanso/zedis) had to
   patch an AccessKit macOS window-close crash; re-check our pin's
   `SubclassingAdapter` drop path (`gpui_macos/src/window.rs` line
   1411) whenever the pin moves.

## 6. Honesty constraints (unchanged)

This report does not claim: screen-reader support, per-OS verified
a11y, contrast ratios beyond the two computed above, or that any
assistive-tech flow works. Every unverified item above stays unverified
until a fresh run says otherwise (PLAN §7).
