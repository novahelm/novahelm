#!/bin/zsh
# Builds NovaGear.app (the SwiftUI menu bar app with the Rust agent embedded).
# Output goes to novagear/target/mac, or to NOVAGEAR_OUT if you set it (for example a build drive).
# Signs ad hoc by default; set NOVAGEAR_IDENTITY to a codesigning identity to sign with it.
# Use a real identity for a copy you keep: macOS ties Accessibility permission to the
# signature, and an ad hoc one changes on every build, so the permission would not survive.
set -euo pipefail

REPO="${0:A:h:h}"
if [[ -n "${NOVAGEAR_OUT:-}" ]]; then
  OUT="$NOVAGEAR_OUT"
  # A set output whose parent is missing is usually an unplugged drive: stop rather than fill another disk.
  [[ -d "${OUT:h}" ]] || { echo "!! ${OUT:h} is missing; not building." >&2; exit 1; }
else
  OUT="$REPO/target/mac"
fi
APP="$OUT/NovaGear.app"
VERSION="$(grep -m1 -E '^version' "$REPO/Cargo.toml" | sed -E 's/.*"(.*)".*/\1/')"

echo "==> Building the agent (release)"
CARGO_TARGET_DIR="$OUT" cargo build --release --manifest-path "$REPO/Cargo.toml" -p novagear-agent -p novagear-cli

echo "==> Building the menu bar app (release)"
swift build -c release --package-path "$REPO/apple" --scratch-path "$OUT/swift" --product NovaGearApp
BIN="$(swift build -c release --package-path "$REPO/apple" --scratch-path "$OUT/swift" --show-bin-path)"

echo "==> Assembling $APP"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Helpers" "$APP/Contents/Resources"
cp "$BIN/NovaGearApp" "$APP/Contents/MacOS/NovaGear"
cp "${REPO:h}/brand/icons/novagear.icns" "$APP/Contents/Resources/AppIcon.icns"
cp "$OUT/release/novagear-agent" "$OUT/release/novagear" "$APP/Contents/Helpers/"
cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key><string>com.novahelm.novagear</string>
  <key>CFBundleName</key><string>NovaGear</string>
  <key>CFBundleDisplayName</key><string>NovaGear</string>
  <key>CFBundleExecutable</key><string>NovaGear</string>
  <key>CFBundlePackageType</key><string>APPL</string>
  <key>CFBundleIconFile</key><string>AppIcon</string>
  <key>CFBundleShortVersionString</key><string>$VERSION</string>
  <key>CFBundleVersion</key><string>$VERSION</string>
  <key>LSMinimumSystemVersion</key><string>15.0</string>
  <key>LSUIElement</key><true/>
  <key>LSApplicationCategoryType</key><string>public.app-category.utilities</string>
  <key>NSLocalNetworkUsageDescription</key><string>NovaGear finds and controls your Elgato Key Lights on your local network.</string>
  <key>NSBonjourServices</key><array><string>_elg._tcp</string></array>
  <key>NSHumanReadableCopyright</key><string>Lights, Stream Deck, prompter, capture and mice in one small app</string>
</dict>
</plist>
PLIST
plutil -lint "$APP/Contents/Info.plist" >/dev/null

IDENTITY="${NOVAGEAR_IDENTITY:--}"
echo "==> Signing (${IDENTITY/#-/ad hoc})"
sign() { codesign --force --options runtime --timestamp=none --sign "$IDENTITY" "$@"; }
sign --identifier com.novahelm.novagear.agent "$APP/Contents/Helpers/novagear-agent"
sign --identifier com.novahelm.novagear.cli "$APP/Contents/Helpers/novagear"
sign --identifier com.novahelm.novagear "$APP"
codesign --verify --strict --deep "$APP"
echo "==> Built $APP ($VERSION)"
