# Device records

NovaGear knows about each piece of gear through a **record**: a few lines of TOML that say what the device is, how to
recognise it and which driver handles it. The files here (one per vendor) are built into the agent. Your own
`devices.toml` in the config folder (`~/Library/Application Support/NovaGear/` on macOS) is read on top of them, and a
record there with the same id replaces the built-in one.

## Two ways to add a device

**1. A record (no code).** For a device that a driver NovaGear already has can handle: another Razer mouse that speaks
the same protocol as the Basilisk V3, another Elgato light, a capture card, or any device you just want listed with
the right name. You write a record, test it in your own `devices.toml`, and send it as a pull request against the
vendor's file here. Most new devices start here.

**2. A driver (code).** For a device that talks a protocol NovaGear doesn't speak yet (a Logitech Litra, a Stream Deck
with a different key layout, a Loupedeck). That means Rust in `crates/drivers/src/` and wiring in the agent. Open a
"Request a device" issue first so we can agree on the approach; a record with `family = "none"` can land before the
driver, so the device is at least listed.

Not sure which? Open a "Request a device" issue with the panel's **Copy device details** output and we'll say.

## Start from "Copy device details"

Plug the device in and open the NovaGear panel. A device with no record is listed with a **Copy device details**
button. It copies two things, and never a serial number or a device path:

- the details NovaGear sees (USB vendor and product ids, product string, interface classes, HID usage pages and the
  device version), for an issue;
- a **draft record**, listed but not driven (`family = "none"`), for your `devices.toml`.

Paste the draft into `devices.toml`, restart NovaGear, and the device shows with its name. If the file has a mistake,
the panel shows a banner and `/api/status` names the record; the other records still load.

## The format

```toml
# One [[vendor]] per USB vendor id. Devices from this vendor with no record of their own are listed as
# "Unknown <name> device" with this kind.
[[vendor]]
vid = 0x1532          # USB vendor id, as a number
name = "Razer"
kind = "mouse"

# unconfirmed: firmware tool/URL not verified   <- say what you haven't checked on real hardware
[[device]]
usb = "1532:0099"     # "vid:pid" in hex, as the details show it. This is the record's id.
name = "Razer Basilisk V3"
kind = "mouse"        # what it is (see "kind" below)
family = "razer90"    # which driver handles it (see "family" below)

# Optional: driver-specific settings. Only the family's driver reads them.
quirks = { interface = 3, transaction_id = 0x1F }

# Optional: how its firmware is updated. policy is native, fwupd, vendor_tool or none (the default).
firmware = { policy = "vendor_tool", tool = "Razer Synapse", url = "https://www.razer.com/synapse-4" }

# Optional: capabilities, as a table. Loose on purpose; each family reads what it needs.
[device.caps]
# ...
```

Elgato Key Lights are found on the network, not over USB, so their records use `elgato_board` (the light's
`hardwareBoardType` from `http://<light>:9123/elgato/accessory-info`) as the id instead of `usb`:

```toml
[[device]]
elgato_board = 53
name = "Key Light"
kind = "light"
family = "keylight"
firmware = { policy = "native" }
[device.caps]
colour = false
kelvin_min = 2900
kelvin_max = 7000
```

### `kind`

`light`, `stream_deck`, `prompter`, `capture`, `mouse`, `keyboard`, `audio`, `controller` or `other`. It decides the
icon, the group the device is listed in and which panel controls can apply.

### `family`

| Family        | What a record alone gets you                                                                                                                     |
| ------------- | ------------------------------------------------------------------------------------------------------------------------------------------------ |
| `keylight`    | Elgato lights on the Key Light HTTP API: on/off, brightness, warmth, firmware check. The record names the board.                                 |
| `razer90`     | Razer mice on the 90-byte feature-report protocol: DPI, static colour, brightness. Needs `quirks.interface` and usually `quirks.transaction_id`. |
| `uvc`         | Capture cards and webcams: listed, with Preview in the panel.                                                                                    |
| `streamdeck`  | Listed. Only the Stream Deck + is driven today; other layouts need driver work.                                                                  |
| `displaylink` | The Elgato Prompter's screen.                                                                                                                    |
| `hid`, `none` | Listed with the right name and kind, not controlled.                                                                                             |

### Finding a Razer mouse's `quirks`

On macOS a feature report sent to the wrong USB interface makes the mouse disconnect and reconnect, so the driver only
talks to the interface the record names. Find it with:

```bash
cargo run -p novagear-drivers --example razer_probe <pid>
```

It tries a read-only DPI query on each interface and prints which one answers. The transaction id is `0x1F` or `0x3F`
for most current models and `0xFF` for older ones; [OpenRazer](https://github.com/openrazer/openrazer)'s device list is a
good reference (read it, don't copy it).

## Check a record before you send it

```bash
cargo run -p novagear-drivers --example check_records -- path/to/devices.toml
```

It does what CI does to the files here: every record must parse, ids must be unique in the file, and unknown keys
(usually typos such as `quirk =`) are errors. It also notes when a record replaces a built-in one. With no arguments
it checks the built-in files; `cargo test -p novagear-drivers` checks them too.

## Rules for a record pull request

- One device (or one family of near-identical models) per pull request, in the vendor's file here. A new vendor gets a
  new file, added to `BUILTIN` in `crates/drivers/src/devices.rs`.
- Say what you tested on real hardware, and mark anything you didn't check with a `# unconfirmed:` comment above the
  record.
- Never include a serial number, a device path or your own machine's details.
- Ids come from the device, names from the vendor's own product name. Read open-source references (OpenRazer,
  python-elgato-streamdeck, fwupd) to check values, but don't copy their files.
