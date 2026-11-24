//! Wake-on-LAN: the magic packet and a blocking broadcast sender.
use std::net::{IpAddr, Ipv4Addr, SocketAddrV4, UdpSocket};
use std::time::Duration;

use crate::netif;

/// UDP port the magic packet is sent to (the "discard" port, the usual choice).
pub const WOL_PORT: u16 = 9;

const ROUNDS: usize = 3;
const ROUND_GAP: Duration = Duration::from_millis(100);

/// 6 bytes of 0xFF, then the MAC address 16 times.
pub fn magic_packet(mac: [u8; 6]) -> [u8; 102] {
    let mut p = [0xff; 102];
    for i in 0..16 {
        p[6 + i * 6..12 + i * 6].copy_from_slice(&mac);
    }
    p
}

/// Where to send a magic packet: the limited broadcast first, then the directed broadcast of
/// every allowed interface subnet (some stacks only route one or the other). Takes
/// `(name, addr, netmask)`; loopback and link-local addresses are skipped, duplicates dropped.
pub fn broadcast_targets(ifaces: &[(String, Ipv4Addr, Ipv4Addr)]) -> Vec<Ipv4Addr> {
    let mut out = vec![Ipv4Addr::BROADCAST];
    for (name, addr, mask) in ifaces {
        if addr.is_loopback() || addr.is_link_local() || !netif::allowed(name, IpAddr::V4(*addr)) {
            continue;
        }
        let directed = Ipv4Addr::from(u32::from(*addr) | !u32::from(*mask));
        if !out.contains(&directed) {
            out.push(directed);
        }
    }
    out
}

/// Sends a magic packet for each MAC to every broadcast target, three rounds 100 ms apart
/// (UDP broadcast is lossy). Blocking. Returns the number of datagrams sent.
pub fn send_wake(macs: &[[u8; 6]]) -> std::io::Result<usize> {
    let ifaces: Vec<(String, Ipv4Addr, Ipv4Addr)> = if_addrs::get_if_addrs()?
        .into_iter()
        .filter_map(|i| match i.addr {
            if_addrs::IfAddr::V4(v4) => Some((i.name, v4.ip, v4.netmask)),
            _ => None,
        })
        .collect();
    let targets = broadcast_targets(&ifaces);
    let sock = UdpSocket::bind("0.0.0.0:0")?;
    sock.set_broadcast(true)?;
    let mut sent = 0;
    for round in 0..ROUNDS {
        if round > 0 {
            std::thread::sleep(ROUND_GAP);
        }
        for mac in macs {
            let packet = magic_packet(*mac);
            for target in &targets {
                if sock
                    .send_to(&packet, SocketAddrV4::new(*target, WOL_PORT))
                    .is_ok()
                {
                    sent += 1;
                }
            }
        }
    }
    Ok(sent)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn magic_packet_layout() {
        let p = magic_packet([1, 2, 3, 4, 5, 6]);
        assert_eq!(&p[..6], &[0xff; 6]);
        for i in 0..16 {
            assert_eq!(&p[6 + i * 6..12 + i * 6], &[1, 2, 3, 4, 5, 6]);
        }
    }

    #[test]
    fn broadcast_targets_global_first_then_allowed_subnets() {
        let t = broadcast_targets(&[
            (
                "en0".into(),
                "192.168.68.42".parse().unwrap(),
                "255.255.252.0".parse().unwrap(),
            ),
            (
                "en0".into(),
                "192.168.68.43".parse().unwrap(),
                "255.255.252.0".parse().unwrap(),
            ),
            (
                "docker0".into(),
                "172.17.0.1".parse().unwrap(),
                "255.255.0.0".parse().unwrap(),
            ),
            (
                "lo0".into(),
                "127.0.0.1".parse().unwrap(),
                "255.0.0.0".parse().unwrap(),
            ),
        ]);
        assert_eq!(
            t,
            vec![
                "255.255.255.255".parse::<Ipv4Addr>().unwrap(),
                "192.168.71.255".parse().unwrap()
            ]
        );
    }
}
