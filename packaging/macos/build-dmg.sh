#!/usr/bin/env bash
# Build "Android Monitor.app" and a .dmg  ->  dist/      (run on macOS)
# Usage: packaging/macos/build-dmg.sh [version]
set -euo pipefail
cd "$(dirname "$0")/../.."

VERSION="${1:-$(grep -m1 '^version' Cargo.toml | cut -d'"' -f2)}"
ARCH="$(uname -m)"; [ "$ARCH" = "x86_64" ] && ARCH=x64
APP="dist/Android Monitor.app"
DMG="dist/android-monitor-${VERSION}-macos-${ARCH}.dmg"

cargo build --release

rm -rf "$APP" "$DMG"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp target/release/android-monitor "$APP/Contents/MacOS/android-monitor"
cp assets/icon.icns "$APP/Contents/Resources/icon.icns"

cat > "$APP/Contents/Info.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Android Monitor</string>
  <key>CFBundleDisplayName</key><string>Android Monitor</string>
  <key>CFBundleIdentifier</key><string>io.github.android-monitor</string>
  <key>CFBundleVersion</key><string>${VERSION}</string>
  <key>CFBundleShortVersionString</key><string>${VERSION}</string>
  <key>CFBundleExecutable</key><string>android-monitor</string>
  <key>CFBundleIconFile</key><string>icon</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>LSMinimumSystemVersion</key><string>12.0</string>
  <key>NSHighResolutionCapable</key><true/>
  <key>LSApplicationCategoryType</key><string>public.app-category.developer-tools</string>
</dict>
</plist>
EOF

# Ad-hoc signature so Apple Silicon will run it (not notarised).
codesign --force --deep --sign - "$APP"

# .dmg with an /Applications shortcut
STAGE="$(mktemp -d)"
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
hdiutil create -volname "Android Monitor" -srcfolder "$STAGE" -ov -format UDZO "$DMG" >/dev/null
rm -rf "$STAGE"

ls -lh "$DMG"
