<!-- For a device record (TOML only). Open a pull request with ?template=device-record.md to get this checklist. -->

## Device

- Maker and model:
- `usb` id (or `elgato_board`):
- Family:
- Closes #<!-- the "Request a device" issue, if there is one -->

## Tested on real hardware

- Operating system and version:
- NovaGear version or commit:
- Device firmware version:
- What I tried (listed with the right name; controls that work; controls that don't):

## Checklist

- [ ] The record is in the vendor's file in `novagear/crates/drivers/devices/` (or a new vendor file added to
      `BUILTIN` in `devices.rs`).
- [ ] I tested it from my own `devices.toml` first, and the panel showed no record problems.
- [ ] `cargo run -p novagear-drivers --example check_records -- <file>` reports no errors.
- [ ] Anything I couldn't check on real hardware is marked with a `# unconfirmed:` comment.
- [ ] No serial numbers, device paths or details of my own machine.
- [ ] Values were checked against the device or read from references, not copied from another project's files.
- [ ] Every commit is signed off (`git commit -s`).
