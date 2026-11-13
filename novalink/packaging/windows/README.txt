NovaLink for Windows (test build)

This folder is the portable copy (see "Portable" below). NovaLink-Setup.exe, which
installs NovaLink with a tray icon, is a separate download next to this zip.

Run NovaLink-Setup.exe

1. Before running it: right-click NovaLink-Setup.exe > Properties > tick "Unblock" > OK.
   (Otherwise Windows warns, because the build is not signed.)
2. Double-click NovaLink-Setup.exe. If SmartScreen appears: More info > Run anyway.
3. When Windows Firewall asks, allow access on Private networks, and make sure your
   home network is set to Private (Settings > Network & internet > your network).
4. Look for the NovaLink icon in the system tray (near the clock; it may be under the
   ^ arrow). Use it to pair devices and to turn NovaLink off.

Portable (no install)

This folder, from NovaLink-windows.zip, runs NovaLink without installing it.

1. Before unzipping: right-click the .zip > Properties > tick "Unblock" > OK.
2. Unzip this folder anywhere (e.g. your Documents folder).
3. Double-click "Start NovaLink.bat". If SmartScreen appears: More info > Run anyway.
4. Allow Private networks in the firewall prompt, as above.
5. Leave that window open. Double-click "NovaLink Terminal.bat" for a window where
   `novalink` commands work, e.g. `novalink status`.

Pairing and the full checklist: docs/gate/plan-1-gate.md in the repo.
Logs: %LOCALAPPDATA%\NovaLink\logs\
