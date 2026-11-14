# Keyboard Reference (Phase 7)

**How to read this document:** every flow below is marked with one of:

- `[implemented + smoke-listed]` — the handler is wired in code and the app
  was smoke-run (`cargo run`, dev profile) without panics; the *visual*
  behavior (focus rings, cursor highlight) is confirmed by inspection of the
  render code, not by an automated UI driver. This is the honest ceiling for
  a headless CI: no screenshot driver is wired up in this repo yet.
- `[unit-tested]` — an automated `cargo test` covers the logic.

Nothing is marked as screen-reader-ready. See
[`ACCESSIBILITY.md`](ACCESSIBILITY.md) for what the pinned GPUI
revision actually exposes (roles, labels, focus forwarding) — full
**AccessKit screen-reader support is NOT promised or claimed.**

## Global

| Context | Key | Action | Status |
|---|---|---|---|
| Anywhere | `tab` | Move focus to the next focusable element (buttons, chips, inputs, panel containers) | `[implemented + smoke-listed]` |
| Anywhere | `shift-tab` | Move focus to the previous focusable element | `[implemented + smoke-listed]` |
| Anywhere | `escape` (no modal open) | Clear focus from the current input / dismiss in-place UI | `[implemented + smoke-listed]` |
| Any modal open | `escape` | Cancel the topmost modal (decline forget, close save dialog, dismiss host-key prompt, decline console confirm) | `[implemented + smoke-listed]` |

Tab traversal is bound at the workspace root (`tab` / `shift-tab` →
`window.focus_next` / `focus_prev`) — the root `on_action` handlers in
`src/workspace.rs` (`bind_workspace_keys`).

## Connect form

| Key | Action | Status |
|---|---|---|
| `tab` / `shift-tab` | Walk the form: name → host → port → db → username → password → TLS toggle → SSH toggle → Connect | `[implemented + smoke-listed]` |
| `enter` (in host field) | Submit the form (same path as the Connect button; `EnterPressed` event from `SingleLineInput`, workspace subscription) | `[implemented + smoke-listed]` |
| `enter` (in a non-host field) | Move on / no-op (single-line inputs don't submit from port, db, etc.) | `[implemented + smoke-listed]` |

## Saved-connections rail

| Key | Action | Status |
|---|---|---|
| `tab` | Focus the rail (the rail is one tab stop; rows are arrow-driven) | `[implemented + smoke-listed]` |
| `up` / `down` (rail focused) | Move the cursor (highlighted row) through saved entries; clamps at the ends | `[implemented + smoke-listed]` |
| `enter` (rail focused) | Prefill the connect form from the saved entry at the cursor (name, host, port, TLS/SSH; password via keychain or the per-session prompt) | `[implemented + smoke-listed]` |
| `tab` from rail | Continue into the connect form | `[implemented + smoke-listed]` |

## Tab bar (connections)

| Key | Action | Status |
|---|---|---|
| `left` / `right` (tab bar focused) | Switch to the previous / next connection tab (the guarded path — a dirty editor asks first, same as a click) | `[implemented + smoke-listed]` |
| `ctrl-w` | Close the active connection tab (guarded; a dirty editor asks first) | `[implemented + smoke-listed]` |
| `tab` | Walk the individual tab items and their close buttons | `[implemented + smoke-listed]` |

## Keys panel (flat list and tree)

The keys panel container is the tab stop; rows are arrow-driven. In flat
mode: `up`/`down` move a cursor through the keys, `enter` opens the key in
the value panel. In tree mode: `up`/`down` move through visible rows,
`left`/`right` collapse/expand the node at the cursor (branch) — `left` on
a leaf key is a no-op — and `enter` on a leaf opens the key; on a branch it
toggles expansion. Every cursor move scrolls the row into view
(`UniformListScrollHandle::scroll_to_item(ix, Nearest)`).

| Key | Action | Status |
|---|---|---|
| `tab` | Focus the keys panel container | `[implemented + smoke-listed]` |
| `up` / `down` | Move the key / tree cursor; scroll into view | `[implemented + smoke-listed]` |
| `left` / `right` (tree mode) | Collapse / expand the node at the cursor | `[implemented + smoke-listed]` |
| `enter` | Open the key (leaf) / toggle expansion (branch) | `[implemented + smoke-listed]` |
| typing | Type-to-search (the search input keeps focus; the pattern filters the list) | `[implemented + smoke-listed]` |
| `cmd-f` / `ctrl-f` | Jump focus to the keys search input (`FocusSearch`) | `[implemented + smoke-listed]` |

## Value panel

| Key | Action | Status |
|---|---|---|
| `tab` | Walk the panel: view-mode chips, TTL controls, edit/discard/save buttons | `[implemented + smoke-listed]` |
| `cmd-s` / `ctrl-s` | Save the dirty editor (same path as the Save button; no-op when clean) | `[implemented + smoke-listed]` |
| `enter` (inline field edit) | Confirm the field's new value | `[implemented + smoke-listed]` |

## Console

| Key | Action | Status |
|---|---|---|
| `ctrl-\`` | Toggle the console panel open/closed | `[implemented + smoke-listed]` |
| `enter` (input line) | Submit the command to the active connection | `[implemented + smoke-listed]` |
| `up` / `down` (input line) | Walk the per-connection command history (down past the newest restores the draft) | `[implemented + smoke-listed]` |

## Modals (save dialog, forget, host-key, console confirm, editor dialogs)

| Key | Action | Status |
|---|---|---|
| `tab` / `shift-tab` | Cycle the modal's buttons (every `modal_button` is a real tab stop with the shared focus ring) | `[implemented + smoke-listed]` |
| `enter` | Activate the primary action — **only when armed** (save dialog: always; console confirm: only when the typed phrase matches; Enable Editing and Conflict-overwrite: armed; forget / delete-key / discard / host-key trust: deliberately NOT armed, Enter is absorbed) | `[implemented + smoke-listed]` |
| `escape` | Cancel (decline / close / dismiss — the topmost modal always closes) | `[implemented + smoke-listed]` |

## Settings (status bar)

| Key | Action | Status |
|---|---|---|
| `tab` | Reach the S/M/L segmented control (each chip is a focus stop with the focus ring) | `[implemented + smoke-listed]` |
| `enter` / click | Apply the scale instantly (0.9× / 1.0× / 1.15× via the frame rem-size lever; persisted to `<config_dir>/settings.json`) | `[implemented + smoke-listed]` — persistence logic `[unit-tested]` in `src/settings.rs` |

## Focus rings (acceptance 11)

Every interactive element above shows the same visible keyboard-focus
indicator: a 1px accent border that is transparent at rest (no layout
shift) and fires only on *keyboard* focus (GPUI's `:focus-visible`
semantics — mouse clicks never draw it). The ring is one shared helper,
`FocusRingExt::focus_visible_ring(accent)` on `Stateful<Div>` in
`src/focus_ring.rs`, applied uniformly to buttons, chips, toggles, rows,
segments, and modal buttons.
