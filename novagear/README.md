# NovaGear

One small app for creator gear: Elgato Key Lights, Stream Deck +, the Elgato Prompter, Elgato capture cards and Razer
mice. It stands in for the separate vendor apps (Elgato Control Center, Stream Deck, Camera Hub, Razer Synapse) for
day-to-day control, in about 5 MB of memory and a quarter of a percent of one core at idle.

NovaGear is part of [NovaHelm](../README.md). It is free and open source under the
[Apache License 2.0](LICENSE).

> **Our licence promise.** NovaGear is Apache-2.0 and will stay Apache-2.0. We won't move it to a "source available"
> licence, and we don't ask contributors to sign a CLA that would let us relicense their work: contributions come in
> under the same licence with a DCO sign-off (see [CONTRIBUTING.md](../CONTRIBUTING.md)).

## Supported platforms and devices

- **macOS 15 or later:** a SwiftUI menu bar app over the Rust agent. This is the main, tested build.
- **Windows and Linux (including SteamOS):** the Rust tray builds and runs, but it is a preview and not yet packaged.

| Device                    | Status                                                                                                              |
| ------------------------- | ------------------------------------------------------------------------------------------------------------------- |
| Key Light                 | found by mDNS; on/off, brightness, warmth, identify. Verified live.                                                 |
| Razer Basilisk V3         | reads and sets DPI, static colour, brightness (interface 3). DPI read verified live.                                |
| Elgato Prompter           | teleprompter window driven by the agent; Chrome/Edge put it on the Prompter screen with F.                          |
| Elgato 4K X / Cam Link 4K | listed; Preview in the panel uses the browser's camera API.                                                         |
| Stream Deck +             | keys, dials, touch strip, bindings. Verified live. Needs the Elgato Stream Deck app quit (it holds it exclusively). |
| 8BitDo Retro Keyboard     | listed only.                                                                                                        |

The Stream Deck stays read-only until you click "Let NovaGear drive the deck" in the panel. More devices are
listed in `crates/drivers/devices/`. To add a device, see [crates/drivers/devices/README.md](crates/drivers/devices/README.md):
most need only a few lines of TOML.

## Build and run

You need [Rust](https://rustup.rs) 1.85 or later (the workspace uses edition 2024). The Mac app also needs Xcode 16 or
later (Swift 6) on macOS 15. Build output goes to `novagear/target/`; set `CARGO_TARGET_DIR` (Rust) or `NOVAGEAR_OUT`
(the Mac app script) to put it somewhere else.

### The Rust agent, CLI and tray (any platform)

```bash
cd novagear
cargo build --release
./target/release/novagear-tray &     # the tray starts the agent next to it
./target/release/novagear devices
./target/release/novagear open       # the full control panel in your browser
cargo test --workspace               # run the tests
```

On Linux, `hidapi` needs `libudev` (`libudev-dev` on Debian and Ubuntu), and your user needs udev rules that let it open
the Stream Deck and the mouse.

### The Mac app

```bash
novagear/scripts/build-mac-app.sh               # builds novagear/target/mac/NovaGear.app, signed ad hoc
open novagear/target/mac/NovaGear.app
```

The app's Swift package uses NovaUI from `../novaui` and its icon from `../brand/icons`, so build it from a full
checkout of the repository. An ad hoc signature changes on every build, so macOS asks for Accessibility permission
again after each one. To keep the permission, sign with your own identity:
`NOVAGEAR_IDENTITY="Developer ID Application: Your Name (TEAMID)" novagear/scripts/build-mac-app.sh`.

Either front end starts `novagear-agent` if none is running and stops it on Quit; an agent you started yourself is left
running. Both have "Open NovaGear panel", start at login, and live state from the agent's event stream.

CLI examples: `novagear light toggle`, `novagear light set --brightness 40 --kelvin 4800`, `novagear prompter play`,
`novagear dpi 1600`, `novagear action '{"do":"light_kelvin_step","step":-200}'`.

## What it does

### Stream Deck pages and actions

The deck has named pages (8 keys, 4 dials each), with keys to switch pages and go back. Keys and dials can:

- open an app or website, type text, or press a hotkey (macOS needs Accessibility permission for NovaGear);
- change output or mic volume and mute;
- switch OBS scenes and toggle recording and streaming (obs-websocket; set the address and password in the panel);
- control the lights, prompter, mouse DPI and deck brightness.

`novagear import-streamdeck` (or "Import from the Stream Deck app" in the panel) turns the Elgato app's profiles,
folders and key icons into pages. Actions NovaGear can't run yet stay as grey placeholder keys.

### Modes

A mode is a named deck setup: an icon, an accent colour, its pages (the first is home) and four strip panels. The mode
key opens a picker on the strip (tap a segment or press its knob; nothing picked in 5 s closes it), or cycles, per
`mode_key` in the config. `mode_rules` switch modes on their own (`obs_recording`, `obs_streaming`,
`app_frontmost`); a rule's mode shows while its condition holds, a mode picked by hand wins until it ends, and the
deck then returns to where it was. The current mode and page survive restarts. An old config with top-level `pages`
loads as one mode, "Main". Keys can run a sequence of actions with delays, toggle between two states, or need a hold
(`hold_ms`). The deck sleeps after `deck_sleep_min` idle minutes and wakes on any input.

Strip panels are `value`, `gauge`, `bar`, `meter`, `choice`, `dots`, `sparkline` and `message`. A `choice` panel lights
the option whose label equals its feed's text, ignoring case. A panel's knob belongs to it; a `choice` panel with
`commit: press` previews on turn and acts on press.

### Feeds API

A feed is a named value that key faces and strip panels show (`system.mem.pct`, `obs.record.time`,
`novaguard.builds.running`). Built-in feeds run only while something visible reads them. Any local program can push
its own. The examples go to `127.0.0.1:47820` with a bearer token (the agent token, or a client token, see below):

```bash
TOKEN=ngc_...   # a client token; the main agent token works too
curl -s -X PUT http://127.0.0.1:47820/api/feeds/novaguard.builds.running \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"value": 2, "state": "ok", "ttl_s": 120}'
curl -s -X DELETE http://127.0.0.1:47820/api/feeds/novaguard.builds.running -H "Authorization: Bearer $TOKEN"
curl -s http://127.0.0.1:47820/api/feeds -H "Authorization: Bearer $TOKEN"
```

The body is `{value, unit?, state?, ttl_s?}`: `value` is a number, text or a short list, `state` is `ok`, `warn` or
`bad`, `ttl_s` is 1-86400 (default 120). A value not refreshed within its ttl shows as stale (a dash). A client may
push only keys that start with its own name and a dot, and the body is capped at 4 KB. `novagear feed set <key> <value>
[--state ok]` and `novagear feed clear <key>` do the same from the CLI.

### Approvals API

A local program asks for a yes or no and the deck shows it in a mode that allows pop-ups (Development in the shipped
template). **Only the deck approves**: hold Approve for 1 second. Deny is one tap. There is no approve endpoint, and
no API call, panel session or popover button can say yes; the panel and the Mac popover can only show requests and
Deny them.

```bash
# Ask (body up to 8 KB; lines up to 3; risk low|high; expires_in_s default 600, max 3600)
curl -s -X POST http://127.0.0.1:47820/api/approvals \
  -H "Authorization: Bearer $TOKEN" -H 'Content-Type: application/json' \
  -d '{"title": "Deploy web to production?", "lines": ["build 412", "3 files changed"],
       "source": "novaguard", "risk": "high", "details_url": "https://example.com/run/412"}'
# -> {"id": "..."}

# Wait for the answer (long poll, up to 60 s): approved, denied, expired or cancelled
curl -s "http://127.0.0.1:47820/api/approvals/$ID?wait=60" -H "Authorization: Bearer $TOKEN"

# Withdraw your own request
curl -s -X DELETE http://127.0.0.1:47820/api/approvals/$ID -H "Authorization: Bearer $TOKEN"

# Deny (for the panel and popover: $AGENT_TOKEN is the agent's own token, not a client's; a client token is refused)
curl -s -X POST http://127.0.0.1:47820/api/approvals/$ID/deny -H "Authorization: Bearer $AGENT_TOKEN"
```

`details_url` opens only when it is `https`. Requests live in memory; an agent restart ends them as `cancelled`. A
press made before the pop-up appeared or within 300 ms of it never counts. The log records only the id, source, risk
and outcome, never the title or lines. To try it without a program: `novagear approve-test [--title ...] [--risk high]`
sends one as a temporary client and waits for your answer.

### Client tokens and named events

`novagear client add <name>` prints a token once (`ngc_...`); only its hash is stored in the config dir.
`novagear client list` and `novagear client remove <name>` manage them, and the panel lists and revokes them. A client
token may push feeds under its own prefix, create, read and cancel its own approvals, read `/api/status`, and
receive its own named events. It cannot call `/api/action`, `/api/config`, `/api/logs` or anything else. Rate caps:
feeds 10/s, approvals 1 per 5 s and 20 waiting.

A key bound to `{"do": "client_event", "client": "novaguard", "name": "pause_all"}` sends one named event to that
client only. The client reads them from its stream and decides what they mean:

```bash
curl -N http://127.0.0.1:47820/api/clients/me/events -H "Authorization: Bearer $TOKEN"
```

### The Unix socket (NovaGuard's channel)

The agent also serves the same API, with the same tokens, on a Unix socket: `~/Library/Application Support/NovaGear/agent.sock`
(`agent.sock` in the config dir), mode 0600. The Host-header check doesn't apply there. **NovaGuard uses only the socket** (not
port 47820), and checks the running program on it before sending anything: identifier `com.novahelm.novagear.agent`, the same
signing certificate as NovaGuard, hardened runtime on. So NovaGear must be built with `NOVAGEAR_IDENTITY` (a real team
identity) for NovaGuard's feeds, deck approvals and Pause all to work; an ad hoc build is ignored. The TCP port stays for the
panel, the CLI and other clients.

NovaGuard now uses this for waiting-ship and deploy approvals, the Pause all key (client `novaguard`, feed
`novaguard.paused`) and its feeds. NovaGuard is a separate app and is not part of this repository; NovaGear works
without it, and the Pause all key then does nothing.

### Templates

`GET /api/templates` lists the built-in templates; `POST /api/templates/{name}/apply[?replace_rules=true]` adds their
modes next to yours (a clashing id or name becomes `dev-2` / "Development 2"; nothing is deleted) and says what is left
to pick. "Developer + Creator" is the maintainer's own Development, Create and Live setup. A value written `<pick:scene>`,
`<pick:mic>` or `<pick:game>` is a blank for an OBS scene or audio input, chosen in the panel. A choice panel lights the option whose label equals its feed's text (case-insensitive), so the
Autonomy panel's feed `nova.autonomy` should say Ask, Safe or Full. NovaGuard pushes the `novaguard.*` sessions, builds, deploy and machine feeds. Feeds nothing pushes (`nova.*`, `ship.*`,
`live.*`) show a dash. `PUT /api/images` (raw bytes, up to 20 MB) stores a picture as
`images/<sha256>.<ext>` and returns `{name}`. `GET /api/images/{name}` (the panel and the app only; a client token is
refused) returns a picture the deck can draw, found the way the deck finds it: your `images/`, the old `icons/`, then
the bundled set (`lucide%2Fhand.svg`). It is sent with `nosniff` and a `sandbox` CSP, and the panel shows it from a blob
URL fetched with its Authorization header, so no token is ever part of an image URL.

## Device records, logs and firmware

Every device the agent lists comes from a record: a vendor, a model, how to recognise it (USB vid:pid) and which driver
family handles it. The built-in records ship in the agent. To add or override one, create `devices.toml` beside
`config.json` (`~/Library/Application Support/NovaGear/` on macOS); mistakes in it are reported in `/api/status` and
shown as a banner in the panel instead of stopping the agent. A USB device with no record is still listed, with a
"Copy device details" button in the panel that copies its vendor, product and interface details and a draft record
for `devices.toml` (never a serial).

The agent writes a rotating JSON log: `~/Library/Logs/NovaGear` on macOS, `%LOCALAPPDATA%\NovaGear\Logs` on Windows and
`~/.local/state/novagear` on Linux. Read it with `novagear logs [--level warn] [-q text] [-n 200]` or the Logs tab in the panel, which can
follow live and "Copy for a bug report" (the last 500 lines, version, OS and a device list without serials).

Firmware: `novagear devices` and the panel show each device's firmware. Key Lights are checked once a day against
Elgato's public update feed (turn it off with "Check for firmware updates" in the panel or `firmware_check` in
`config.json`); other devices point to their vendor's updater.

## Layout

- `crates/core`: device types, actions, bindings, config (`~/Library/Application Support/NovaGear/config.json`).
- `crates/drivers`: `keylight`, `streamdeck`, `razer`, `catalog` (USB listing), `draw` (key images).
- `crates/agent`: `novagear-agent`, the local API on `127.0.0.1:47820` and on the 0600 socket `agent.sock` (token in the config dir) and the panel pages in
  `panel/`.
- `crates/cli`: `novagear`.
- `apple/`: `NovaGear.app`, the Mac menu bar app (NovaGearKit: agent client and menu text; NovaGearApp: the popover).
- `crates/tray`: `novagear-tray` (`tray-icon` + `tao`, the same tray stack as NovaLink).

## Adding a Razer model

On macOS a feature report sent to the wrong USB interface makes the mouse re-enumerate, so the driver only talks to
interfaces listed in `control_interface` (`crates/drivers/src/razer.rs`). Find a new model's interface with
`cargo run -p novagear-drivers --example razer_probe <pid>`.

## Contributing, security and licence

- Contributions are welcome: see [CONTRIBUTING.md](../CONTRIBUTING.md). Every commit needs a `Signed-off-by` line.
- Found a security problem? Please don't open a public issue; see [SECURITY.md](../SECURITY.md).
- NovaGear is licensed under the [Apache License 2.0](LICENSE). The bundled fonts and icons keep their own licences,
  listed in [NOTICE](NOTICE). The Nova names and logos are not covered by the licence.
