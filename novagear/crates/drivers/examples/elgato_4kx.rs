//! Reads or sets the Elgato 4K X's USB speed mode through its vendor channel, without the
//! agent. Quit Elgato Studio and the Capture Device Utility first, so only one app talks
//! to the card.
//!
//! ```text
//! cargo run -p novagear-drivers --example elgato_4kx            # read the mode
//! cargo run -p novagear-drivers --example elgato_4kx -- raw     # print the read reply's bytes
//! cargo run -p novagear-drivers --example elgato_4kx -- set 10  # switch (the card reconnects)
//! ```

use anyhow::{Context, Result, bail};
use novagear_core::UsbSpeedMode;
use novagear_drivers::elgato4kx::{self, Channel, SetOutcome};

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (mut ch, card) = elgato4kx::open(None)?;
    println!(
        "found {:04x}:{:04x}, firmware bcdDevice {:#06x}{}",
        card.usb.vid,
        card.usb.pid,
        card.device_version,
        if card.device_version == elgato4kx::DECODED_ON {
            ""
        } else {
            " (not the firmware the protocol was decoded on)"
        }
    );
    match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        [] => {
            let m = elgato4kx::get_usb_speed_mode(&mut ch)?;
            println!("USB speed mode: {}", m.label());
        }
        ["raw"] => {
            let req = elgato4kx::get_usb_mode_request();
            ch.set_cur(2, &(req.len() as u16).to_le_bytes())?;
            ch.set_cur(1, &req)?;
            let n = ch.get_cur(2, 2)?;
            let n = u16::from_le_bytes([n[0], n[1]]) as usize;
            let reply = ch.get_cur(1, n)?;
            println!("reply length {n}, got {} bytes:", reply.len());
            for row in reply.chunks(16) {
                let line: Vec<String> = row.iter().map(|b| format!("{b:02x}")).collect();
                println!("  {}", line.join(" "));
            }
            match elgato4kx::parse_usb_mode_reply(&reply) {
                Ok(m) => println!("parses as {}", m.label()),
                Err(e) => println!("does not parse: {e:#}"),
            }
        }
        ["set", speed] => {
            let mode = match speed {
                "5" => UsbSpeedMode::Gbps5,
                "10" => UsbSpeedMode::Gbps10,
                _ => bail!("set 5 or set 10"),
            };
            match elgato4kx::set_usb_speed_mode(&mut ch, mode).context("switching")? {
                SetOutcome::Already => println!("already in {} mode", mode.label()),
                SetOutcome::Switching => println!(
                    "switched to {} mode; the card reconnects as {:04x}:{:04x}",
                    mode.label(),
                    elgato4kx::usb_for(mode).vid,
                    elgato4kx::usb_for(mode).pid
                ),
            }
        }
        _ => bail!("usage: elgato_4kx [raw | set 5 | set 10]"),
    }
    Ok(())
}
