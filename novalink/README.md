# NovaLink

Use every computer on your desk as if each were another monitor. Push the cursor off the edge of one screen and your
keyboard and mouse drive the next machine. There is no fixed main machine: whichever one you touched last is in
control.

NovaLink is early. It runs on macOS and Windows, the builds are unsigned test builds, and it has not had a security
review by anyone outside the project. It sends your keyboard and mouse input between your own machines, so only pair
devices you own, on a network you trust.

## What is here

| Folder                        | What it is                                                                     |
| ----------------------------- | ------------------------------------------------------------------------------ |
| `crates/agent`                | The background agent: discovery, pairing, the encrypted link and input handoff |
| `crates/cli`                  | `novalink`, the command-line tool that talks to the agent on this machine      |
| `crates/client`               | The small library the CLI and the apps use to reach the agent                  |
| `crates/core`                 | Layout, edges and who-is-in-control rules, with no system calls                |
| `crates/net`                  | Device identity, pairing and the QUIC transport                                |
| `crates/proto`                | The messages devices send each other                                           |
| `crates/platform-macos`       | Input capture and injection on macOS                                           |
| `crates/platform-windows`     | Input capture and injection on Windows                                         |
| `crates/tray`, `crates/setup` | The Windows tray app and its installer                                         |
| `apple/`                      | The macOS menu bar app (Swift), built on [NovaUI](../novaui/)                  |
| `packaging/`, `scripts/`      | What goes into the downloads, and the scripts that build them                  |

## Build and test

You need a current stable Rust toolchain. The Mac app also needs Xcode's Swift toolchain (macOS 15 or later).

```bash
cd novalink
cargo test --workspace --features novalink-agent/fake
swift test --package-path apple
```

The `fake` feature swaps real input capture for a test double, so the tests need no permissions and move no cursor.

To run it for real, start the agent and use the CLI from a second terminal:

```bash
cargo run -p novalink-agent
cargo run -p novalink-cli -- doctor
```

On macOS the agent needs Accessibility and Input Monitoring (System Settings > Privacy & Security). `novalink doctor`
says what is missing and how to fix it.

## Using it

- `novalink status` shows this machine, its peers and the layout.
- `novalink discovered` lists NovaLink devices seen on the network that are not paired yet.
- `novalink pair <name|id>` asks a device to pair; `novalink pair accept <id>` on the other machine accepts.
- `novalink layout` says which device sits on which edge of this machine.
- `novalink doctor` checks permissions and connections, with advice.

## Packaging

- `scripts/package-macos.sh` builds the portable macOS zip (ad hoc signed).
- `scripts/build-mac-app.sh OUT_DIR` builds `NovaLink.app` and a dmg, signed with a local self-made identity that
  `scripts/setup-signing.sh` creates on first use. It is not a registered Apple developer identity, so macOS asks
  before the first open.
- `scripts/package-windows.sh OUT_DIR` cross-builds the Windows installer and portable zip from macOS or Linux with
  mingw-w64.

## Licence

Apache-2.0; see [LICENSE](LICENSE) and [NOTICE](NOTICE). The NovaLink name and logo are trademarks; see
[TRADEMARKS.md](../TRADEMARKS.md). Security reports: [SECURITY.md](../SECURITY.md).
