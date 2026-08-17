#!/bin/sh
# Assemble Kod.app (dev bundle) so macOS shows the app icon in the dock.
# Copies BOTH binaries: the GUI resolves orchestrator-daemon as a SIBLING
# of its own executable (daemon lib.rs daemon_binary()).
set -e
cd "$(dirname "$0")/.."
PROFILE="${1:-debug}"
if [ "$PROFILE" = "release" ]; then cargo build --release -p orchestrator-gui -p orchestrator-daemon
else cargo build -p orchestrator-gui -p orchestrator-daemon; fi
APP="../Kod.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "target/$PROFILE/orchestrator" "$APP/Contents/MacOS/kod"
cp "target/$PROFILE/orchestrator-daemon" "$APP/Contents/MacOS/orchestrator-daemon"
if [ -f assets/kod.icns ]; then
  cp assets/kod.icns "$APP/Contents/Resources/kod.icns"
else
  echo "warning: assets/kod.icns not found — building Kod.app without a dock icon" >&2
fi
cat > "$APP/Contents/Info.plist" <<'PLIST'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleName</key><string>Kod</string>
  <key>CFBundleDisplayName</key><string>Kod</string>
  <key>CFBundleIdentifier</key><string>ai.felis.kod</string>
  <key>CFBundleExecutable</key><string>kod</string>
  <key>CFBundleIconFile</key><string>kod</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleShortVersionString</key><string>0.1.0</string>
  <key>CFBundleVersion</key><string>1</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST
echo "assembled: $(cd "$APP" && pwd)"
echo "launch:    open ../Kod.app"
