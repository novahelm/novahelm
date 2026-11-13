#!/bin/bash
# Opens a shell where the `novalink` command works (pair, status, layout, metrics...).
cd "$(dirname "$0")" || exit 1
xattr -dr com.apple.quarantine . 2>/dev/null
export PATH="$PWD:$PATH"
echo "NovaLink commands are ready. Try:"
echo "  novalink status"
echo "  novalink discovered"
echo "  novalink pair <name>        then  novalink pair pending  /  novalink pair accept <id>"
echo "  novalink layout right <name>"
echo "  novalink doctor"
echo
exec "${SHELL:-/bin/zsh}" -i
