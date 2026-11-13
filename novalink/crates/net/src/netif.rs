//! Which network interfaces/addresses NovaLink may use, and how to rank them.
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub enum PathKind {
    Wired,
    Wifi,
    Vpn,
    Other,
}

const DENIED_PREFIXES: [&str; 7] = [
    "docker",
    "br-",
    "veth",
    "vmnet",
    "vboxnet",
    "vethernet",
    "bridge",
];

fn is_cgnat(v4: Ipv4Addr) -> bool {
    let o = v4.octets();
    o[0] == 100 && (64..=127).contains(&o[1])
}

fn is_ula(v6: Ipv6Addr) -> bool {
    v6.segments()[0] & 0xfe00 == 0xfc00
}

fn is_v6_link_local(v6: Ipv6Addr) -> bool {
    v6.segments()[0] & 0xffc0 == 0xfe80
}

/// Whether traffic may use this interface name and address.
pub fn allowed(name: &str, addr: IpAddr) -> bool {
    let lower = name.to_ascii_lowercase();
    if DENIED_PREFIXES.iter().any(|p| lower.starts_with(p)) {
        return false;
    }
    match addr {
        IpAddr::V4(v4) => v4.is_private() || v4.is_link_local() || is_cgnat(v4),
        IpAddr::V6(v6) => is_ula(v6) || is_v6_link_local(v6),
    }
}

/// Self-assigned addresses (169.254/16, fe80::/10): they work only on one link, and an
/// adapter with nothing behind it (no DHCP) still has one, so they are tried last.
pub fn is_link_local(addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => v4.is_link_local(),
        IpAddr::V6(v6) => is_v6_link_local(v6),
    }
}

/// Lower is preferred.
pub fn rank(kind: PathKind) -> u8 {
    match kind {
        PathKind::Wired => 0,
        PathKind::Wifi => 1,
        PathKind::Other => 2,
        PathKind::Vpn => 3,
    }
}

/// Best-effort classification by name; `Other` when it can't be told
/// (e.g. `en0`, which is Wi-Fi on laptops but wired on desktops).
pub fn classify(name: &str, addr: IpAddr) -> PathKind {
    if matches!(addr, IpAddr::V4(v4) if is_cgnat(v4)) {
        return PathKind::Vpn;
    }
    let n = name.to_ascii_lowercase();
    let starts = |ps: &[&str]| ps.iter().any(|p| n.starts_with(p));
    if starts(&["utun", "tun", "tap", "wg", "tailscale", "ppp", "ipsec"]) {
        PathKind::Vpn
    } else if starts(&["wlan", "wlp", "wlx", "wi-fi", "wifi", "wireless"]) {
        PathKind::Wifi
    } else if starts(&["eth", "enp", "eno", "ens"]) {
        PathKind::Wired
    } else {
        PathKind::Other
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn interface_filter_table() -> anyhow::Result<()> {
        assert!(allowed("en0", "192.168.1.10".parse()?));
        assert!(allowed("utun3", "100.101.1.2".parse()?));
        assert_eq!(classify("utun3", "100.101.1.2".parse()?), PathKind::Vpn);
        assert!(!allowed("docker0", "172.17.0.1".parse()?));
        assert!(!allowed("vEthernet (WSL)", "172.20.0.1".parse()?));
        assert!(!allowed("en0", "8.8.8.8".parse()?));
        assert!(!allowed("lo0", "127.0.0.1".parse()?));
        assert!(allowed("en0", "169.254.10.1".parse()?));
        assert_eq!(classify("en0", "169.254.10.1".parse()?), PathKind::Other);
        assert!(allowed("en0", "fe80::1".parse()?));
        assert!(allowed("en0", "fd00::1".parse()?));
        assert!(!allowed("en0", "2001:db8::1".parse()?));
        assert!(!allowed("br-abc", "192.168.5.1".parse()?));
        assert_eq!(rank(PathKind::Wired), 0);
        assert_eq!(rank(PathKind::Vpn), 3);
        Ok(())
    }
}
