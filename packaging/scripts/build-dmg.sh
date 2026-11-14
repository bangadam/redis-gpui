#!/usr/bin/env bash
# build-dmg.sh — macOS .dmg artifact (called by dist's extra-artifacts,
# or by hand). Adam-gated: NEVER run this without Adam's explicit
# command (equivalent to a release build).
#
# Flow: expects cargo-dist to have built the release binaries for both
# macOS targets into target/; assembles a .app bundle for the current
# arch (run once on arm64, once on x86_64 hardware) and produces a
# compressed, internet-enabled dmg via hdiutil (stock macOS tooling;
# no create-dmg dependency).
set -euo pipefail

APP_NAME="Redis GPUI"
APP_BUNDLE="Redis GPUI.app"
APP_ID="dev.bangadam.redis-gpui"
VERSION="$(cargo metadata --format-version 1 --no-deps 2>/dev/null \
    | python3 -c 'import json,sys; m=json.load(sys.stdin); print([p["version"] for p in m["packages"] if p["name"]=="redis-gpui"][0])')"
ARCH="$(uname -m)"
# dist builds land in target/<triple>/; pick the host arch's triple.
TRIPLE="$( [[ "${ARCH}" == "arm64" ]] && echo aarch64-apple-darwin || echo x86_64-apple-darwin )"
BIN="target/${TRIPLE}/release/redis-gpui"

if [[ ! -f "${BIN}" ]]; then
    echo "ERROR: ${BIN} missing — dist/cargo build --release must run first."
    exit 1
fi

mkdir -p "build-dmg/${APP_BUNDLE}/Contents/MacOS" "build-dmg/${APP_BUNDLE}/Contents/Resources"
install -m 0755 "${BIN}" "build-dmg/${APP_BUNDLE}/Contents/MacOS/redis-gpui"

# Info.plist — minimal bundle descriptor. Unsigned (PRD: first release
# ships unsigned with clear instructions).
cat > "build-dmg/${APP_BUNDLE}/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>${APP_NAME}</string>
    <key>CFBundleIdentifier</key><string>${APP_ID}</string>
    <key>CFBundleExecutable</key><string>redis-gpui</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>CFBundleShortVersionString</key><string>${VERSION}</string>
    <key>CFBundleVersion</key><string>${VERSION}</string>
    <key>LSMinimumSystemVersion</key><string>13.0</string>
    <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

# Icon: TODO — assets/icon.svg exists but macOS bundles need a proper
# .icns. See packaging/README.md (no fabricated binaries).
if [[ -f "packaging/icon.icns" ]]; then
    install -m 0644 "packaging/icon.icns" "build-dmg/${APP_BUNDLE}/Contents/Resources/AppIcon.icns"
    /usr/libexec/PlistBuddy -c "Add :CFBundleIconFile string AppIcon" \
        "build-dmg/${APP_BUNDLE}/Contents/Info.plist"
else
    echo "NOTE: packaging/icon.icns missing (TODO in packaging/README.md); bundle ships without an icon."
fi

mkdir -p dist
DMG="dist/RedisGPUI-${VERSION}-${ARCH}.dmg"
rm -f "${DMG}"
hdiutil create -volname "${APP_NAME}" -srcfolder "build-dmg/${APP_BUNDLE}" \
    -ov -format UDZO "${DMG}"
echo "built: ${DMG}"
