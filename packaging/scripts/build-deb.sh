#!/usr/bin/env bash
# build-deb.sh — Linux .deb package (PRD story 54; Ubuntu LTS x64).
# Adam-gated: NEVER run without Adam's explicit command.
#
# Flow: expects the linux x86_64 release binary in
# target/x86_64-unknown-linux-gnu/release/ (built by dist or by Adam);
# assembles the dpkg layout by hand (no cargo-deb dependency) and runs
# dpkg-deb. Run on a Linux host (macOS cannot run dpkg-deb).
set -euo pipefail

APP_NAME="Redis GPUI"
APP_ID="dev.bangadam.redis-gpui"
VERSION="$(cargo metadata --format-version 1 --no-deps 2>/dev/null \
    | python3 -c 'import json,sys; m=json.load(sys.stdin); print([p["version"] for p in m["packages"] if p["name"]=="redis-gpui"][0])')"
TARGET="x86_64-unknown-linux-gnu"
BIN="target/${TARGET}/release/redis-gpui"

if [[ ! -f "${BIN}" ]]; then
    echo "ERROR: ${BIN} missing — dist/cargo build --release --target ${TARGET} must run first."
    exit 1
fi

PKG="build-deb-pkg"
rm -rf "${PKG}"
mkdir -p "${PKG}/DEBIAN" \
    "${PKG}/usr/bin" \
    "${PKG}/usr/share/applications" \
    "${PKG}/usr/share/icons/hicolor/512x512/apps"

install -m 0755 "${BIN}" "${PKG}/usr/bin/redis-gpui"

cat > "${PKG}/usr/share/applications/${APP_ID}.desktop" <<DESKTOP
[Desktop Entry]
Type=Application
Name=${APP_NAME}
Exec=redis-gpui
Icon=${APP_ID}
Terminal=false
Categories=Development;Database;
Comment=Write-safe, privacy-first Redis GUI
DESKTOP

if [[ -f "packaging/icon-512.png" ]]; then
    install -m 0644 "packaging/icon-512.png" "${PKG}/usr/share/icons/hicolor/512x512/apps/${APP_ID}.png"
else
    echo "NOTE: packaging/icon-512.png missing (TODO); deb ships without an icon."
fi

mkdir -p "${PKG}/usr/share/doc/redis-gpui"
cat > "${PKG}/usr/share/doc/redis-gpui/copyright" <<COPYRIGHT
Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/
Upstream-Name: redis-gpui
Files: *
Copyright: 2026 Muhammad Meganata Adam
License: MIT
COPYRIGHT

cat > "${PKG}/DEBIAN/control" <<CONTROL
Package: redis-gpui
Version: ${VERSION}
Section: devel
Priority: optional
Architecture: amd64
Depends: libc6 (>= 2.35)
Maintainer: Muhammad Meganata Adam <bangadam@users.noreply.github.com>
Description: ${APP_NAME} — write-safe, privacy-first Redis GUI
 A personal, native Redis desktop client: read-only by default,
 explicit saves, byte-faithful values, no telemetry.
Homepage: https://github.com/bangadam/redis-gpui
CONTROL

mkdir -p dist
dpkg-deb --build "${PKG}" "dist/redis-gpui_${VERSION}_amd64.deb"
echo "built: dist/redis-gpui_${VERSION}_amd64.deb"
