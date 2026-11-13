#!/bin/zsh
# Builds and signs NovaLink.app (menu-bar app with the agent embedded as a LaunchAgent) and a dmg.
# Usage: scripts/build-mac-app.sh OUT_DIR
# This is a full release build. Set NOVALINK_BUILD_ROOT to keep build output somewhere other than OUT_DIR/build.
set -euo pipefail

OUT="${1:?usage: build-mac-app.sh OUT_DIR}"
REPO="${0:A:h:h}"
BUILD_ROOT="${NOVALINK_BUILD_ROOT:-$OUT/build}"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$BUILD_ROOT/target}"
SCRATCH="$BUILD_ROOT/swift-build"
APP="$OUT/NovaLink.app"
DMG="$OUT/NovaLink-macos.dmg"
IDENTITY="NovaLink Local Signing"
KC="$HOME/Library/Application Support/NovaLink/signing/signing.keychain-db"
VERSION="$(sed -nE '/^\[workspace\.package\]/,/^\[/ s/^version *= *"(.*)"/\1/p' "$REPO/Cargo.toml" | head -1)"
[[ -n "$VERSION" ]] || { echo "!! No version found in Cargo.toml" >&2; exit 1; }

"$REPO/scripts/setup-signing.sh"
security unlock-keychain -p "$(security find-generic-password -s com.novalink.signing -a keychain -w)" "$KC"

echo "==> Building agent and CLI (release)"
(cd "$REPO" && cargo build --release -p novalink-agent -p novalink-cli)
RUST_BIN="$CARGO_TARGET_DIR/release"

echo "==> Building app (release)"
swift build -c release --package-path "$REPO/apple" --product NovaLinkApp --scratch-path "$SCRATCH"
SWIFT_BIN="$(swift build -c release --package-path "$REPO/apple" --scratch-path "$SCRATCH" --show-bin-path)"

echo "==> Assembling $APP"
mkdir -p "$OUT"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Helpers" "$APP/Contents/Library/LaunchAgents" "$APP/Contents/Resources"
cp "$SWIFT_BIN/NovaLinkApp" "$APP/Contents/MacOS/NovaLink"
cp "${REPO:h}/brand/icons/novalink.icns" "$APP/Contents/Resources/AppIcon.icns"
cp "$RUST_BIN/novalink-agent" "$APP/Contents/Helpers/novalink-agent"
cp "$RUST_BIN/novalink" "$APP/Contents/Helpers/novalink"
cp "$REPO/packaging/macos/com.novalink.agent.plist" "$APP/Contents/Library/LaunchAgents/com.novalink.agent.plist"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key><string>com.novalink.app</string>
  <key>CFBundleName</key><string>NovaLink</string>
  <key>CFBundleDisplayName</key><string>NovaLink</string>
  <key>CFBundleExecutable</key><string>NovaLink</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>LSMinimumSystemVersion</key><string>15.0</string>
  <key>LSUIElement</key><true/>
  <key>NSAppTransportSecurity</key><dict><key>NSAllowsLocalNetworking</key><true/></dict>
  <key>NSLocalNetworkUsageDescription</key><string>NovaLink finds and connects to your other computers on your home network.</string>
  <key>NSBonjourServices</key><array><string>_novalink._udp</string></array>
</dict>
</plist>
PLIST
plutil -lint "$APP/Contents/Info.plist" >/dev/null
plutil -lint "$APP/Contents/Library/LaunchAgents/com.novalink.agent.plist" >/dev/null

echo "==> Signing with \"$IDENTITY\""
sign() { codesign --force --options runtime --timestamp=none --keychain "$KC" --sign "$IDENTITY" "$@"; }
sign --identifier com.novalink.agent "$APP/Contents/Helpers/novalink-agent"
sign --identifier com.novalink.cli "$APP/Contents/Helpers/novalink"
sign --identifier com.novalink.app "$APP"
codesign --verify --strict --deep "$APP"
codesign -d -r- "$APP/Contents/Helpers/novalink-agent" 2>&1 | grep designated
# The agent runs outside the bundle under launchd; macOS reads its Local Network reason
# from the Info.plist embedded in the binary (crates/agent/build.rs).
launchctl plist __TEXT,__info_plist "$APP/Contents/Helpers/novalink-agent" | grep -q NSLocalNetworkUsageDescription \
  || { echo "!! novalink-agent has no embedded Info.plist" >&2; exit 1; }

echo "==> Creating dmg"
STAGE="$(mktemp -d)"; trap 'rm -rf "$STAGE" "$STAGE.rw.dmg"' EXIT
ditto "$APP" "$STAGE/NovaLink.app"
ln -s /Applications "$STAGE/Applications"
cp "${REPO:h}/brand/icons/novalink.icns" "$STAGE/.VolumeIcon.icns"
rm -f "$DMG"
# Made writable first so the mounted volume can be flagged to show its icon, then compressed.
hdiutil create -volname NovaLink -srcfolder "$STAGE" -ov -format UDRW "$STAGE.rw.dmg" >/dev/null
MOUNT="$(hdiutil attach -nobrowse -noautoopen "$STAGE.rw.dmg" | awk -F'\t' '/\/Volumes\// { print $NF }')"
SetFile -a C "$MOUNT"
hdiutil detach "$MOUNT" >/dev/null
hdiutil convert "$STAGE.rw.dmg" -format UDZO -o "$DMG" >/dev/null
rm -f "$STAGE.rw.dmg"
echo "==> Built $APP ($VERSION)"
echo "$APP"
echo "$DMG"
