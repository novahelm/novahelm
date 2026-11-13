use serde::{Deserialize, Serialize};
use std::fmt;

pub const PROTO_VERSION: u16 = 1;

/// Stable identity of a machine: its Ed25519 public key.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct DeviceId(pub [u8; 32]);

impl fmt::Display for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.iter().try_for_each(|b| write!(f, "{b:02x}"))
    }
}

impl fmt::Debug for DeviceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "DeviceId({self})")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    pub fn opposite(self) -> Edge {
        match self {
            Edge::Left => Edge::Right,
            Edge::Right => Edge::Left,
            Edge::Top => Edge::Bottom,
            Edge::Bottom => Edge::Top,
        }
    }
}

/// Where the cursor appears on the receiving machine. `edge` is the edge of the
/// *receiving* machine; `frac` runs 0..=1 from the top or left.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct EntryPoint {
    pub edge: Edge,
    pub frac: f32,
}

/// A display in the OS's native cursor coordinate space.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct DisplayInfo {
    pub id: u32,
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
    pub scale: f64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
    Back,
    Forward,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Link {
    pub from: DeviceId,
    pub from_edge: Edge,
    pub to: DeviceId,
}

/// Unreliable, sequence-numbered messages (QUIC datagrams).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum Datagram {
    Motion {
        seq: u64,
        dx: f32,
        dy: f32,
        t_us: u64,
    },
    Scroll {
        seq: u64,
        dx: f32,
        dy: f32,
        t_us: u64,
    },
    Heartbeat {
        seq: u64,
        t_us: u64,
    },
    HeartbeatAck {
        seq: u64,
        echo_t_us: u64,
        t_us: u64,
    },
}

/// Reliable, ordered messages (the single bidirectional stream).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum Control {
    Hello {
        proto_version: u16,
        name: String,
        displays: Vec<DisplayInfo>,
    },
    Displays(Vec<DisplayInfo>),
    Key {
        usage: u16,
        down: bool,
        t_us: u64,
    },
    Button {
        button: MouseButton,
        down: bool,
        t_us: u64,
    },
    Enter {
        entry: EntryPoint,
    },
    Leave,
    ReleaseAll,
    EdgeHit {
        edge: Edge,
        frac: f32,
    },
    Claim {
        clock: u64,
    },
    Layout {
        version: u64,
        links: Vec<Link>,
    },
    PairConfirm {
        accepted: bool,
    },
    /// Pairing step 1 (requester): sha256 commitment to its nonce.
    PairCommit {
        commit: [u8; 32],
    },
    /// Pairing steps 2 and 3: the nonce behind a commitment.
    PairReveal {
        nonce: [u8; 32],
    },
    /// Sent by the driven side a settle period after a `Leave`: keys and buttons it held
    /// before releasing, and still held after the settle (non-zero = stuck).
    HeldReport {
        held_before_release: u32,
        held_after_settle: u32,
    },
    /// Sent on connect: whether this machine's agent runs with `--dry-inject`.
    PeerInfo {
        dry_inject: bool,
    },
}

/// First message on every service channel (a bidirectional stream a paired peer opens
/// beside the control stream). Framed like control messages; see
/// [`crate::codec::encode_channel_open`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelOpen {
    pub service: String,
}

/// Longest accepted service name, in bytes.
pub const MAX_SERVICE_LEN: usize = 32;

/// A service name is 1-32 bytes of `[a-z0-9-]`.
pub fn valid_service_name(name: &str) -> bool {
    (1..=MAX_SERVICE_LEN).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}
