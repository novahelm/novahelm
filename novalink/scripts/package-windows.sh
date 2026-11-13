#!/bin/bash
# Builds NovaLink-Setup.exe and the portable NovaLink-windows.zip into OUT_DIR.
# Usage: scripts/package-windows.sh OUT_DIR
# Cross-builds with mingw-w64 (x86_64-pc-windows-gnu). Honors CARGO_TARGET_DIR.
set -euo pipefail
out="$1"
repo="$(cd "$(dirname "$0")/.." && pwd)"
export CARGO_TARGET_DIR="${CARGO_TARGET_DIR:-$repo/target}"
target=x86_64-pc-windows-gnu
rel="$CARGO_TARGET_DIR/$target/release"

cd "$repo"
cargo build --release --target "$target" -p novalink-agent -p novalink-cli -p novalink-tray

stage="$out/NovaLink-windows"
rm -rf "$stage" "$out/NovaLink-windows.zip" "$out/NovaLink-Setup.exe"
mkdir -p "$stage"
cp "$rel/novalink-agent.exe" "$rel/novalink.exe" "$rel/novalink-tray.exe" "$stage/"
# Drop debug info from mingw builds (MSVC builds keep it in a separate .pdb anyway).
if command -v x86_64-w64-mingw32-strip >/dev/null; then
  x86_64-w64-mingw32-strip "$stage"/*.exe
fi

# The setup embeds the stripped executables, so build it last.
NOVALINK_SETUP_PAYLOAD="$stage" cargo build --release --target "$target" -p novalink-setup
cp "$rel/NovaLink-Setup.exe" "$out/NovaLink-Setup.exe"

cp "$repo"/packaging/windows/* "$stage/"
# cmd.exe expects CRLF line endings in batch files.
for f in "$stage"/*.bat; do
  perl -pi -e 's/\r?\n/\r\n/' "$f"
done
(cd "$out" && zip -qr NovaLink-windows.zip NovaLink-windows)
echo "$out/NovaLink-Setup.exe"
echo "$out/NovaLink-windows.zip"
