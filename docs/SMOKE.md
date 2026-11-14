# SMOKE.md — per-OS manual checklist

**Every row defaults to `unverified`.** A row is only checked after a
fresh run on that OS, on that date, by a human (PLAN §7: "any OS not
actually tested is labeled unverified; no OS is claimed working
without a fresh run"). Nothing in this file passes by default.

**How to run the app for a smoke session:**

```sh
cargo run          # from the repo root (Rust 1.98.1 via rustup override)
```

You also need a reachable Redis. Local dev one:

```sh
redis-server --port 6379 --daemonize yes   # or docker run -p 6379:6379 redis:7
```

For the TLS/SSH rows you need the fixtures described per-row (TLS
Redis, an sshd bastion). For the million-key row see
`crates/app-workflows/src/scale_measure.rs` (`REDIS_GPUI_SCALE=1`
harness) or seed manually.

Rows marked **[live-only]** were flagged "untested live" during the
phase 5/6 development work: the logic is unit-tested headlessly, but
nobody has driven it against the real thing yet. They are the first
rows to run.

---

## macOS (Apple Silicon / Intel)

| # | Flow | How to run | Result |
|---|---|---|---|
| 1 | First-run connect (plaintext) | `cargo run`; fill host `127.0.0.1`, port 6379, db 0; Connect. Expect: a tab appears, keys panel starts scanning progressively. | ☐ unverified |
| 2 | Browse tree + flat | Connect; toggle tree/flat; expand `session:…` namespaces; switch back. Expect: same keys, colon hierarchy only in tree. | ☐ unverified |
| 3 | Search + cancel | Type in search (prefix mode), switch to contains, hit Esc mid-search. Expect: results arrive progressively; Esc stops delivery, UI returns interactive. | ☐ unverified |
| 4 | DB switch | If server has >1 db: switch db in the switcher. Expect: key list restarts for the new db; counts reset. | ☐ unverified |
| 5 | Console read-only block | Open console (`ctrl-\``); run `GET somekey` → runs; run `FLUSHALL` → refused with explanation; run `EVAL "return 1" 0` → refused; run unknown `FOOBAR` → refused. | ☐ unverified |
| 6 | Console: enable editing + confirm modal | Enable editing for the connection; run `DEL probe:json-valid` → confirmation modal naming what is lost; decline → nothing happens (verify key still exists). | ☐ unverified |
| 7 | Value edit save/discard | Open a string value; edit; Save → re-read shows new bytes; edit again then Discard → warning shown, original bytes restored. | ☐ unverified |
| 8 | Dirty guard | Edit a value, then click another key. Expect: discard warning, not silent loss. Refresh (on-demand) must NOT clobber the dirty editor. | ☐ unverified |
| 9 | TTL CRUD | Set a TTL on a key; it shows; remove TTL; refresh; key persists; wait for expiry → refresh → key gone. | ☐ unverified |
| 10 | Delete key | Enable editing; delete a key; confirm; key gone from list after refresh. | ☐ unverified |
| 11 | TLS verify-full | Connect to a Redis with a self-signed/untrusted cert, no override. **[live-only]** Expect: connection fails with a TLS error (trust refused). | ☐ unverified |
| 12 | TLS insecure override + chip | Same server, set the insecure override. **[live-only]** Expect: connects; a visible "TLS verification disabled" chip stays while connected. | ☐ unverified |
| 13 | SSH TOFU prompt | Connect through an SSH tunnel to a first-seen host. **[live-only]** Expect: host-key fingerprint prompt; Trust & connect proceeds and records; Reject aborts. | ☐ unverified |
| 14 | SSH changed-key hard block | Change the server host key; reconnect. **[live-only]** Expect: hard block with clear error; no override offered. | ☐ unverified |
| 15 | Keyring save + prefill | Save a connection with password (macOS Keychain). Expect: saved list shows it; reconnect prompts nothing (prefill); no plaintext password in app files (search the config dir). | ☐ unverified |
| 16 | Keyring absent path | On a machine/CI without a keychain (or simulate). Expect: prompt per session; nothing written to disk. | ☐ unverified |
| 17 | Million-key interactive feel | Point at the 1M fixture (harness or manual seed); browse, search, cancel. Expect: UI stays responsive; no full keyspace load; honest "N+" counts. | ☐ unverified |
| 18 | Light/dark switch | Flip system appearance mid-session. Expect: app follows per frame, both palettes readable. | ☐ unverified |
| 19 | Text size | Adjust text size if/when the setting exists. Expect: layout reflows, focus still visible. (Currently: no user-facing text-size preference — row documents the gap.) | ☐ unverified |
| 20 | Keyboard-only session | Complete flows 1–10 using only the keyboard; see `KEYS.md`. Expect: every control reachable; focus visible at every step. | ☐ unverified |
| 21 | Console log privacy eyeball | Run commands incl. AUTH-protected connect; inspect anything the app writes (logs/console). Expect: no passwords, key data, or command results visible. | ☐ unverified |

## Linux (Ubuntu LTS — Wayland and X11 are separate rows)

Run the same 21 flows. Platform-specific notes:

| # | Flow | Result |
|---|---|---|
| L0 | Window opens at all (Wayland) — `cargo run` under a Wayland session | ☐ unverified |
| L0b | Window opens at all (X11) — `cargo run` with `XDG_SESSION_TYPE=x11` | ☐ unverified |
| L15b | Keyring = Secret Service (gnome-keyring) save/prefill; absent-keychain path on a bare WM | ☐ unverified |
| L22 | Console toggle `ctrl-\`` maps sanely on a layout without backtick on ctrl (record the actual key) | ☐ unverified |

All other flows: same checklist as macOS, each ☐ unverified until run.

## Windows (x64)

Run the same 21 flows. Platform-specific notes:

| # | Flow | Result |
|---|---|---|
| W15c | Credential Manager save/prefill | ☐ unverified |
| W23 | SSH-agent auth path (named risk: password + key-file first; agent may degrade with a clear message) **[live-only]** | ☐ unverified |
| W24 | Windows console toggle `ctrl-\`` key mapping | ☐ unverified |

All other flows: same checklist as macOS, each ☐ unverified until run.

---

## Recording results

Edit this file in place: flip `☐ unverified` to `☑ verified (<date>,
<OS/version>)` or `✗ failed (<date>, symptom)`. Never batch-fill rows
from one session across OSes — one OS, one fresh run, one date.

The **million-key fixture** for row 17 (both for the harness and manual
seeding) is defined in
`crates/app-workflows/src/scale_measure.rs` (`fixture_plan`): fixed
seed `0x5eed000000000001`, ~70/10/8/5/4/2/1 type mix, depth-2–4 colon
names, 16 B–4 KB values, ~2% TTL, plus probe keys (invalid UTF-8, NUL,
BOM, CRLF, emoji, valid/invalid JSON, 1 MB value). Run the harness:

```sh
REDIS_GPUI_SCALE=1 cargo test --test scale_measurements -p app-workflows -- --nocapture
```

(Docker required; without it the harness self-reports
`MEASURE-SKIPPED` and passes honestly.)
