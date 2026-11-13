//! Typed views of the agent's JSON. The agent is the source of truth; these mirror what it
//! emits. Device ids are 64-character hex strings.

use novalink_proto::Edge;
use serde::Deserialize;

#[derive(Clone, Debug, Deserialize)]
pub struct Status {
    pub me: String,
    pub name: String,
    pub controller: String,
    pub focus: String,
    pub peers: Vec<Peer>,
    pub layout: Layout,
    /// "running", "waiting_for_permission" or "failed". An agent from before capture was
    /// reported sends nothing; it is treated as running, as the Mac app does.
    #[serde(default = "running")]
    pub capture: String,
}

fn running() -> String {
    "running".into()
}

#[derive(Clone, Debug, Deserialize)]
pub struct Peer {
    pub id: String,
    pub name: String,
    pub connected: bool,
    pub rtt_us: Option<u64>,
    /// How the connection runs, e.g. "Ethernet".
    pub path: String,
    pub dry_inject: Option<bool>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct Layout {
    pub version: u64,
    pub links: Vec<Link>,
}

/// The device `to` sits beyond `from_edge` of device `from`.
#[derive(Clone, Debug, Deserialize)]
pub struct Link {
    pub from: String,
    pub from_edge: Edge,
    pub to: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PendingPair {
    pub peer: String,
    pub name: String,
    pub code: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Discovered {
    pub id: String,
    pub name: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Check {
    pub name: String,
    pub ok: bool,
    pub detail: String,
    pub advice: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_from_an_agent_without_capture_is_running() {
        let old = r#"{"me":"a","name":"M","controller":"a","focus":"a","peers":[],
                      "layout":{"version":0,"links":[]}}"#;
        let s: Status = serde_json::from_str(old).unwrap();
        assert_eq!(s.capture, "running");
    }
}
