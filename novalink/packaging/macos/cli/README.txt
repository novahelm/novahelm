NovaLink for macOS (test build)

1. Unzip this folder anywhere (e.g. Applications or your home folder).
2. Right-click "Start NovaLink.command" > Open, then Open again. (Only the first
   time; macOS asks because the build is not signed by a registered developer.)
3. The first run asks for Accessibility and Input Monitoring. Allow both for
   Terminal in System Settings > Privacy & Security, then quit and re-run step 2.
4. Leave that window open. Double-click "NovaLink Terminal.command" for a window
   where `novalink` commands work, e.g. `novalink status`.

Pairing: `novalink discovered` lists nearby devices, `novalink pair <name>` asks one to pair,
and `novalink pair accept <id>` on the other machine accepts.
Logs: ~/Library/Logs/NovaLink/
