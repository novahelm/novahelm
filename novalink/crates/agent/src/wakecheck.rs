//! Whether the PC's wired adapter is set up to wake it from a magic packet. The facts come
//! from Windows (`winnet::wake_facts`); the verdict is pure so it is tested anywhere.

/// What Windows says about one wired adapter's wake settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WakeFacts {
    /// The adapter's name in Windows, as in "Ethernet".
    pub adapter: String,
    /// The adapter's description, which is how `powercfg` lists it.
    pub description: String,
    /// Output of `Get-NetAdapterPowerManagement ... WakeOnMagicPacket`.
    pub magic_packet: String,
    /// Output of `powercfg /devicequery wake_armed`: one device per line.
    pub wake_armed: String,
}

/// `(ok, detail, advice)` for the doctor's Wake-on-LAN check.
pub fn verdict(f: &WakeFacts) -> (bool, String, String) {
    let magic = f.magic_packet.trim() == "Enabled";
    let armed = f
        .wake_armed
        .lines()
        .any(|l| l.trim().eq_ignore_ascii_case(f.description.trim()));
    if magic && armed {
        (
            true,
            format!(
                "{} can wake this PC. If a wake still fails, check the BIOS Wake-on-LAN setting \
                 and the router.",
                f.adapter
            ),
            String::new(),
        )
    } else {
        let what = match (magic, armed) {
            (false, false) => "magic packet wake is off and the device may not wake the PC",
            (false, true) => "magic packet wake is off",
            _ => "the device may not wake the PC",
        };
        (
            false,
            format!("{}: {what}", f.adapter),
            format!(
                "Turn on 'Allow this device to wake the computer' and 'Only allow a magic packet \
                 to wake the computer' for {} in Device Manager → Power Management. Some PCs \
                 also need Wake-on-LAN turned on in the BIOS.",
                f.adapter
            ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verdict_needs_magic_packet_and_wake_armed() {
        let mut f = WakeFacts {
            adapter: "Ethernet".into(),
            description: "Intel(R) Ethernet Controller I225-V".into(),
            magic_packet: "Enabled\r\n".into(),
            wake_armed: "HID Keyboard Device\r\nintel(r) ethernet controller i225-v\r\n".into(),
        };
        let (ok, detail, _) = verdict(&f);
        assert!(ok);
        assert_eq!(
            detail,
            "Ethernet can wake this PC. If a wake still fails, check the BIOS Wake-on-LAN \
             setting and the router."
        );
        f.magic_packet = "Disabled".into();
        let (ok, _, advice) = verdict(&f);
        assert!(
            !ok && advice.contains("Allow this device to wake the computer")
                && advice.contains("Ethernet")
        );
        f.magic_packet = "Enabled".into();
        f.wake_armed = "HID Keyboard Device".into();
        assert!(!verdict(&f).0);
    }
}
