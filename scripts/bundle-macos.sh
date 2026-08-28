#!/usr/bin/env bash
set -euo pipefail
# 组装 RustTunnel.app（ad-hoc 签名，不含公证）。
# 用法: VERSION=v0.8.0 GUI_BIN=target/.../rust-tunnel-client-gui scripts/bundle-macos.sh
VERSION="${VERSION:-${GITHUB_REF_NAME:-v0.0.0}}"
GUI_BIN="${GUI_BIN:-target/universal2/rust-tunnel-client-gui}"
APP="dist/RustTunnel.app"
ICON_SRC="crates/client-gui/icons/icon.png"

rm -rf dist/RustTunnel.app dist/*.dmg
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

cp "$GUI_BIN" "$APP/Contents/MacOS/rust-tunnel-client-gui"
chmod +x "$APP/Contents/MacOS/rust-tunnel-client-gui"

# icon.icns：优先用已有 icns，否则用 png 充当（ad-hoc 阶段可接受）
if [ -f "crates/client-gui/icons/icon.icns" ]; then
  cp "crates/client-gui/icons/icon.icns" "$APP/Contents/Resources/icon.icns"
elif [ -f "$ICON_SRC" ]; then
  cp "$ICON_SRC" "$APP/Contents/Resources/icon.png"
fi

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key><string>com.rust-tunnel.client</string>
  <key>CFBundleName</key><string>RustTunnel</string>
  <key>CFBundleDisplayName</key><string>Rust Tunnel</string>
  <key>CFBundleVersion</key><string>${VERSION#v}</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleExecutable</key><string>rust-tunnel-client-gui</string>
  <key>LSUIElement</key><true/>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST

echo "APPL????" > "$APP/Contents/PkgInfo"

if command -v codesign >/dev/null 2>&1; then
  codesign -s - --deep --force "$APP" || true
fi

# 产出 dmg（若 create-dmg/hdiutil 可用）
DMG="dist/RustTunnel-${VERSION}-macos-universal2.dmg"
if command -v create-dmg >/dev/null 2>&1; then
  create-dmg --volname "RustTunnel ${VERSION}" --window-size 540 380 --icon-size 96 \
    --app-drop-link 380 150 --icon RustTunnel.app 120 150 \
    "$DMG" "$APP" || true
elif command -v hdiutil >/dev/null 2>&1; then
  mkdir -p dist/dmg-staging && cp -R "$APP" dist/dmg-staging/ && ln -s /Applications dist/dmg-staging/Applications
  hdiutil create -volname "RustTunnel ${VERSION}" -srcfolder dist/dmg-staging -ov -format UDZO "$DMG" || true
  rm -rf dist/dmg-staging
fi

echo "bundle done: $APP${DMG:+ + $DMG}"
