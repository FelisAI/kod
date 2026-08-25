#!/bin/sh
# Assemble Kod.app (dev bundle) so macOS shows the app icon in the dock.
# Copies ALL THREE binaries. Both helpers are resolved as SIBLINGS of the
# executable that starts them: the GUI finds orchestrator-daemon next to itself,
# and the daemon finds kod-bridge next to ITSELF (daemon bridge.rs
# bridge_binary()). Miss kod-bridge and Settings -> Mobile fails only on a real
# install, never in a dev build, which is the worst way to find out.
set -e
cd "$(dirname "$0")/.."
PROFILE="${1:-debug}"
BINS="-p orchestrator-gui -p orchestrator-daemon -p orchestrator-bridge"
if [ "$PROFILE" = "release" ]; then cargo build --release $BINS
else cargo build $BINS; fi
APP="../Kod.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "target/$PROFILE/orchestrator" "$APP/Contents/MacOS/kod"
cp "target/$PROFILE/orchestrator-daemon" "$APP/Contents/MacOS/orchestrator-daemon"
cp "target/$PROFILE/kod-bridge" "$APP/Contents/MacOS/kod-bridge"
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
  <key>CFBundleShortVersionString</key><string>0.2.0</string>
  <key>CFBundleVersion</key><string>2</string>
  <key>LSMinimumSystemVersion</key><string>13.0</string>
  <key>NSHighResolutionCapable</key><true/>
</dict>
</plist>
PLIST
echo "assembled: $(cd "$APP" && pwd)"
echo "launch:    open ../Kod.app"
