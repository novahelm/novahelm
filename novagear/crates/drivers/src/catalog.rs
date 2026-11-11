//! USB and HID listing: what is plugged in or paired, and what we know about each
//! vendor/product pair. Both are cheap reads of the OS's device list that open nothing
//! (a controller is never opened, so there is no Input Monitoring prompt).

use crate::devices::{Family, Records};
use novagear_core::{DeviceKind, UsbId};
use sha2::{Digest, Sha256};

pub const ELGATO: u16 = 0x0fd9;
pub const RAZER: u16 = 0x1532;
pub const EIGHTBITDO: u16 = 0x2dc8;
pub const DISPLAYLINK: u16 = 0x17e9;
pub const RODE: u16 = 0x19f7;

pub const STREAM_DECK_PLUS: UsbId = UsbId {
    vid: ELGATO,
    pid: 0x0084,
};
pub const BASILISK_V3: UsbId = UsbId {
    vid: RAZER,
    pid: 0x0099,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbDevice {
    pub usb: UsbId,
    /// Stable across replugs when the device has a serial; otherwise falls back to bus and address.
    pub key: String,
    pub vendor: String,
    /// The record's name when the product string is empty; "Unknown <vendor> device" for
    /// a known vendor without a record.
    pub name: String,
    pub serial: Option<String>,
    pub kind: DeviceKind,
    pub family: Family,
    /// bcdDevice as "x.yy".
    pub device_version: Option<String>,
    /// Known vendor, no record for this product.
    pub unknown: bool,
    /// The raw product string, for "Copy device details".
    pub product: String,
    /// Class code of each USB interface.
    pub interfaces: Vec<u8>,
    /// HID usage pages; only looked up for unknown devices.
    pub usage_pages: Vec<u16>,
}

/// What we know about a vendor/product pair. `None` means a device not worth listing
/// (hubs, card readers, docks, disks). The third value is true for a known vendor whose
/// product has no record.
///
/// Order: plumbing filter, then the record, then the vendor, then name guessing (for
/// vendors we have no entry for).
pub fn classify(
    records: &Records,
    usb: UsbId,
    class: u8,
    _vendor: &str,
    product: &str,
) -> Option<(DeviceKind, Family, bool)> {
    use DeviceKind::*;
    // 0x09 hub, 0x08 mass storage, 0x11 billboard: plumbing, not gear.
    if matches!(class, 0x08 | 0x09 | 0x11) {
        return None;
    }
    let p = product.to_ascii_lowercase();
    if p.contains("hub")
        || p.contains("card reader")
        || p.contains("billboard")
        || p.contains("dock")
    {
        return None;
    }
    if let Some(r) = records.by_usb(usb) {
        return Some((r.kind, r.family, false));
    }
    if let Some(v) = records.vendor(usb.vid) {
        return Some((v.kind, Family::None, true));
    }
    let kind = if p.contains("keyboard") {
        Keyboard
    } else if p.contains("mouse") {
        Mouse
    } else if p.contains("cam") || p.contains("capture") {
        Capture
    } else if p.contains("mic") {
        Audio
    } else {
        return None;
    };
    Some((kind, Family::None, false))
}

/// Controllers with a record are listed through HID only, over USB and Bluetooth alike, so
/// one controller has one id (`hid-...`) however it is connected. Unknown ones stay in the
/// USB list, which carries the details to copy.
pub fn owned_by_hid(kind: DeviceKind, unknown: bool) -> bool {
    kind == DeviceKind::Controller && !unknown
}

fn bcd(v: u16) -> String {
    format!("{:x}.{:02x}", v >> 8, v & 0xff)
}

/// Runs `f` on a freshly enumerated HID device list; one enumeration per call, always on
/// the hid thread (see `hid`).
fn with_hid<R: Send + 'static>(f: impl FnOnce(&hidapi::HidApi) -> R + Send + 'static) -> Option<R> {
    crate::hid::with(f).ok()
}

/// HID usage pages of each device (opens nothing), from one enumeration.
fn usage_pages(wanted: &[UsbId]) -> Vec<Vec<u16>> {
    let all = with_hid(|api| {
        api.device_list()
            .map(|d| {
                (
                    UsbId {
                        vid: d.vendor_id(),
                        pid: d.product_id(),
                    },
                    d.usage_page(),
                )
            })
            .collect::<Vec<_>>()
    })
    .unwrap_or_default();
    wanted
        .iter()
        .map(|w| {
            let mut pages: Vec<u16> = all
                .iter()
                .filter(|(u, _)| u == w)
                .map(|(_, p)| *p)
                .collect();
            pages.sort_unstable();
            pages.dedup();
            pages
        })
        .collect()
}

/// Lists the gear that is plugged in now, skipping plumbing and devices from vendors
/// we know nothing about.
pub fn list(records: &Records) -> Vec<UsbDevice> {
    let Ok(devs) = nusb::list_devices() else {
        return Vec::new();
    };
    let mut out: Vec<UsbDevice> = devs
        .filter_map(|d| {
            let usb = UsbId {
                vid: d.vendor_id(),
                pid: d.product_id(),
            };
            let product = d.product_string().unwrap_or("").trim().to_string();
            let manufacturer = d.manufacturer_string().unwrap_or("").trim().to_string();
            let (kind, family, unknown) =
                classify(records, usb, d.class(), &manufacturer, &product)?;
            if owned_by_hid(kind, unknown) {
                return None;
            }
            let vendor = if manufacturer.is_empty() {
                records
                    .vendor(usb.vid)
                    .map(|v| v.name.clone())
                    .unwrap_or_default()
            } else {
                manufacturer
            };
            let name = if unknown {
                let who = records
                    .vendor(usb.vid)
                    .map(|v| v.name.as_str())
                    .unwrap_or(&vendor);
                format!("Unknown {who} device")
            } else if product.is_empty() {
                records
                    .by_usb(usb)
                    .map(|r| r.name.clone())
                    .unwrap_or_default()
            } else {
                product.clone()
            };
            let serial = d
                .serial_number()
                .map(str::to_string)
                .filter(|s| !s.is_empty() && s != "Not Provided");
            let key = match &serial {
                Some(s) => format!("usb-{:04x}-{:04x}-{s}", usb.vid, usb.pid),
                None => format!(
                    "usb-{:04x}-{:04x}-b{}a{}",
                    usb.vid,
                    usb.pid,
                    d.bus_number(),
                    d.device_address()
                ),
            };
            Some(UsbDevice {
                usb,
                key,
                vendor,
                name,
                serial,
                kind,
                family,
                device_version: Some(bcd(d.device_version())),
                unknown,
                product,
                interfaces: d.interfaces().map(|i| i.class()).collect(),
                usage_pages: Vec::new(),
            })
        })
        .collect();
    // Usage pages only help identify unknown devices; look them up in one enumeration.
    let unknown: Vec<usize> = (0..out.len()).filter(|&i| out[i].unknown).collect();
    if !unknown.is_empty() {
        let ids: Vec<UsbId> = unknown.iter().map(|&i| out[i].usb).collect();
        for (i, pages) in unknown.into_iter().zip(usage_pages(&ids)) {
            out[i].usage_pages = pages;
        }
    }
    out
}

/// A HID device that is listed but never opened: a paired Bluetooth gadget or a
/// controller. USB gear is the USB list's job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HidDevice {
    pub usb: UsbId,
    /// `hid-<vid>-<pid>-<serial>`, or `hid-<vid>-<pid>-<bt|usb>-<8 hex of sha256(path)>`
    /// without a serial.
    pub key: String,
    pub name: String,
    pub vendor: String,
    pub serial: Option<String>,
    pub bluetooth: bool,
    pub kind: DeviceKind,
}

/// The id of a HID device; see [`HidDevice::key`].
pub fn hid_key(usb: UsbId, serial: Option<&str>, bluetooth: bool, path: &str) -> String {
    match serial {
        Some(s) => format!("hid-{:04x}-{:04x}-{s}", usb.vid, usb.pid),
        None => {
            let h = Sha256::digest(path.as_bytes());
            format!(
                "hid-{:04x}-{:04x}-{}-{:02x}{:02x}{:02x}{:02x}",
                usb.vid,
                usb.pid,
                if bluetooth { "bt" } else { "usb" },
                h[0],
                h[1],
                h[2],
                h[3]
            )
        }
    }
}

/// Lists HID devices from known vendors or with a record. Bluetooth devices all count; a
/// USB-bus one only if it is a controller (other USB gear is in `list`). Never opens one.
pub fn list_hid(records: &Records) -> Vec<HidDevice> {
    use hidapi::BusType;
    // What the loop needs from each entry, copied out on the hid thread so nothing borrowed
    // crosses to it.
    struct Row {
        usb: UsbId,
        bluetooth: bool,
        manufacturer: Option<String>,
        product: String,
        serial: Option<String>,
        path: String,
    }
    let rows = with_hid(|api| {
        api.device_list()
            .filter_map(|d| {
                let bluetooth = match d.bus_type() {
                    BusType::Bluetooth => true,
                    BusType::Usb => false,
                    _ => return None,
                };
                Some(Row {
                    usb: UsbId {
                        vid: d.vendor_id(),
                        pid: d.product_id(),
                    },
                    bluetooth,
                    manufacturer: d.manufacturer_string().map(|m| m.trim().to_string()),
                    product: d.product_string().unwrap_or("").trim().to_string(),
                    serial: d
                        .serial_number()
                        .map(str::to_string)
                        .filter(|s| !s.is_empty() && s != "Not Provided"),
                    path: d.path().to_string_lossy().into_owned(),
                })
            })
            .collect::<Vec<_>>()
    })
    .unwrap_or_default();
    let mut out: Vec<HidDevice> = Vec::new();
    for Row {
        usb,
        bluetooth,
        manufacturer,
        product,
        serial,
        path,
    } in rows
    {
        let (kind, record_name) = match (records.by_usb(usb), records.vendor(usb.vid)) {
            (Some(r), _) => (r.kind, Some(r.name.clone())),
            (None, Some(v)) => (v.kind, None),
            (None, None) => continue,
        };
        if !bluetooth && kind != DeviceKind::Controller {
            continue;
        }
        let vendor = records
            .vendor(usb.vid)
            .map(|v| v.name.clone())
            .or(manufacturer)
            .unwrap_or_default();
        let name = if !product.is_empty() {
            product
        } else if let Some(n) = record_name {
            n
        } else {
            format!("Unknown {vendor} device")
        };
        let key = hid_key(usb, serial.as_deref(), bluetooth, &path);
        // hidapi lists one entry per interface; one device is one key.
        if out.iter().any(|o| o.key == key) {
            continue;
        }
        out.push(HidDevice {
            usb,
            key,
            name,
            vendor,
            serial,
            bluetooth,
            kind,
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_the_gear_on_a_sample_desk() {
        let r = Records::builtin();
        let k = |vid, pid, name| classify(&r, UsbId { vid, pid }, 0, "", name).map(|c| c.0);
        assert_eq!(
            k(ELGATO, 0x0084, "Stream Deck Plus"),
            Some(DeviceKind::StreamDeck)
        );
        assert_eq!(k(ELGATO, 0x009c, "Elgato 4K X"), Some(DeviceKind::Capture));
        assert_eq!(k(ELGATO, 0x0066, "Cam Link 4K"), Some(DeviceKind::Capture));
        assert_eq!(
            k(DISPLAYLINK, 0xff1a, "Elgato Prompter"),
            Some(DeviceKind::Prompter)
        );
        assert_eq!(
            k(RAZER, 0x0099, "Razer Basilisk V3"),
            Some(DeviceKind::Mouse)
        );
        assert_eq!(
            k(EIGHTBITDO, 0x5201, "8BitDo Retro Keyboard"),
            Some(DeviceKind::Keyboard)
        );
        assert_eq!(k(RODE, 0x004a, "RØDE PodMic USB"), Some(DeviceKind::Audio));
    }

    #[test]
    fn skips_plumbing() {
        assert_eq!(
            classify(
                &Records::builtin(),
                UsbId {
                    vid: ELGATO,
                    pid: 0x00a4
                },
                9,
                "",
                "USB2.0 Hub"
            ),
            None
        );
        assert_eq!(
            classify(
                &Records::builtin(),
                UsbId {
                    vid: ELGATO,
                    pid: 0x00ac
                },
                0,
                "",
                "USB Dock for Stream Deck +"
            ),
            None
        );
        assert_eq!(
            classify(
                &Records::builtin(),
                UsbId {
                    vid: 0x05e3,
                    pid: 0x0749
                },
                0,
                "",
                "USB3.0 Card Reader"
            ),
            None
        );
        assert_eq!(
            classify(
                &Records::builtin(),
                UsbId {
                    vid: 0x1058,
                    pid: 0x2631
                },
                8,
                "",
                "Game Drive"
            ),
            None
        );
    }

    #[test]
    fn known_vendor_unknown_pid_is_unknown() {
        let r = Records::builtin();
        let (k, f, unknown) = classify(
            &r,
            UsbId {
                vid: 0x1532,
                pid: 0x0fff,
            },
            0,
            "Razer",
            "Razer Mystery",
        )
        .unwrap();
        assert_eq!((k, f, unknown), (DeviceKind::Mouse, Family::None, true));
    }

    #[test]
    fn records_win_over_name_guessing() {
        let r = Records::builtin();
        assert_eq!(
            classify(
                &r,
                UsbId {
                    vid: 0x0fd9,
                    pid: 0x0084
                },
                0,
                "Elgato",
                ""
            )
            .unwrap()
            .0,
            DeviceKind::StreamDeck
        );
    }

    #[test]
    fn empty_names_from_unknown_vendors_never_panic() {
        let r = Records::builtin();
        let id = UsbId {
            vid: 0xdead,
            pid: 0xbeef,
        };
        assert_eq!(classify(&r, id, 0, "", ""), None);
        let (k, _, unknown) = classify(&r, id, 0, "", "USB Gaming Mouse").unwrap();
        assert_eq!((k, unknown), (DeviceKind::Mouse, false));
    }

    #[test]
    fn device_version_is_bcd() {
        assert_eq!(bcd(0x0102), "1.02");
        assert_eq!(bcd(0x0210), "2.10");
    }

    #[test]
    fn known_controllers_are_listed_by_hid_only() {
        let r = Records::builtin();
        let id = UsbId {
            vid: 0x054c,
            pid: 0x0ce6,
        };
        let (kind, _, unknown) =
            classify(&r, id, 0, "Sony", "DualSense Wireless Controller").unwrap();
        assert!(owned_by_hid(kind, unknown));
        let (kind, _, unknown) = classify(
            &r,
            UsbId {
                vid: 0x054c,
                pid: 0xffff,
            },
            0,
            "Sony",
            "Sony Thing",
        )
        .unwrap();
        assert!(unknown && !owned_by_hid(kind, unknown));
        assert!(!owned_by_hid(DeviceKind::Mouse, false));
    }

    #[test]
    fn hid_keys_use_the_serial_or_a_path_hash() {
        let id = UsbId {
            vid: 0x054c,
            pid: 0x0ce6,
        };
        assert_eq!(hid_key(id, Some("ab"), true, "p"), "hid-054c-0ce6-ab");
        let a = hid_key(id, None, true, "DevSrvsID:1");
        assert!(a.starts_with("hid-054c-0ce6-bt-") && a.len() == "hid-054c-0ce6-bt-".len() + 8);
        assert_eq!(a, hid_key(id, None, true, "DevSrvsID:1"));
        assert_ne!(a, hid_key(id, None, true, "DevSrvsID:2"));
    }
}
