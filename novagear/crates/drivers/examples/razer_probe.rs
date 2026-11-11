//! Lists a Razer device's HID interfaces and tries a read-only DPI query on each, to find
//! the one that takes feature reports. `cargo run -p novagear-drivers --example razer_probe [pid]`

use novagear_drivers::catalog::RAZER;
use novagear_drivers::razer::request;

fn main() -> anyhow::Result<()> {
    let pid = std::env::args()
        .nth(1)
        .map(|p| u16::from_str_radix(p.trim_start_matches("0x"), 16))
        .transpose()?
        .unwrap_or(0x0099);
    // hidapi is only ever used on the hid thread.
    novagear_drivers::hid::with(move |api| {
        for d in api
            .device_list()
            .filter(|d| d.vendor_id() == RAZER && d.product_id() == pid)
        {
            print!(
                "iface {} usage {:#06x}/{:#06x} path {:?}: ",
                d.interface_number(),
                d.usage_page(),
                d.usage(),
                d.path()
            );
            let dev = match d.open_device(api) {
                Ok(x) => x,
                Err(e) => {
                    println!("open failed: {e}");
                    continue;
                }
            };
            let mut out = [0u8; 91];
            out[1..].copy_from_slice(&request(0x1f, 0x04, 0x85, 0x07, &[0x01]));
            match dev.send_feature_report(&out) {
                Err(e) => println!("send failed: {e}"),
                Ok(()) => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                    let mut back = [0u8; 91];
                    match dev.get_feature_report(&mut back) {
                        Ok(n) => println!(
                            "got {n} bytes: status {:#04x} class {:#04x} id {:#04x} dpi {}",
                            back[1],
                            back[7],
                            back[8],
                            u16::from_be_bytes([back[10], back[11]])
                        ),
                        Err(e) => println!("read failed: {e}"),
                    }
                }
            }
        }
    })
}
