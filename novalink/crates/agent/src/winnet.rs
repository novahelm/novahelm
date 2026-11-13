//! What Windows lets NovaLink do on the network: whether the connected networks are Public,
//! and whether Windows Firewall lets the agent in on Private ones. The rules are pure so
//! they are tested anywhere; [`probe`] reads the facts on Windows.

use std::path::Path;

/// Windows Firewall profile bits (NET_FW_PROFILE_TYPE2).
pub const PROFILE_DOMAIN: i32 = 1;
pub const PROFILE_PRIVATE: i32 = 2;
pub const PROFILE_PUBLIC: i32 = 4;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Category {
    Public,
    Private,
    Domain,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Network {
    pub name: String,
    pub category: Category,
}

/// An enabled inbound firewall rule for the agent's program.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rule {
    pub allow: bool,
    /// Profile bits the rule applies to.
    pub profiles: i32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Facts {
    /// Networks Windows says are connected.
    pub networks: Vec<Network>,
    pub firewall_on_private: bool,
    pub rules: Vec<Rule>,
}

/// Connected networks Windows treats as Public, where it blocks NovaLink.
pub fn public_networks(f: &Facts) -> Vec<&str> {
    f.networks
        .iter()
        .filter(|n| n.category == Category::Public)
        .map(|n| n.name.as_str())
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Firewall {
    Allowed,
    Off,
    Blocked,
    NoRule,
}

/// What Windows Firewall does to the agent on Private networks. A block rule wins over any
/// allow rule, as it does in the firewall itself.
pub fn firewall(f: &Facts) -> Firewall {
    let private = |r: &&Rule| r.profiles & PROFILE_PRIVATE != 0;
    if f.rules.iter().filter(private).any(|r| !r.allow) {
        Firewall::Blocked
    } else if !f.firewall_on_private {
        Firewall::Off
    } else if f.rules.iter().filter(private).any(|r| r.allow) {
        Firewall::Allowed
    } else {
        Firewall::NoRule
    }
}

/// Settings advice for a network set to Public.
pub fn public_advice(names: &[&str]) -> String {
    let which = match names {
        [one] => format!("“{one}”"),
        _ => "it".into(),
    };
    format!(
        "Windows blocks NovaLink on Public networks. If {which} is your home or office network, \
         open Settings → Network & internet, choose your connection and set it to Private network."
    )
}

/// Whether a rule's program is `exe`. Rules may name it with environment variables, as
/// %LOCALAPPDATA%\Programs\NovaLink\novalink-agent.exe, and Windows paths ignore case.
pub fn same_program(rule_app: &str, exe: &Path, env: impl Fn(&str) -> Option<String>) -> bool {
    let mut expanded = String::new();
    let mut rest = rule_app;
    while let Some(start) = rest.find('%') {
        let Some(len) = rest[start + 1..].find('%') else {
            break;
        };
        let name = &rest[start + 1..start + 1 + len];
        expanded.push_str(&rest[..start]);
        match env(name) {
            Some(v) => expanded.push_str(&v),
            None => expanded.push_str(&rest[start..start + len + 2]),
        }
        rest = &rest[start + len + 2..];
    }
    expanded.push_str(rest);
    expanded.eq_ignore_ascii_case(&exe.to_string_lossy())
}

#[cfg(windows)]
pub use probe::probe;

#[cfg(windows)]
mod probe {
    use super::{Category, Facts, Network, Rule, same_program};
    use std::path::Path;
    use windows::Win32::NetworkManagement::WindowsFirewall::{
        INetFwPolicy2, INetFwRule, NET_FW_ACTION_ALLOW, NET_FW_PROFILE2_PRIVATE,
        NET_FW_RULE_DIR_IN, NetFwPolicy2,
    };
    use windows::Win32::Networking::NetworkListManager::{
        INetworkListManager, NLM_ENUM_NETWORK_CONNECTED, NLM_NETWORK_CATEGORY_DOMAIN_AUTHENTICATED,
        NLM_NETWORK_CATEGORY_PUBLIC, NetworkListManager,
    };
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx,
        CoUninitialize,
    };
    use windows::Win32::System::Ole::IEnumVARIANT;
    use windows::Win32::System::Variant::{VARIANT, VT_DISPATCH};
    use windows::core::Interface;

    /// Reads the connected networks and the firewall rules for `exe`. Needs no admin
    /// rights. Blocks while COM answers, so call it off the async runtime.
    pub fn probe(exe: &Path) -> Result<Facts, String> {
        // SAFETY: COM is initialised for this thread before use and released after every
        // interface pointer below has been dropped.
        unsafe {
            let init = CoInitializeEx(None, COINIT_MULTITHREADED);
            let facts = read(exe).map_err(|e| e.message().trim().to_string());
            if init.is_ok() {
                CoUninitialize();
            }
            facts
        }
    }

    unsafe fn read(exe: &Path) -> windows::core::Result<Facts> {
        unsafe {
            let nlm: INetworkListManager =
                CoCreateInstance(&NetworkListManager, None, CLSCTX_INPROC_SERVER)?;
            let mut networks = Vec::new();
            let list = nlm.GetNetworks(NLM_ENUM_NETWORK_CONNECTED)?;
            loop {
                let mut one = [None];
                let mut got = 0;
                list.Next(&mut one, Some(&mut got))?;
                let Some(n) = one[0].take().filter(|_| got == 1) else {
                    break;
                };
                let category = match n.GetCategory()? {
                    NLM_NETWORK_CATEGORY_PUBLIC => Category::Public,
                    NLM_NETWORK_CATEGORY_DOMAIN_AUTHENTICATED => Category::Domain,
                    _ => Category::Private,
                };
                networks.push(Network {
                    name: n.GetName()?.to_string(),
                    category,
                });
            }

            let policy: INetFwPolicy2 =
                CoCreateInstance(&NetFwPolicy2, None, CLSCTX_INPROC_SERVER)?;
            let firewall_on_private = policy
                .get_FirewallEnabled(NET_FW_PROFILE2_PRIVATE)?
                .as_bool();
            let all: IEnumVARIANT = policy.Rules()?._NewEnum()?.cast()?;
            let mut rules = Vec::new();
            loop {
                let mut v = [VARIANT::default()];
                let mut got = 0;
                if all.Next(&mut v, &mut got).is_err() || got == 0 {
                    break;
                }
                let raw = &v[0].Anonymous.Anonymous;
                if raw.vt != VT_DISPATCH {
                    continue;
                }
                let Some(Ok(rule)) = raw
                    .Anonymous
                    .pdispVal
                    .as_ref()
                    .map(|d| d.cast::<INetFwRule>())
                else {
                    continue;
                };
                let inbound_on =
                    rule.Direction()? == NET_FW_RULE_DIR_IN && rule.Enabled()?.as_bool();
                // Rules for services or ports carry no program and fail to answer here.
                let app = rule
                    .ApplicationName()
                    .map(|a| a.to_string())
                    .unwrap_or_default();
                if inbound_on && same_program(&app, exe, |k| std::env::var(k).ok()) {
                    rules.push(Rule {
                        allow: rule.Action()? == NET_FW_ACTION_ALLOW,
                        profiles: rule.Profiles()?,
                    });
                }
            }
            Ok(Facts {
                networks,
                firewall_on_private,
                rules,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts(on: bool, rules: &[(bool, i32)]) -> Facts {
        Facts {
            networks: vec![],
            firewall_on_private: on,
            rules: rules
                .iter()
                .map(|&(allow, profiles)| Rule { allow, profiles })
                .collect(),
        }
    }

    #[test]
    fn firewall_needs_an_allow_rule_for_private_and_no_block() {
        assert_eq!(firewall(&facts(true, &[])), Firewall::NoRule);
        assert_eq!(
            firewall(&facts(true, &[(true, PROFILE_PUBLIC)])),
            Firewall::NoRule
        );
        assert_eq!(
            firewall(&facts(true, &[(true, PROFILE_PRIVATE | PROFILE_DOMAIN)])),
            Firewall::Allowed
        );
        assert_eq!(
            firewall(&facts(
                true,
                &[(true, 0x7fff_ffff), (false, PROFILE_PRIVATE)]
            )),
            Firewall::Blocked
        );
        // A block on Public only, as Windows makes when its prompt is answered for Private.
        assert_eq!(
            firewall(&facts(
                true,
                &[(true, PROFILE_PRIVATE), (false, PROFILE_PUBLIC)]
            )),
            Firewall::Allowed
        );
        assert_eq!(firewall(&facts(false, &[])), Firewall::Off);
        assert_eq!(
            firewall(&facts(false, &[(false, PROFILE_PRIVATE)])),
            Firewall::Blocked
        );
    }

    #[test]
    fn public_networks_are_named_in_the_advice() {
        let f = Facts {
            networks: vec![
                Network {
                    name: "HomeNetwork".into(),
                    category: Category::Public,
                },
                Network {
                    name: "Office".into(),
                    category: Category::Domain,
                },
            ],
            firewall_on_private: true,
            rules: vec![],
        };
        assert_eq!(public_networks(&f), ["HomeNetwork"]);
        let a = public_advice(&public_networks(&f));
        assert!(
            a.contains("“HomeNetwork”") && a.contains("Private network"),
            "{a}"
        );
        assert!(public_advice(&["a", "b"]).contains("If it is"));
    }

    #[test]
    fn same_program_expands_variables_and_ignores_case() {
        let exe = Path::new(r"C:\Users\Jo Doe\AppData\Local\Programs\NovaLink\novalink-agent.exe");
        let env =
            |k: &str| (k == "LOCALAPPDATA").then(|| r"C:\Users\Jo Doe\AppData\Local".to_string());
        assert!(same_program(
            r"%LOCALAPPDATA%\Programs\NovaLink\novalink-agent.exe",
            exe,
            env
        ));
        assert!(same_program(
            r"c:\users\jo doe\appdata\local\programs\novalink\NOVALINK-AGENT.EXE",
            exe,
            env
        ));
        assert!(!same_program(r"%OTHER%\novalink-agent.exe", exe, env));
        assert!(!same_program("", exe, env));
    }
}
