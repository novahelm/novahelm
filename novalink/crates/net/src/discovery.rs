//! mDNS advertisement and browsing for `_novalink._udp.local.`.
//!
//! The TXT record carries `id` (hex device id), `name` and `v` (protocol version). Only
//! addresses that [`netif::allowed`] accepts are advertised or reported. mDNS is a hint
//! about where a device might be; the QUIC handshake still authenticates it.
use crate::netif;
use anyhow::{Context, Result};
use mdns_sd::{IfKind, ServiceDaemon, ServiceEvent, ServiceInfo};
use novalink_proto::{DeviceId, PROTO_VERSION};
use std::net::{IpAddr, SocketAddr};
use tokio::sync::mpsc;

pub const SERVICE_TYPE: &str = "_novalink._udp.local.";

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Discovered {
    pub id: DeviceId,
    pub name: String,
    pub addrs: Vec<SocketAddr>,
}

/// Keeps the advertisement and the browse alive; both stop on drop.
pub struct Discovery {
    daemon: ServiceDaemon,
}

fn parse_id(hex: &str) -> Option<DeviceId> {
    if hex.len() != 64 || !hex.is_ascii() {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, b) in out.iter_mut().enumerate() {
        *b = u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).ok()?;
    }
    Some(DeviceId(out))
}

/// Turns a resolved service into a [`Discovered`], or `None` if it is malformed, from a
/// different protocol version, is ourselves, or has no usable address.
fn from_service(me: DeviceId, info: &ServiceInfo) -> Option<Discovered> {
    let id = parse_id(info.get_property_val_str("id")?)?;
    if id == me || info.get_property_val_str("v")? != PROTO_VERSION.to_string() {
        return None;
    }
    let name = crate::sanitize_name(info.get_property_val_str("name")?);
    let port = info.get_port();
    let mut addrs: Vec<SocketAddr> = info
        .get_addresses()
        .iter()
        // Scope ids are not carried, so IPv6 link-local cannot be dialled.
        .filter(|ip| netif::allowed("", **ip) && !is_v6_link_local(**ip))
        .map(|ip| SocketAddr::new(*ip, port))
        .collect();
    addrs.sort();
    (!addrs.is_empty()).then_some(Discovered { id, name, addrs })
}

fn is_v6_link_local(ip: IpAddr) -> bool {
    matches!(ip, IpAddr::V6(v6) if v6.segments()[0] & 0xffc0 == 0xfe80)
}

impl Discovery {
    pub fn start(
        me: DeviceId,
        name: &str,
        port: u16,
    ) -> Result<(Discovery, mpsc::Receiver<Discovered>)> {
        let daemon = ServiceDaemon::new().context("start mDNS daemon")?;
        let local: Vec<(String, IpAddr)> = if_addrs::get_if_addrs()
            .context("list interfaces")?
            .into_iter()
            .filter(|i| !i.is_loopback() && netif::allowed(&i.name, i.ip()))
            .map(|i| {
                let ip = i.ip();
                (i.name, ip)
            })
            .collect();

        // Speak mDNS only on interfaces NovaLink may use.
        daemon.disable_interface(IfKind::All)?;
        for (_, ip) in &local {
            daemon.enable_interface(IfKind::Addr(*ip))?;
        }

        let hex = me.to_string();
        let instance = &hex[..16];
        let advertised: Vec<IpAddr> = local
            .iter()
            .map(|(_, ip)| *ip)
            .filter(|ip| !is_v6_link_local(*ip))
            .collect();
        if advertised.is_empty() {
            tracing::warn!("no LAN address to advertise over mDNS");
        } else {
            let props = [
                ("id", hex.as_str()),
                ("name", name),
                ("v", &PROTO_VERSION.to_string()),
            ];
            let info = ServiceInfo::new(
                SERVICE_TYPE,
                instance,
                &format!("novalink-{instance}.local."),
                advertised.as_slice(),
                port,
                &props[..],
            )
            .context("build mDNS service")?;
            daemon.register(info).context("register mDNS service")?;
        }

        let events = daemon.browse(SERVICE_TYPE).context("browse mDNS")?;
        let (tx, rx) = mpsc::channel(64);
        std::thread::Builder::new()
            .name("novalink-mdns".into())
            .spawn(move || {
                // Ends when the daemon shuts down and the flume channel disconnects.
                while let Ok(ev) = events.recv() {
                    if let ServiceEvent::ServiceResolved(info) = ev
                        && let Some(d) = from_service(me, &info)
                        && tx.blocking_send(d).is_err()
                    {
                        return;
                    }
                }
            })
            .context("spawn mDNS thread")?;
        Ok((Discovery { daemon }, rx))
    }
}

impl Drop for Discovery {
    fn drop(&mut self) {
        let _ = self.daemon.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(props: &[(&str, &str)], ips: &[&str]) -> ServiceInfo {
        ServiceInfo::new(SERVICE_TYPE, "x", "h.local.", ips, 4242, props).unwrap()
    }

    #[test]
    fn parses_txt_and_filters_addresses() {
        let id = DeviceId([7; 32]);
        let v = PROTO_VERSION.to_string();
        let hex = id.to_string();
        let props = [("id", hex.as_str()), ("name", "pc"), ("v", v.as_str())];
        let me = DeviceId([1; 32]);
        let d = from_service(me, &info(&props, &["192.168.1.5", "8.8.8.8", "fe80::1"])).unwrap();
        assert_eq!(d.id, id);
        assert_eq!(d.name, "pc");
        assert_eq!(d.addrs, vec!["192.168.1.5:4242".parse().unwrap()]);
        // ourselves, wrong version, public-only addresses, bad id: all ignored
        assert!(from_service(id, &info(&props, &["192.168.1.5"])).is_none());
        let bad_v = [("id", hex.as_str()), ("name", "pc"), ("v", "99")];
        assert!(from_service(me, &info(&bad_v, &["192.168.1.5"])).is_none());
        assert!(from_service(me, &info(&props, &["8.8.8.8"])).is_none());
        let bad_id = [("id", "zz"), ("name", "pc"), ("v", v.as_str())];
        assert!(from_service(me, &info(&bad_id, &["192.168.1.5"])).is_none());
    }

    #[test]
    fn discovered_names_are_sanitized() {
        let id = DeviceId([7; 32]);
        let v = PROTO_VERSION.to_string();
        let hex = id.to_string();
        let props = [
            ("id", hex.as_str()),
            ("name", " evil\u{1b}[2J\u{85}pc "),
            ("v", v.as_str()),
        ];
        let d = from_service(DeviceId([1; 32]), &info(&props, &["192.168.1.5"])).unwrap();
        assert_eq!(d.name, "evil[2Jpc");
    }

    #[test]
    fn hex_id_roundtrip() {
        let id = DeviceId([0xab; 32]);
        assert_eq!(parse_id(&id.to_string()), Some(id));
        assert_eq!(parse_id("abc"), None);
    }
}
