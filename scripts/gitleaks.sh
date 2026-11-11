#!/usr/bin/env bash
# Runs the pinned gitleaks, downloading it first if this machine doesn't have it yet.
# Works on macOS, Linux and Windows (Git Bash). Nothing is installed: the program is kept in a cache folder.
#
#   scripts/gitleaks.sh dir . --config .gitleaks.toml --redact    # scan the files
#   scripts/gitleaks.sh git . --config .gitleaks.toml --redact    # scan the commits
#
# To move to a newer gitleaks, change VERSION and replace every checksum below with the values from that release's
# gitleaks_<version>_checksums.txt. A download that doesn't match its checksum is thrown away and never run.
set -euo pipefail

VERSION=8.30.1

case "$(uname -s)" in
  Darwin) os=darwin ;;
  Linux) os=linux ;;
  MINGW* | MSYS* | CYGWIN*) os=windows ;;
  *) echo "!! gitleaks.sh: unsupported system $(uname -s)" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  x86_64 | amd64) arch=x64 ;;
  arm64 | aarch64) arch=arm64 ;;
  *) echo "!! gitleaks.sh: unsupported processor $(uname -m)" >&2; exit 1 ;;
esac

case "${os}_${arch}" in
  darwin_arm64) sum=b40ab0ae55c505963e365f271a8d3846efbc170aa17f2607f13df610a9aeb6a5 ;;
  darwin_x64) sum=dfe101a4db2255fc85120ac7f3d25e4342c3c20cf749f2c20a18081af1952709 ;;
  linux_arm64) sum=e4a487ee7ccd7d3a7f7ec08657610aa3606637dab924210b3aee62570fb4b080 ;;
  linux_x64) sum=551f6fc83ea457d62a0d98237cbad105af8d557003051f41f3e7ca7b3f2470eb ;;
  windows_arm64) sum=b95f5e4f5c425cedca7ee203d9afd29597e692c4924a12ed42f970537c72cc0f ;;
  windows_x64) sum=d29144deff3a68aa93ced33dddf84b7fdc26070add4aa0f4513094c8332afc4e ;;
esac

if [ "$os" = windows ]; then ext=zip exe=gitleaks.exe; else ext=tar.gz exe=gitleaks; fi
cache="${GITLEAKS_CACHE:-${XDG_CACHE_HOME:-$HOME/.cache}/novahelm/gitleaks-$VERSION-${os}_$arch}"
bin="$cache/$exe"

sha256() {
  if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi
}

if [ ! -x "$bin" ]; then
  file="gitleaks_${VERSION}_${os}_${arch}.$ext"
  tmp="$(mktemp -d)"
  trap 'rm -rf "$tmp"' EXIT
  echo "==> Downloading gitleaks $VERSION ($os $arch)" >&2
  curl -sSfL -o "$tmp/$file" "https://github.com/gitleaks/gitleaks/releases/download/v$VERSION/$file"
  got="$(sha256 "$tmp/$file")"
  if [ "$got" != "$sum" ]; then
    echo "!! gitleaks.sh: $file has checksum $got, expected $sum. Not running it." >&2
    exit 1
  fi
  if [ "$ext" = zip ]; then unzip -q "$tmp/$file" "$exe" -d "$tmp"; else tar -xzf "$tmp/$file" -C "$tmp" "$exe"; fi
  mkdir -p "$cache"
  mv "$tmp/$exe" "$bin"
  chmod +x "$bin"
  trap - EXIT
  rm -rf "$tmp"
fi

exec "$bin" "$@"
