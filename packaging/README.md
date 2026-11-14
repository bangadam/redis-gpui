# Packaging (scaffolding — nothing has been built)

**Status: configuration + scripts written; zero artifacts produced.**
Per Adam's standing rules, packaging/release builds (`dist build`,
`cargo build --release`, `cargo dist`, creating actual .deb/.dmg/
.AppImage files) run **only on Adam's explicit command**. Agents write
config and scripts; they never execute them.

## What exists

| File | Purpose |
| --- | --- |
| `dist-workspace.toml` (repo root) | [dist](https://github.com/axodotdev/cargo-dist) (formerly cargo-dist) workspace config. Schema checked against the official config reference on 2026-11-08; `extra-artifacts` table shape is the one item to re-verify against your installed dist version before the first real run (flagged inline in the file). |
| `packaging/scripts/build-dmg.sh` | macOS .app bundle + .dmg via hdiutil (host-arch; run once per arch). Executable. |
| `packaging/scripts/build-deb.sh` | Ubuntu x64 .deb via hand-rolled dpkg layout + dpkg-deb (no cargo-deb dep). Executable. |
| `packaging/scripts/build-appimage.sh` | Linux x64 AppImage via the linuxdeploy AppDir flow. Executable. Run on Linux only. |

## App identity (changeable — pick your own before first release)

| Field | Value |
| --- | --- |
| Name | Redis GPUI |
| Identifier | `dev.bangadam.redis-gpui` (reverse-DNS; change freely until the first shipped artifact) |
| Version source | workspace `version = "0.1.0"` in root `Cargo.toml` — single source of truth, scripts read it via `cargo metadata` |
| License | MIT (workspace) |

## Icon — TODO (no file, by policy)

**There is no icon file and none was fabricated.** `assets/icon.svg`
exists but packaging needs real raster/icon assets:

- macOS: `packaging/icon.icns` (generate from icon.svg at 16→1024 px;
  `iconutil -c icns icon.iconset`)
- Linux (deb + AppImage): `packaging/icon-512.png` (512×512 PNG)

Spec: keep it a simple key/glyph mark over the dark panel token
(`#16181D`), readable at 16 px. Until the files exist, the scripts
print a NOTE and build without icons.

## Exact commands (for Adam, when wanted)

Prereq once: `cargo install dist-cli --version <same as
cargo-dist-version in dist-workspace.toml>`.

```sh
# plan first — dry run, no builds, safe to inspect:
dist plan

# build everything for the configured targets (RELEASE BUILD — Adam only):
dist build

# or a single artifact by hand (each is also a release build — Adam only):
packaging/scripts/build-dmg.sh                              # on macOS host
packaging/scripts/build-deb.sh                              # on Linux host
packaging/scripts/build-appimage.sh                         # on Linux host
```

Artifacts land in `dist/`. First release ships unsigned (PRD: clear
instructions, no signing, no auto-updates; `install-updater = false`).
