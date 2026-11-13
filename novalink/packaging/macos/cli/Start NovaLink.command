#!/bin/bash
# Double-click to run the NovaLink agent in this Terminal window. Close the window
# (or press Ctrl-C) to stop it; held keys are released on the way out.
cd "$(dirname "$0")" || exit 1
# Downloaded copies are quarantined; these are our own unsigned builds.
xattr -dr com.apple.quarantine . 2>/dev/null
echo "NovaLink agent starting. Leave this window open while you use NovaLink."
echo "To pair or check status, double-click 'NovaLink Terminal.command'."
echo
./novalink-agent "$@"
status=$?
echo
echo "NovaLink agent stopped (exit code $status). Logs: ~/Library/Logs/NovaLink/"
read -r -p "Press Return to close."
