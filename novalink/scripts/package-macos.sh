#!/bin/bash
# Bundles release binaries into dist/NovaLink-macos.zip.
# Usage: scripts/package-macos.sh OUT_DIR BIN_DIR [BIN_DIR...]
# With several BIN_DIRs (one per architecture) the binaries are merged with lipo.
set -euo pipefail
out="$1"
shift
repo="$(cd "$(dirname "$0")/.." && pwd)"
stage="$out/NovaLink-macos"
rm -rf "$stage" "$out/NovaLink-macos.zip"
mkdir -p "$stage"
for bin in novalink-agent novalink; do
  if [ "$#" -gt 1 ]; then
    lipo -create $(for d in "$@"; do echo "$d/$bin"; done) -output "$stage/$bin"
  else
    cp "$1/$bin" "$stage/$bin"
  fi
  # Ad-hoc signature: required on Apple silicon, and lipo output needs a fresh one.
  codesign --force --sign - "$stage/$bin"
done
cp "$repo"/packaging/macos/cli/* "$stage/"
chmod +x "$stage"/*.command
(cd "$out" && ditto -c -k --keepParent NovaLink-macos NovaLink-macos.zip)
echo "$out/NovaLink-macos.zip"
