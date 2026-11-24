//! Shared test helpers (feature `fake`): engines over loopback with in-memory platforms.

use crate::engine::{Engine, EngineHandle, EngineOpts};
use crate::settings::Settings;
use crate::wake::FakeWaker;
use novalink_core::FakePlatform;
use novalink_net::discovery::Discovered;
use novalink_net::identity::{Identity, MemoryStore};
use novalink_net::manager::{ManagerConfig, Timings};
use novalink_net::trust::{PairedDevice, TrustStore};
use novalink_proto::DeviceId;
use novalink_proto::Edge;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Polls `f` until it is true; panics after `limit`.
pub async fn wait_until_within(limit: Duration, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    loop {
        if f() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "condition not met within {limit:?}"
        );
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
}

pub async fn wait_until(f: impl FnMut() -> bool) {
    wait_until_within(Duration::from_secs(5), f).await
}

fn timings() -> Timings {
    Timings {
        settle: Duration::from_millis(20),
        ..Timings::default()
    }
}

fn opts(dir: &std::path::Path, dry_inject: bool) -> EngineOpts {
    EngineOpts {
        dry_inject,
        recorder_span: Duration::from_secs(30),
        log_dir: dir.join("logs"),
        // Off, so tests see keys exactly as sent; swap tests turn it on.
        settings: Settings {
            swap_ctrl_cmd: false,
        },
        settings_path: dir.join("settings.json"),
        waker: Arc::new(FakeWaker::default()),
        wake_deadline: Duration::from_secs(2),
        wake_gap: Duration::from_millis(500),
        advertise_macs: vec![],
    }
}

fn cfg(identity: Identity, name: &str, trust: TrustStore) -> ManagerConfig {
    ManagerConfig {
        identity,
        trust: Arc::new(Mutex::new(trust)),
        name: name.into(),
        bind: "127.0.0.1:0".parse().unwrap(),
        hello_displays: vec![],
        discovery: false,
    }
}

fn temp_dir() -> std::path::PathBuf {
    // Leaked on purpose: engines outlive the helper and dump into it.
    tempfile::tempdir().expect("tempdir").keep()
}

/// One engine with no peers.
pub async fn one_engine() -> (EngineHandle, FakePlatform) {
    one_engine_with(|_| {}).await
}

/// Like [`one_engine`], with `setup` applied to the platform before the engine starts.
pub async fn one_engine_with(setup: impl FnOnce(&FakePlatform)) -> (EngineHandle, FakePlatform) {
    let dir = temp_dir();
    let id = Identity::load_or_create(&MemoryStore::default()).unwrap();
    let trust = TrustStore::open(&dir.join("trust.json")).unwrap();
    let p = FakePlatform::new();
    setup(&p);
    let e = Engine::start_with(
        Arc::new(p.clone()),
        cfg(id, "solo", trust),
        timings(),
        opts(&dir, false),
    )
    .await
    .unwrap();
    (e, p)
}

/// Two paired engines connected over loopback, B to the right of A, layouts synced.
pub async fn two_engines() -> (EngineHandle, FakePlatform, EngineHandle, FakePlatform) {
    two_engines_dry(false).await
}

/// Like [`two_engines`], with B running in `--dry-inject` mode when `dry_b`.
pub async fn two_engines_dry(
    dry_b: bool,
) -> (EngineHandle, FakePlatform, EngineHandle, FakePlatform) {
    let dir = temp_dir();
    let ida = Identity::load_or_create(&MemoryStore::default()).unwrap();
    let idb = Identity::load_or_create(&MemoryStore::default()).unwrap();
    let (ia, ib) = (ida.device_id(), idb.device_id());
    let mut ta = TrustStore::open(&dir.join("trust-a.json")).unwrap();
    let mut tb = TrustStore::open(&dir.join("trust-b.json")).unwrap();
    let dev = |id, name: &str| PairedDevice {
        id,
        name: name.into(),
        last_addrs: vec![],
        wake_macs: vec![],
    };
    ta.add(dev(ib, "b")).unwrap();
    tb.add(dev(ia, "a")).unwrap();
    let (pa, pb) = (FakePlatform::new(), FakePlatform::new());
    let a = Engine::start_with(
        Arc::new(pa.clone()),
        cfg(ida, "a", ta),
        timings(),
        opts(&dir, false),
    )
    .await
    .unwrap();
    let b = Engine::start_with(
        Arc::new(pb.clone()),
        cfg(idb, "b", tb),
        timings(),
        opts(&dir, dry_b),
    )
    .await
    .unwrap();
    a.manager().hint_peer(Discovered {
        id: ib,
        name: "b".into(),
        addrs: vec![b.manager().local_addr()],
    });
    b.manager().hint_peer(Discovered {
        id: ia,
        name: "a".into(),
        addrs: vec![a.manager().local_addr()],
    });
    let ready = |e: &EngineHandle| {
        let p = e.status().peers;
        !p.is_empty() && p.iter().all(|p| p.connected && p.clock_offset_us.is_some())
    };
    wait_until_within(Duration::from_secs(10), || ready(&a) && ready(&b)).await;
    wait_until(|| a.status().peers.iter().all(|p| p.dry_inject.is_some())).await;
    a.set_layout(Edge::Right, ib);
    let v = a.status().layout.version;
    wait_until(|| b.status().layout.version == v).await;
    // Let the engines' offset caches pick up the clock estimates.
    tokio::time::sleep(Duration::from_millis(250)).await;
    (a, pa, b, pb)
}

/// Three paired engines in a row, A | B | C (B in the middle), fully connected over
/// loopback with layouts synced.
pub async fn three_engines() -> [(EngineHandle, FakePlatform); 3] {
    let dir = temp_dir();
    let ids: Vec<Identity> = (0..3)
        .map(|_| Identity::load_or_create(&MemoryStore::default()).unwrap())
        .collect();
    let dids: Vec<_> = ids.iter().map(|i| i.device_id()).collect();
    let names = ["a", "b", "c"];
    let mut engines = Vec::new();
    for (i, id) in ids.into_iter().enumerate() {
        let mut trust = TrustStore::open(&dir.join(format!("trust-{i}.json"))).unwrap();
        for j in (0..3).filter(|&j| j != i) {
            trust
                .add(PairedDevice {
                    id: dids[j],
                    name: names[j].into(),
                    last_addrs: vec![],
                    wake_macs: vec![],
                })
                .unwrap();
        }
        let p = FakePlatform::new();
        let e = Engine::start_with(
            Arc::new(p.clone()),
            cfg(id, names[i], trust),
            timings(),
            opts(&dir, false),
        )
        .await
        .unwrap();
        engines.push((e, p));
    }
    for i in 0..3 {
        for j in (0..3).filter(|&j| j != i) {
            engines[i].0.manager().hint_peer(Discovered {
                id: dids[j],
                name: names[j].into(),
                addrs: vec![engines[j].0.manager().local_addr()],
            });
        }
    }
    let ready = |e: &EngineHandle| {
        let p = e.status().peers;
        p.len() == 2 && p.iter().all(|p| p.connected && p.clock_offset_us.is_some())
    };
    wait_until_within(Duration::from_secs(10), || {
        engines.iter().all(|(e, _)| ready(e))
    })
    .await;
    engines[0].0.set_layout(Edge::Right, dids[1]);
    wait_until(|| engines.iter().all(|(e, _)| e.status().layout.version == 1)).await;
    engines[1].0.set_layout(Edge::Right, dids[2]);
    wait_until(|| engines.iter().all(|(e, _)| e.status().layout.version == 2)).await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    let [a, b, c]: [_; 3] = engines.try_into().ok().unwrap();
    [a, b, c]
}

/// A's side of a pair whose B can be stopped and restarted, to test waking it.
pub struct WakePair {
    pub a: EngineHandle,
    pub pa: FakePlatform,
    pub waker: FakeWaker,
    pub b: Option<EngineHandle>,
    pub pb: FakePlatform,
    dir: std::path::PathBuf,
    store_b: MemoryStore,
    b_id: DeviceId,
}

/// The MAC B advertises to A.
const B_MAC: [u8; 6] = [2, 0, 0, 0, 0, 0xb];

impl WakePair {
    /// [`two_engines`]' layout, with B advertising [`B_MAC`], A waking with a 2 s
    /// deadline and 0.5 s gap, and both connected.
    pub async fn new() -> WakePair {
        let dir = temp_dir();
        let ida = Identity::load_or_create(&MemoryStore::default()).unwrap();
        let store_b = MemoryStore::default();
        let idb = Identity::load_or_create(&store_b).unwrap();
        let (ia, ib) = (ida.device_id(), idb.device_id());
        let mut ta = TrustStore::open(&dir.join("trust-a.json")).unwrap();
        let mut tb = TrustStore::open(&dir.join("trust-b.json")).unwrap();
        let dev = |id, name: &str| PairedDevice {
            id,
            name: name.into(),
            last_addrs: vec![],
            wake_macs: vec![],
        };
        ta.add(dev(ib, "b")).unwrap();
        tb.add(dev(ia, "a")).unwrap();
        let waker = FakeWaker::default();
        let pa = FakePlatform::new();
        let a = Engine::start_with(
            Arc::new(pa.clone()),
            cfg(ida, "a", ta),
            timings(),
            EngineOpts {
                waker: Arc::new(waker.clone()),
                ..opts(&dir, false)
            },
        )
        .await
        .unwrap();
        let mut w = WakePair {
            a,
            pa,
            waker,
            b: None,
            pb: FakePlatform::new(),
            dir,
            store_b,
            b_id: ib,
        };
        w.launch_b(idb, tb).await;
        let ready = |e: &EngineHandle| {
            let p = e.status().peers;
            !p.is_empty() && p.iter().all(|p| p.connected && p.clock_offset_us.is_some())
        };
        let b = w.b.clone().unwrap();
        wait_until_within(Duration::from_secs(10), || ready(&w.a) && ready(&b)).await;
        w.a.set_layout(Edge::Right, ib);
        let v = w.a.status().layout.version;
        wait_until(|| b.status().layout.version == v).await;
        // A must have B's wake addresses before B is stopped.
        wait_until(|| w.a.status().peers.iter().all(|p| p.can_wake)).await;
        tokio::time::sleep(Duration::from_millis(250)).await;
        w
    }

    pub fn b_id(&self) -> DeviceId {
        self.b_id
    }

    /// Shuts B down, as if its machine went to sleep.
    pub async fn stop_b(&mut self) {
        if let Some(b) = self.b.take() {
            b.shutdown().await;
        }
        // Until A notices, a push at the edge would cross instead of waking.
        wait_until(|| self.a.status().peers.iter().all(|p| !p.connected)).await;
    }

    /// Starts B again with the same identity and trust, on a new address, and points
    /// both sides at each other.
    pub async fn start_b(&mut self) {
        let id = Identity::load_or_create(&self.store_b).unwrap();
        let trust = TrustStore::open(&self.dir.join("trust-b.json")).unwrap();
        self.pb = FakePlatform::new();
        self.launch_b(id, trust).await;
    }

    async fn launch_b(&mut self, id: Identity, trust: TrustStore) {
        let b = Engine::start_with(
            Arc::new(self.pb.clone()),
            cfg(id, "b", trust),
            timings(),
            EngineOpts {
                advertise_macs: vec![B_MAC],
                ..opts(&self.dir, false)
            },
        )
        .await
        .unwrap();
        let ia = self.a.status().me;
        self.a.manager().hint_peer(Discovered {
            id: self.b_id,
            name: "b".into(),
            addrs: vec![b.manager().local_addr()],
        });
        b.manager().hint_peer(Discovered {
            id: ia,
            name: "a".into(),
            addrs: vec![self.a.manager().local_addr()],
        });
        self.b = Some(b);
    }
}
