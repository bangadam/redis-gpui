#!/usr/bin/env bash
# build-appimage.sh — Linux AppImage packaging script (PRD story 54).
#
# SCHEMA/FLOW: uses the standard linuxdeploy AppDir flow:
#   1. build the binary (caller/Adam runs this; the script assumes the
#      binary exists or builds it — see BIN_PATH below),
#   2. assemble an AppDir (AppRun + .desktop + icon),
#   3. run linuxdeploy AppImage with OUTPUT to produce the final
#      AppImage.
#
# ⚠️ THIS SCRIPT IS NEVER RUN BY AGENTS. Packaging builds are
# Adam-gated: run it yourself, after reading it, when you want an
# AppImage. `chmod +x` was applied at creation.
#
# Reference flow: https://github.com/linuxdeploy/linuxdeploy
# (docs.linuxdeploy: AppDir layout + linuxdeploy-x86_64.AppImage runs
# on Linux only — macOS agents cannot run this anyway.)
set -euo pipefail

APP_NAME="Redis GPUI"
APP_ID="dev.bangadam.redis-gpui"
VERSION="$(cargo metadata --format-version 1 --no-deps 2>/dev/null \
    | python3 -c 'import json,sys; m=json.load(sys.stdin); print([p["version"] for p in m["packages"] if p["name"]=="redis-gpui"][0])')"
TARGET="x86_64-unknown-linux-gnu"
BIN_PATH="target/${TARGET}/release/redis-gpui"
OUTPUT="dist/RedisGPUI-${VERSION}-x86_64.AppImage"

# 0. Sanity: the binary must exist (Adam runs the release build first).
if [[ ! -f "${BIN_PATH}" ]]; then
    echo "ERROR: ${BIN_PATH} missing. Run first:"
    echo "  cargo build --release --target ${TARGET}"
    echo "(Adam-gated: do not let an agent run this for you.)"
    exit 1
fi

# 1. AppDir skeleton.
APPDIR="build-appdir"
rm -rf "${APPDIR}"
mkdir -p "${APPDIR}/usr/bin" "${APPDIR}/usr/share/applications" "${APPDIR}/usr/share/icons/hicolor/512x512/apps"

install -m 0755 "${BIN_PATH}" "${APPDIR}/usr/bin/redis-gpui"

# AppRun: exec the binary (standard linuxdeploy pattern).
cat > "${APPDIR}/AppRun" <<'RUN'
#!/usr/bin/env bash
HERE="$(dirname "$(readlink -f "$0")")"
exec "${HERE}/usr/bin/redis-gpui" "$@"
RUN
chmod +x "${APPDIR}/AppRun"

# .desktop entry (icon references the installed icon name, no extension).
cat > "${APPDIR}/usr/share/applications/${APP_ID}.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=${APP_NAME}
Exec=redis-gpui
Icon=${APP_ID}
Terminal=false
Categories=Development;Database;
Comment=Write-safe, privacy-first Redis GUI
DESKTOP

# Icon: TODO — no icon file is committed (placeholder note in
# packaging/README.md). Until an icon exists, AppImage builds must
# supply one manually:
if [[ -f "packaging/icon-512.png" ]]; then
    install -m 0644 "packaging/icon-512.png" "${APPDIR}/usr/share/icons/hicolor/512x512/apps/${APP_ID}.png"
else
    echo "NOTE: packaging/icon-512.png missing (TODO); AppImage will lack an icon."
fi

# 2. linuxdeploy (download pinned revision; verify SHA at first use).
mkdir -p dist
LINUXDEPLOY="linuxdeploy-x86_64.AppImage"
if [[ ! -f "${LINUXDEPLOY}" ]]; then
    curl -L -o "${LINUXDEPLOY}" \
        "https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-x86_64.AppImage"
    chmod +x "${LINUXDEPLOY}"
fi

OUTPUT="${OUTPUT}" "${LINUXDEPLOY}" --appdir "${APPDIR}" --output appimage

mv Redis_GPUI*.AppImage "${OUTPUT}" 2>/dev/null || true
echo "built: ${OUTPUT}"
