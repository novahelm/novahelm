use anyhow::Result;
use novalink_net::identity::{Identity, MemoryStore};
use novalink_net::impair::Impairment;
use novalink_net::session::{CloseReason, Session, SessionEvent};
use novalink_net::tls::{self, PeerPolicy};
use novalink_proto::{Control, Datagram, DeviceId, PROTO_VERSION};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::time::{sleep, timeout};

struct AllowAll;
impl PeerPolicy for AllowAll {
    fn allow(&self, _: &DeviceId) -> bool {
        true
    }
}
struct DenyAll;
impl PeerPolicy for DenyAll {
    fn allow(&self, _: &DeviceId) -> bool {
        false
    }
}

fn hello_v(name: &str, v: u16) -> Control {
    Control::Hello {
        proto_version: v,
        name: name.into(),
        displays: vec![],
    }
}
fn hello(name: &str) -> Control {
    hello_v(name, PROTO_VERSION)
}
fn loopback() -> SocketAddr {
    "127.0.0.1:0".parse().unwrap()
}
fn ident() -> Identity {
    Identity::load_or_create(&MemoryStore::default()).unwrap()
}

async fn connect_pair(
    pa: Arc<dyn PeerPolicy>,
    pb: Arc<dyn PeerPolicy>,
    ha: Control,
    hb: Control,
    ia: Option<Impairment>,
) -> (Result<(Session, Control)>, Result<(Session, Control)>) {
    let (ida, idb) = (ident(), ident());
    let (a, b) = (
        tls::endpoint(&ida, loopback(), pa).unwrap(),
        tls::endpoint(&idb, loopback(), pb).unwrap(),
    );
    let baddr = b.local_addr().unwrap();
    let (ma, mb) = (ida.device_id(), idb.device_id());
    let server = tokio::spawn(async move {
        let conn = timeout(Duration::from_secs(3), b.accept())
            .await?
            .ok_or_else(|| anyhow::anyhow!("endpoint closed"))?
            .await?;
        let r = Session::establish(conn, mb, hb).await;
        // keep endpoint alive until the test is done with the session
        std::mem::forget(b);
        r
    });
    let client = async {
        let conn = a.connect(baddr, "novalink")?.await?;
        Session::establish_with(conn, ma, ha, ia).await
    }
    .await;
    let server = server.await.unwrap();
    std::mem::forget(a);
    (client, server)
}

async fn pair_of_endpoints() -> (Session, Session) {
    let (a, b) = connect_pair(
        Arc::new(AllowAll),
        Arc::new(AllowAll),
        hello("a"),
        hello("b"),
        None,
    )
    .await;
    (a.unwrap().0, b.unwrap().0)
}

#[tokio::test]
async fn datagrams_and_controls_flow_both_ways() -> Result<()> {
    let (mut sa, mut sb) = pair_of_endpoints().await;
    sa.send_control(&Control::Key {
        usage: 4,
        down: true,
        t_us: 1,
    })
    .await?;
    sa.send_datagram(&Datagram::Motion {
        seq: 1,
        dx: 3.0,
        dy: 0.0,
        t_us: 2,
    })?;
    let mut got = vec![];
    while got.len() < 2 {
        match timeout(Duration::from_secs(1), sb.events().recv())
            .await?
            .unwrap()
        {
            SessionEvent::Control(c) => got.push(format!("{c:?}")),
            SessionEvent::Datagram(d @ Datagram::Motion { .. }) => got.push(format!("{d:?}")),
            _ => {}
        }
    }
    assert_eq!(got.len(), 2);
    // and the other way
    sb.send_control(&Control::Leave).await?;
    loop {
        if let SessionEvent::Control(Control::Leave) =
            timeout(Duration::from_secs(1), sa.events().recv())
                .await?
                .unwrap()
        {
            break;
        }
    }
    Ok(())
}

#[tokio::test]
async fn hello_is_exchanged_and_peer_id_known() {
    let (a, b) = connect_pair(
        Arc::new(AllowAll),
        Arc::new(AllowAll),
        hello("alpha"),
        hello("beta"),
        None,
    )
    .await;
    let ((sa, hb), (sb, ha)) = (a.unwrap(), b.unwrap());
    assert!(matches!(hb, Control::Hello { ref name, .. } if name == "beta"));
    assert!(matches!(ha, Control::Hello { ref name, .. } if name == "alpha"));
    assert_ne!(sa.peer(), sb.peer());
    assert_eq!(sa.tls_binding(), sb.tls_binding());
}

#[tokio::test]
async fn untrusted_peer_rejected() {
    // dialer distrusts the listener
    let (a, _b) = connect_pair(
        Arc::new(DenyAll),
        Arc::new(AllowAll),
        hello("a"),
        hello("b"),
        None,
    )
    .await;
    assert!(a.is_err());
    // listener distrusts the dialer: the session never establishes
    let (a, b) = connect_pair(
        Arc::new(AllowAll),
        Arc::new(DenyAll),
        hello("a"),
        hello("b"),
        None,
    )
    .await;
    assert!(a.is_err());
    assert!(b.is_err());
}

#[tokio::test]
async fn own_device_id_rejected() {
    let id = ident();
    let ep = tls::endpoint(&id, loopback(), Arc::new(AllowAll)).unwrap();
    let addr = ep.local_addr().unwrap();
    let ep2 = ep.clone();
    tokio::spawn(async move {
        if let Some(i) = ep2.accept().await {
            let _ = i.await;
        }
    });
    let r = async {
        ep.connect(addr, "novalink")?
            .await
            .map_err(anyhow::Error::from)
    }
    .await;
    assert!(r.is_err());
}

#[tokio::test]
async fn proto_version_mismatch_closes() {
    let (a, b) = connect_pair(
        Arc::new(AllowAll),
        Arc::new(AllowAll),
        hello_v("a", PROTO_VERSION + 1),
        hello("b"),
        None,
    )
    .await;
    assert!(a.is_err());
    assert!(b.is_err());
}

#[tokio::test]
async fn heartbeat_timeout_detected_within_400ms() {
    let (sa, mut sb) = pair_of_endpoints().await;
    sleep(Duration::from_millis(250)).await;
    let t = Instant::now();
    sa.freeze_for_test();
    loop {
        if let SessionEvent::Closed(CloseReason::HeartbeatTimeout) =
            sb.events().recv().await.unwrap()
        {
            break;
        }
    }
    assert!(
        t.elapsed() < Duration::from_millis(400),
        "{:?}",
        t.elapsed()
    );
}

#[tokio::test]
async fn clock_offset_and_rtt_available_after_one_second() {
    let (sa, mut sb) = pair_of_endpoints().await;
    sleep(Duration::from_secs(1)).await;
    assert!(sa.rtt_us().unwrap() < 5_000);
    assert!(sa.clock_offset_us().unwrap().abs() < 2_000);
    // a healthy link stays open for a full second
    while let Ok(ev) = sb.events().try_recv() {
        assert!(!matches!(ev, SessionEvent::Closed(_)));
    }
}

#[tokio::test]
async fn stale_motion_dropped_and_loss_counted() {
    let (sa, mut sb) = pair_of_endpoints().await;
    for seq in [1u64, 4, 3] {
        sa.send_datagram(&Datagram::Motion {
            seq,
            dx: 1.0,
            dy: 0.0,
            t_us: 0,
        })
        .unwrap();
        sleep(Duration::from_millis(20)).await;
    }
    let mut seqs = vec![];
    while let Ok(Some(ev)) = timeout(Duration::from_millis(200), sb.events().recv()).await {
        if let SessionEvent::Datagram(Datagram::Motion { seq, .. }) = ev {
            seqs.push(seq);
        }
    }
    assert_eq!(seqs, [1, 4]);
    assert_eq!(sb.lost_datagrams(), 2);
}

#[tokio::test]
async fn peer_drop_reports_peer_closed() {
    let (sa, mut sb) = pair_of_endpoints().await;
    drop(sa);
    loop {
        let ev = timeout(Duration::from_secs(1), sb.events().recv())
            .await
            .unwrap()
            .unwrap();
        if let SessionEvent::Closed(r) = ev {
            assert!(matches!(r, CloseReason::PeerClosed), "{r:?}");
            break;
        }
    }
}

#[tokio::test]
async fn total_loss_impairment_starves_peer() {
    let imp = Impairment {
        loss: 1.0,
        ..Default::default()
    };
    let (a, b) = connect_pair(
        Arc::new(AllowAll),
        Arc::new(AllowAll),
        hello("a"),
        hello("b"),
        Some(imp),
    )
    .await;
    let (_sa, mut sb) = (a.unwrap().0, b.unwrap().0);
    // a's heartbeats are all dropped, so b declares it dead
    loop {
        if let SessionEvent::Closed(CloseReason::HeartbeatTimeout) =
            timeout(Duration::from_secs(2), sb.events().recv())
                .await
                .unwrap()
                .unwrap()
        {
            break;
        }
    }
}

#[test]
fn impairment_parse() -> Result<()> {
    let i = Impairment::parse("latency=20ms,jitter=5ms,loss=0.02,flap=2s/300s")?;
    assert_eq!(
        (i.latency, i.jitter, i.loss, i.flap),
        (
            Duration::from_millis(20),
            Duration::from_millis(5),
            0.02,
            Some((Duration::from_secs(2), Duration::from_secs(300)))
        )
    );
    assert!(Impairment::parse("loss=2").is_err());
    assert!(Impairment::parse("bogus=1").is_err());
    assert!(Impairment::parse("latency=20").is_err());
    Ok(())
}

#[test]
fn flap_window_is_tail_of_each_period() {
    let i = Impairment::parse("flap=2s/10s").unwrap();
    assert!(!i.in_flap(Duration::from_secs(5)));
    assert!(i.in_flap(Duration::from_secs(9)));
    assert!(!i.in_flap(Duration::from_secs(11)));
    assert!(i.in_flap(Duration::from_secs(19)));
}

struct Toggle(std::sync::atomic::AtomicBool);
impl PeerPolicy for Toggle {
    fn allow(&self, _: &DeviceId) -> bool {
        self.0.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[tokio::test]
async fn revoked_peer_cannot_resume() {
    use std::sync::atomic::Ordering::SeqCst;
    let pol = Arc::new(Toggle(true.into()));
    let (ida, idb) = (ident(), ident());
    let a = tls::endpoint(&ida, loopback(), Arc::new(AllowAll)).unwrap();
    let b = tls::endpoint(&idb, loopback(), pol.clone()).unwrap();
    let baddr = b.local_addr().unwrap();
    let b2 = b.clone();
    tokio::spawn(async move {
        while let Some(i) = b2.accept().await {
            if let Ok(c) = i.await {
                std::mem::forget(c);
            }
        }
    });
    let c1 = a.connect(baddr, "novalink").unwrap().await.unwrap();
    // let any post-handshake tickets arrive
    sleep(Duration::from_millis(200)).await;
    c1.close(0u32.into(), b"");
    pol.0.store(false, SeqCst);
    let r = a.connect(baddr, "novalink").unwrap().await;
    let ok = match r {
        Err(_) => false,
        // server-side rejection surfaces once the connection is used
        Ok(c) => {
            timeout(Duration::from_secs(1), c.open_bi())
                .await
                .is_ok_and(|s| s.is_ok())
                && timeout(Duration::from_millis(500), c.closed())
                    .await
                    .is_err()
        }
    };
    assert!(!ok, "revoked peer reconnected");
}

#[tokio::test]
async fn service_name_limits() -> Result<()> {
    let (sa, mut sb) = pair_of_endpoints().await;
    let bad = [
        "",
        "Compute",
        "has space",
        "under_score",
        "é",
        &"a".repeat(33),
    ];
    // The opener refuses bad names without touching the wire.
    for name in bad {
        assert!(sa.open_channel(name).await.is_err(), "opened {name:?}");
    }
    // The acceptor closes streams whose header names a bad service, or is malformed.
    use novalink_proto::{ChannelOpen, encode_channel_open};
    let mut frames: Vec<Vec<u8>> = bad
        .iter()
        .map(|n| {
            encode_channel_open(&ChannelOpen {
                service: n.to_string(),
            })
        })
        .collect();
    frames.push(1000u32.to_le_bytes().to_vec()); // header larger than the 64-byte limit
    frames.push([2u8, 0, 0, 0, 250, 250].to_vec()); // undecodable payload
    for f in frames {
        let (mut send, mut recv) = sa.open_raw_stream_for_test().await?;
        send.write_all(&f).await?;
        let mut buf = [0u8; 8];
        let r = timeout(Duration::from_secs(3), recv.read(&mut buf)).await?;
        assert!(r.is_err(), "acceptor left a bad channel open: {r:?}");
    }
    // None of that reached the application; a good channel still works.
    let (mut send, _recv) = sa.open_channel("ok-1").await?;
    send.write_all(b"x").await?;
    loop {
        if let SessionEvent::Channel { service, .. } =
            timeout(Duration::from_secs(2), sb.events().recv())
                .await?
                .unwrap()
        {
            assert_eq!(service, "ok-1");
            break;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Manager: mesh, reconnect, pairing
// ---------------------------------------------------------------------------

mod mesh {
    use super::*;
    use novalink_net::manager::{Manager, ManagerConfig, ManagerEvent, Timings};
    use novalink_net::trust::{PairedDevice, TrustStore};
    use std::sync::Mutex;
    use tokio::sync::mpsc;

    /// One simulated machine: a persistent identity, trust file and fixed loopback port.
    struct Dev {
        store: MemoryStore,
        dir: tempfile::TempDir,
        port: u16,
        name: String,
        /// Holds the port until the first start so an ephemeral bind cannot take it.
        reserved: Mutex<Option<std::net::UdpSocket>>,
    }

    impl Dev {
        fn new(name: &str) -> Dev {
            // Tests run in parallel: never hand the same free port to two devices.
            static USED: Mutex<Vec<u16>> = Mutex::new(Vec::new());
            let (port, sock) = loop {
                let sock = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
                let port = sock.local_addr().unwrap().port();
                let mut used = USED.lock().unwrap();
                if !used.contains(&port) {
                    used.push(port);
                    break (port, sock);
                }
            };
            Dev {
                store: MemoryStore::default(),
                dir: tempfile::tempdir().unwrap(),
                port,
                name: name.into(),
                reserved: Mutex::new(Some(sock)),
            }
        }
        fn id(&self) -> DeviceId {
            Identity::load_or_create(&self.store).unwrap().device_id()
        }
        fn addr(&self) -> SocketAddr {
            format!("127.0.0.1:{}", self.port).parse().unwrap()
        }
        fn trust(&self) -> Arc<Mutex<TrustStore>> {
            Arc::new(Mutex::new(
                TrustStore::open(&self.dir.path().join("paired.json")).unwrap(),
            ))
        }
        fn trust_peer(&self, other: &Dev) {
            self.trust()
                .lock()
                .unwrap()
                .add(PairedDevice {
                    id: other.id(),
                    name: other.name.clone(),
                    last_addrs: vec![other.addr()],
                })
                .unwrap();
        }
        fn cfg(&self) -> ManagerConfig {
            drop(self.reserved.lock().unwrap().take());
            ManagerConfig {
                identity: Identity::load_or_create(&self.store).unwrap(),
                trust: self.trust(),
                name: self.name.clone(),
                bind: self.addr(),
                hello_displays: vec![],
                discovery: false,
            }
        }
        async fn start(&self) -> (Manager, mpsc::Receiver<ManagerEvent>) {
            Manager::start(self.cfg()).await.unwrap()
        }
        async fn start_with(&self, t: Timings) -> (Manager, mpsc::Receiver<ManagerEvent>) {
            Manager::start_with(self.cfg(), t).await.unwrap()
        }
    }

    type Ev = mpsc::Receiver<ManagerEvent>;

    async fn next_matching<T>(ev: &mut Ev, mut f: impl FnMut(ManagerEvent) -> Option<T>) -> T {
        let fut = async {
            loop {
                let e = ev.recv().await.expect("event channel closed");
                if let Some(v) = f(e) {
                    return v;
                }
            }
        };
        timeout(Duration::from_secs(5), fut)
            .await
            .expect("timed out waiting for event")
    }
    async fn wait_connected(ev: &mut Ev) -> DeviceId {
        next_matching(ev, |e| match e {
            ManagerEvent::Connected { peer, .. } => Some(peer),
            _ => None,
        })
        .await
    }
    async fn wait_lost(ev: &mut Ev) {
        next_matching(ev, |e| matches!(e, ManagerEvent::Lost { .. }).then_some(())).await
    }
    async fn expect_pair_request(ev: &mut Ev) -> String {
        next_matching(ev, |e| match e {
            ManagerEvent::PairRequest { code, .. } => Some(code),
            _ => None,
        })
        .await
    }
    async fn expect_paired(ev: &mut Ev) {
        next_matching(ev, |e| {
            matches!(e, ManagerEvent::Paired { .. }).then_some(())
        })
        .await
    }
    async fn expect_pair_rejected(ev: &mut Ev) {
        next_matching(ev, |e| {
            matches!(e, ManagerEvent::PairRejected { .. }).then_some(())
        })
        .await
    }

    async fn paired_managers() -> (Dev, Manager, Ev, Dev, Manager, Ev) {
        let (a, b) = (Dev::new("alpha"), Dev::new("beta"));
        a.trust_peer(&b);
        b.trust_peer(&a);
        let (ma, ea) = a.start().await;
        let (mb, eb) = b.start().await;
        (a, ma, ea, b, mb, eb)
    }

    async fn unpaired_managers() -> (Dev, Manager, Ev, Dev, Manager, Ev) {
        let (a, b) = (Dev::new("alpha"), Dev::new("beta"));
        let (ma, ea) = a.start().await;
        let (mb, eb) = b.start().await;
        // Discovery is off in tests: tell the requester where the other machine is.
        ma.hint_peer(novalink_net::discovery::Discovered {
            id: b.id(),
            name: "beta".into(),
            addrs: vec![b.addr()],
        });
        (a, ma, ea, b, mb, eb)
    }

    #[tokio::test]
    async fn reconnects_within_one_second_after_restart() {
        let (_a, _ma, mut ea, b, mb, eb) = paired_managers().await;
        wait_connected(&mut ea).await;
        drop((mb, eb));
        wait_lost(&mut ea).await;
        let t = Instant::now();
        let (_mb2, _eb2) = b.start().await;
        wait_connected(&mut ea).await;
        assert!(t.elapsed() < Duration::from_secs(1), "{:?}", t.elapsed());
    }

    #[tokio::test]
    async fn reaches_peer_via_last_addr_without_discovery() {
        let (_a, _ma, mut ea, _b, _mb, mut eb) = paired_managers().await;
        wait_connected(&mut ea).await;
        wait_connected(&mut eb).await;
    }

    #[tokio::test]
    async fn only_one_session_per_pair() {
        let (_a, ma, mut ea, _b, mb, mut eb) = paired_managers().await;
        wait_connected(&mut ea).await;
        wait_connected(&mut eb).await;
        sleep(Duration::from_millis(500)).await;
        assert_eq!(ma.peers().iter().filter(|p| p.connected).count(), 1);
        assert_eq!(mb.peers().iter().filter(|p| p.connected).count(), 1);
    }

    #[tokio::test]
    async fn traffic_flows_between_managed_peers() {
        let (_a, ma, mut ea, b, mb, mut eb) = paired_managers().await;
        wait_connected(&mut ea).await;
        wait_connected(&mut eb).await;
        ma.send_control(b.id(), &Control::Leave).await.unwrap();
        next_matching(&mut eb, |e| {
            matches!(
                e,
                ManagerEvent::Control {
                    c: Control::Leave,
                    ..
                }
            )
            .then_some(())
        })
        .await;
        mb.broadcast(&Control::ReleaseAll).await;
        next_matching(&mut ea, |e| {
            matches!(
                e,
                ManagerEvent::Control {
                    c: Control::ReleaseAll,
                    ..
                }
            )
            .then_some(())
        })
        .await;
        ma.send_datagram(
            b.id(),
            &Datagram::Motion {
                seq: 1,
                dx: 1.0,
                dy: 2.0,
                t_us: 3,
            },
        );
        next_matching(&mut eb, |e| {
            matches!(
                e,
                ManagerEvent::Datagram {
                    d: Datagram::Motion { .. },
                    ..
                }
            )
            .then_some(())
        })
        .await;
        sleep(Duration::from_millis(400)).await;
        let st = &ma.peers()[0];
        assert!(st.connected && st.addr == Some(b.addr()) && st.rtt_us.is_some());
    }

    #[tokio::test]
    async fn pairing_requires_both_and_shows_same_code() {
        let (a, ma, mut ea, b, mb, mut eb) = unpaired_managers().await;
        ma.request_pair(b.id()).await.unwrap();
        let ca = expect_pair_request(&mut ea).await;
        let cb = expect_pair_request(&mut eb).await;
        assert_eq!(ca, cb);
        assert_eq!(ca.len(), 6);
        // One side accepting is not enough.
        ma.answer_pair(b.id(), true).await;
        sleep(Duration::from_millis(200)).await;
        assert!(ma.peers().is_empty() && mb.peers().is_empty());
        mb.answer_pair(a.id(), true).await;
        expect_paired(&mut ea).await;
        expect_paired(&mut eb).await;
        assert_eq!(ma.peers().len(), 1);
        assert_eq!(mb.peers().len(), 1);
        // The pairing session carries on as a normal one.
        wait_connected(&mut ea).await;
        wait_connected(&mut eb).await;
        ma.send_control(b.id(), &Control::Leave).await.unwrap();
        next_matching(&mut eb, |e| {
            matches!(
                e,
                ManagerEvent::Control {
                    c: Control::Leave,
                    ..
                }
            )
            .then_some(())
        })
        .await;
        // and persists
        assert!(a.trust().lock().unwrap().is_trusted(&b.id()));
        assert!(b.trust().lock().unwrap().is_trusted(&a.id()));
    }

    /// A UDP relay in front of `target` that delays each direction by a fixed amount.
    /// Serves a single client, learnt from its first datagram.
    async fn delay_relay(
        target: SocketAddr,
        to_target: Duration,
        to_client: Duration,
    ) -> SocketAddr {
        let front = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
        let back = Arc::new(tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap());
        back.connect(target).await.unwrap();
        let addr = front.local_addr().unwrap();
        let client = Arc::new(Mutex::new(None::<SocketAddr>));
        let (f, b, c) = (front.clone(), back.clone(), client.clone());
        tokio::spawn(async move {
            let mut buf = vec![0u8; 65536];
            while let Ok((n, from)) = f.recv_from(&mut buf).await {
                c.lock().unwrap().get_or_insert(from);
                let (b, pkt) = (b.clone(), buf[..n].to_vec());
                tokio::spawn(async move {
                    sleep(to_target).await;
                    let _ = b.send(&pkt).await;
                });
            }
        });
        tokio::spawn(async move {
            let mut buf = vec![0u8; 65536];
            while let Ok(n) = back.recv(&mut buf).await {
                let Some(to) = *client.lock().unwrap() else {
                    continue;
                };
                let (f, pkt) = (front.clone(), buf[..n].to_vec());
                tokio::spawn(async move {
                    sleep(to_client).await;
                    let _ = f.send_to(&pkt, to).await;
                });
            }
        });
        addr
    }

    /// A peer reachable on several addresses (Ethernet and Wi-Fi) must still pair at the
    /// first attempt (Ruling 48). The two paths are skewed so that the acceptor finishes
    /// its handshake on the slow-inbound path first while the dialer measures the other
    /// as faster: if both paths are dialled, each side keeps a different connection.
    #[tokio::test]
    async fn pairing_works_when_peer_advertises_several_addresses() {
        let (a, ma, mut ea, b, mb, mut eb) = unpaired_managers().await;
        let ms = Duration::from_millis;
        let fast_out = delay_relay(b.addr(), ms(20), ms(0)).await;
        let fast_back = delay_relay(b.addr(), ms(0), ms(30)).await;
        ma.hint_peer(novalink_net::discovery::Discovered {
            id: b.id(),
            name: "beta".into(),
            addrs: vec![fast_out, fast_back],
        });
        ma.request_pair(b.id()).await.unwrap();
        let ca = expect_pair_request(&mut ea).await;
        let cb = expect_pair_request(&mut eb).await;
        assert_eq!(ca, cb);
        ma.answer_pair(b.id(), true).await;
        mb.answer_pair(a.id(), true).await;
        expect_paired(&mut ea).await;
        expect_paired(&mut eb).await;
        assert!(a.trust().lock().unwrap().is_trusted(&b.id()));
        assert!(b.trust().lock().unwrap().is_trusted(&a.id()));
    }

    /// Pairing tries the addresses one at a time, so a dead one only costs its timeout.
    #[tokio::test]
    async fn pairing_skips_a_dead_address() {
        let (_a, ma, mut ea, b, _mb, mut eb) = unpaired_managers().await;
        let dead = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        ma.hint_peer(novalink_net::discovery::Discovered {
            id: b.id(),
            name: "beta".into(),
            addrs: vec![dead.local_addr().unwrap(), b.addr()],
        });
        ma.request_pair(b.id()).await.unwrap();
        assert_eq!(
            expect_pair_request(&mut ea).await,
            expect_pair_request(&mut eb).await
        );
    }

    #[tokio::test]
    async fn pairing_rejected_by_one_side_trusts_nobody() {
        let (a, ma, mut ea, b, mb, mut eb) = unpaired_managers().await;
        ma.request_pair(b.id()).await.unwrap();
        expect_pair_request(&mut eb).await;
        mb.answer_pair(a.id(), false).await;
        expect_pair_rejected(&mut ea).await;
        assert!(ma.peers().is_empty() && mb.peers().is_empty());
        assert!(!a.trust().lock().unwrap().is_trusted(&b.id()));
        assert!(!b.trust().lock().unwrap().is_trusted(&a.id()));
    }

    /// A raw (non-Manager) requester that gets as far as sending a valid PairReveal.
    async fn rogue_pair_start(addr: SocketAddr) -> Result<(Session, quinn::Endpoint)> {
        use novalink_net::pairing::{commitment, new_nonce};
        let rogue = ident();
        let ep = tls::endpoint(&rogue, loopback(), Arc::new(AllowAll))?;
        let conn = ep.connect(addr, "novalink")?.await?;
        let (mut s, _) = Session::establish(conn, rogue.device_id(), hello("rogue")).await?;
        let n = new_nonce();
        let commit = commitment(&rogue.device_id(), &n);
        s.send_control(&Control::PairCommit { commit }).await?;
        loop {
            match timeout(Duration::from_secs(1), s.events().recv()).await? {
                Some(SessionEvent::Control(Control::PairReveal { .. })) => break,
                Some(SessionEvent::Closed(r)) => anyhow::bail!("closed: {r:?}"),
                None => anyhow::bail!("events ended"),
                _ => {}
            }
        }
        s.send_control(&Control::PairReveal { nonce: n }).await?;
        Ok((s, ep))
    }

    #[tokio::test]
    async fn pairing_reveal_that_breaks_commitment_is_aborted() {
        use novalink_net::pairing::{commitment, new_nonce};
        let b = Dev::new("beta");
        let (_mb, mut eb) = b.start().await;
        // A rogue requester commits to one nonce, then reveals another.
        let rogue = ident();
        let ep = tls::endpoint(&rogue, loopback(), Arc::new(AllowAll)).unwrap();
        let conn = ep.connect(b.addr(), "novalink").unwrap().await.unwrap();
        let (mut s, _) = Session::establish(conn, rogue.device_id(), hello("rogue"))
            .await
            .unwrap();
        let (n1, n2) = (new_nonce(), new_nonce());
        let commit = commitment(&rogue.device_id(), &n1);
        s.send_control(&Control::PairCommit { commit })
            .await
            .unwrap();
        loop {
            if let SessionEvent::Control(Control::PairReveal { .. }) =
                s.events().recv().await.unwrap()
            {
                break;
            }
        }
        s.send_control(&Control::PairReveal { nonce: n2 })
            .await
            .unwrap();
        // The acceptor drops the session without ever showing a code.
        loop {
            match timeout(Duration::from_secs(2), s.events().recv())
                .await
                .unwrap()
            {
                Some(SessionEvent::Closed(_)) | None => break,
                _ => {}
            }
        }
        while let Ok(Some(e)) = timeout(Duration::from_millis(200), eb.recv()).await {
            assert!(
                !matches!(
                    e,
                    ManagerEvent::PairRequest { .. } | ManagerEvent::Paired { .. }
                ),
                "a broken commitment must never produce a code: {e:?}"
            );
        }
    }

    #[tokio::test]
    async fn untrusted_peer_cannot_connect_without_pairing() {
        let b = Dev::new("beta");
        let (mb, mut eb) = b.start().await;
        // An untrusted device dials without any pairing handshake and just talks.
        let rogue = ident();
        let ep = tls::endpoint(&rogue, loopback(), Arc::new(AllowAll)).unwrap();
        let conn = ep.connect(b.addr(), "novalink").unwrap().await.unwrap();
        let (mut s, _) = Session::establish(conn, rogue.device_id(), hello("rogue"))
            .await
            .unwrap();
        let _ = s
            .send_control(&Control::Key {
                usage: 4,
                down: true,
                t_us: 1,
            })
            .await;
        // It is closed as "not paired" and nothing reaches the application.
        let reason = loop {
            match timeout(Duration::from_secs(2), s.events().recv())
                .await
                .unwrap()
            {
                Some(SessionEvent::Closed(r)) => break r,
                None => panic!("events ended without Closed"),
                _ => {}
            }
        };
        assert_eq!(reason, CloseReason::NotPaired);
        if let Ok(Some(e)) = timeout(Duration::from_millis(300), eb.recv()).await {
            panic!("untrusted peer produced an application event: {e:?}");
        }
        assert!(mb.peers().is_empty());
        assert!(
            mb.send_control(rogue.device_id(), &Control::Leave)
                .await
                .is_err()
        );
        assert!(b.trust().lock().unwrap().list().is_empty());
    }

    #[tokio::test]
    async fn one_sided_unpair_does_not_loop() {
        // H (higher id) forgot L (lower id); L still trusts H and keeps dialing.
        let (x, y) = (Dev::new("x"), Dev::new("y"));
        let (l, h) = if x.id() < y.id() { (x, y) } else { (y, x) };
        l.trust_peer(&h);
        let (_mh, mut eh) = h.start().await;
        let cfg = l.cfg();
        let l_trust = cfg.trust.clone();
        let (ml, mut el) = Manager::start(cfg).await.unwrap();
        let end = Instant::now() + Duration::from_secs(3);
        if let Ok(Some(e)) = tokio::time::timeout_at(end.into(), eh.recv()).await {
            panic!("H saw an event for a pairing nobody started: {e:?}");
        }
        // (H's channel is quiet; now L's, which has had the same 3 s.)
        if let Ok(Some(e)) = timeout(Duration::from_millis(50), el.recv()).await {
            panic!("L saw an event: {e:?}");
        }
        assert!(!ml.peers()[0].connected);
        assert_eq!(ml.peers()[0].last_close, Some(CloseReason::NotPaired));
        assert!(h.trust().lock().unwrap().list().is_empty());
        // Once L also forgets H, L can pair afresh at once: its stale-trust probes must
        // not have armed H's inbound cool-down.
        l_trust.lock().unwrap().remove(&h.id()).unwrap();
        sleep(Duration::from_millis(300)).await;
        ml.hint_peer(novalink_net::discovery::Discovered {
            id: h.id(),
            name: "h".into(),
            addrs: vec![h.addr()],
        });
        ml.request_pair(h.id()).await.unwrap();
        expect_pair_request(&mut eh).await;
        expect_pair_request(&mut el).await;
    }

    #[tokio::test]
    async fn requester_gets_pair_rejected_when_peer_ignores_commit() {
        let t = Timings {
            pair_step_timeout: Duration::from_millis(300),
            ..Timings::default()
        };
        let a = Dev::new("alpha");
        let (ma, mut ea) = a.start_with(t).await;
        // A peer that completes the session but never answers PairCommit.
        let silent = ident();
        let ep =
            tls::endpoint(&silent, "127.0.0.1:0".parse().unwrap(), Arc::new(AllowAll)).unwrap();
        let addr = ep.local_addr().unwrap();
        let sid = silent.device_id();
        let srv = tokio::spawn(async move {
            let conn = ep.accept().await.unwrap().await.unwrap();
            let r = Session::establish(conn, sid, hello("silent")).await;
            sleep(Duration::from_secs(3)).await;
            drop(r);
            drop(ep);
        });
        ma.hint_peer(novalink_net::discovery::Discovered {
            id: sid,
            name: "silent".into(),
            addrs: vec![addr],
        });
        ma.request_pair(sid).await.unwrap();
        let t0 = Instant::now();
        expect_pair_rejected(&mut ea).await;
        assert!(t0.elapsed() < Duration::from_secs(1), "{:?}", t0.elapsed());
        srv.abort();
    }

    #[tokio::test]
    async fn pairing_deadline_is_not_extended_by_traffic_and_source_cools_down() {
        let t = Timings {
            pair_timeout: Duration::from_millis(800),
            pair_step_timeout: Duration::from_millis(300),
            pair_cooldown: Duration::from_millis(900),
            ..Timings::default()
        };
        let b = Dev::new("beta");
        let (_mb, mut eb) = b.start_with(t).await;
        let start = Instant::now();
        let (s, _ep) = rogue_pair_start(b.addr()).await.unwrap();
        next_matching(&mut eb, |e| {
            matches!(e, ManagerEvent::PairRequest { .. }).then_some(())
        })
        .await;
        // Keep the pairing "alive" with a stream of confirmations.
        let s = Arc::new(s);
        let keep = {
            let s = s.clone();
            tokio::spawn(async move {
                loop {
                    let _ = s
                        .send_control(&Control::PairConfirm { accepted: true })
                        .await;
                    sleep(Duration::from_millis(100)).await;
                }
            })
        };
        expect_pair_rejected(&mut eb).await;
        keep.abort();
        assert!(
            start.elapsed() < Duration::from_millis(1600),
            "pairing outlived its absolute deadline: {:?}",
            start.elapsed()
        );
        // The source is now cooling down: a fresh attempt is refused.
        assert!(rogue_pair_start(b.addr()).await.is_err());
        assert!(
            timeout(Duration::from_millis(100), eb.recv())
                .await
                .is_err(),
            "cooled-down source must not raise a pairing prompt"
        );
        // After the cool-down it may try again.
        sleep(Duration::from_millis(900)).await;
        let _keep = rogue_pair_start(b.addr()).await.unwrap();
        next_matching(&mut eb, |e| {
            matches!(e, ManagerEvent::PairRequest { .. }).then_some(())
        })
        .await;
    }

    #[tokio::test]
    async fn pairing_that_never_reveals_frees_the_slot() {
        let t = Timings {
            pair_step_timeout: Duration::from_millis(200),
            pair_cooldown: Duration::from_millis(1),
            ..Timings::default()
        };
        let b = Dev::new("beta");
        let (_mb, mut eb) = b.start_with(t).await;
        // Commit, then go silent.
        let rogue = ident();
        let ep = tls::endpoint(&rogue, loopback(), Arc::new(AllowAll)).unwrap();
        let conn = ep.connect(b.addr(), "novalink").unwrap().await.unwrap();
        let (mut s, _) = Session::establish(conn, rogue.device_id(), hello("rogue"))
            .await
            .unwrap();
        s.send_control(&Control::PairCommit { commit: [9; 32] })
            .await
            .unwrap();
        loop {
            match timeout(Duration::from_secs(2), s.events().recv())
                .await
                .unwrap()
            {
                Some(SessionEvent::Closed(_)) | None => break,
                _ => {}
            }
        }
        sleep(Duration::from_millis(50)).await;
        let _ok = rogue_pair_start(b.addr()).await.unwrap();
        next_matching(&mut eb, |e| {
            matches!(e, ManagerEvent::PairRequest { .. }).then_some(())
        })
        .await;
    }

    fn cooling_timings() -> Timings {
        Timings {
            pair_step_timeout: Duration::from_millis(200),
            pair_cooldown: Duration::from_secs(5),
            ..Timings::default()
        }
    }

    /// Waits until the raw session `s` is closed by the far side.
    async fn wait_session_closed(s: &mut Session) -> Option<CloseReason> {
        loop {
            match timeout(Duration::from_secs(3), s.events().recv())
                .await
                .expect("session was not closed")
            {
                Some(SessionEvent::Closed(r)) => return Some(r),
                None => return None,
                _ => {}
            }
        }
    }

    /// Asserts a fresh pairing attempt from loopback is refused and raises no prompt.
    async fn assert_cooling_down(b: &Dev, eb: &mut Ev) {
        assert!(
            rogue_pair_start(b.addr()).await.is_err(),
            "source should be cooling down"
        );
        while let Ok(Some(e)) = timeout(Duration::from_millis(100), eb.recv()).await {
            assert!(
                !matches!(e, ManagerEvent::PairRequest { .. }),
                "cooled-down source raised a pairing prompt"
            );
        }
    }

    #[tokio::test]
    async fn idle_inbound_connector_that_never_commits_is_cooled_down() {
        let b = Dev::new("beta");
        let (_mb, mut eb) = b.start_with(cooling_timings()).await;
        // Completes the session, then never sends PairCommit: it held the single
        // inbound pairing slot until the step timeout.
        let rogue = ident();
        let ep = tls::endpoint(&rogue, loopback(), Arc::new(AllowAll)).unwrap();
        let conn = ep.connect(b.addr(), "novalink").unwrap().await.unwrap();
        let (mut s, _) = Session::establish(conn, rogue.device_id(), hello("idle"))
            .await
            .unwrap();
        wait_session_closed(&mut s).await;
        sleep(Duration::from_millis(50)).await;
        assert_cooling_down(&b, &mut eb).await;
    }

    #[tokio::test]
    async fn inbound_that_never_says_hello_is_cooled_down() {
        let b = Dev::new("beta");
        let (_mb, mut eb) = b.start_with(cooling_timings()).await;
        // TLS completes but no control stream or Hello ever arrives: establish fails.
        let rogue = ident();
        let ep = tls::endpoint(&rogue, loopback(), Arc::new(AllowAll)).unwrap();
        let conn = ep.connect(b.addr(), "novalink").unwrap().await.unwrap();
        timeout(Duration::from_secs(4), conn.closed())
            .await
            .expect("acceptor gives up on a silent connection");
        sleep(Duration::from_millis(50)).await;
        assert_cooling_down(&b, &mut eb).await;
    }

    #[tokio::test]
    async fn inbound_that_opens_with_other_traffic_is_cooled_down() {
        let b = Dev::new("beta");
        let (_mb, mut eb) = b.start_with(cooling_timings()).await;
        let rogue = ident();
        let ep = tls::endpoint(&rogue, loopback(), Arc::new(AllowAll)).unwrap();
        let conn = ep.connect(b.addr(), "novalink").unwrap().await.unwrap();
        let (mut s, _) = Session::establish(conn, rogue.device_id(), hello("rogue"))
            .await
            .unwrap();
        s.send_control(&Control::Leave).await.unwrap();
        assert_eq!(
            wait_session_closed(&mut s).await,
            Some(CloseReason::NotPaired)
        );
        sleep(Duration::from_millis(50)).await;
        assert_cooling_down(&b, &mut eb).await;
    }

    /// One stale-trust settle probe from a fresh raw connection, sent `after` the
    /// session is up; returns how the acceptor closed it.
    async fn stale_probe(addr: SocketAddr, after: Duration) -> Option<CloseReason> {
        let rogue = ident();
        let ep = tls::endpoint(&rogue, loopback(), Arc::new(AllowAll)).unwrap();
        let conn = ep.connect(addr, "novalink").unwrap().await.unwrap();
        let (mut s, _) = Session::establish(conn, rogue.device_id(), hello("stale"))
            .await
            .unwrap();
        sleep(after).await;
        s.send_control(&Control::PairConfirm { accepted: false })
            .await
            .unwrap();
        let r = wait_session_closed(&mut s).await;
        sleep(Duration::from_millis(50)).await;
        r
    }

    #[tokio::test]
    async fn stale_trust_probe_is_not_cooled_down() {
        let b = Dev::new("beta");
        let (_mb, mut eb) = b.start_with(cooling_timings()).await;
        // What a peer that still trusts us sends first: the settle probe. Up to three
        // per source per minute are exempt from the cool-down (Ruling 47).
        for _ in 0..3 {
            assert_eq!(
                stale_probe(b.addr(), Duration::ZERO).await,
                Some(CloseReason::NotPaired)
            );
        }
        // A real pairing from the same address goes ahead at once.
        let _ok = rogue_pair_start(b.addr()).await.unwrap();
        next_matching(&mut eb, |e| {
            matches!(e, ManagerEvent::PairRequest { .. }).then_some(())
        })
        .await;
    }

    #[tokio::test]
    async fn fourth_stale_probe_within_a_minute_is_cooled_down() {
        let b = Dev::new("beta");
        let (_mb, mut eb) = b.start_with(cooling_timings()).await;
        for _ in 0..4 {
            assert_eq!(
                stale_probe(b.addr(), Duration::ZERO).await,
                Some(CloseReason::NotPaired)
            );
        }
        assert_cooling_down(&b, &mut eb).await;
    }

    #[tokio::test]
    async fn late_stale_probe_is_cooled_down() {
        // A step timeout long enough that the late probe, not the timeout, ends it.
        let t = Timings {
            pair_step_timeout: Duration::from_secs(3),
            ..cooling_timings()
        };
        let b = Dev::new("beta");
        let (_mb, mut eb) = b.start_with(t).await;
        // Held the inbound slot, then dressed up as a stale-trust probe.
        assert_eq!(
            stale_probe(b.addr(), Duration::from_millis(700)).await,
            Some(CloseReason::NotPaired)
        );
        assert_cooling_down(&b, &mut eb).await;
    }

    #[tokio::test]
    async fn stale_probe_after_a_stalled_hello_is_cooled_down() {
        use novalink_proto::{ControlDecoder, encode_control};
        let t = Timings {
            pair_step_timeout: Duration::from_secs(3),
            ..cooling_timings()
        };
        let b = Dev::new("beta");
        let (_mb, mut eb) = b.start_with(t).await;
        // Takes the inbound slot, stalls the Hello exchange, then probes "promptly"
        // after establish: the 500 ms window runs from slot acquisition (Ruling 47).
        let rogue = ident();
        let ep = tls::endpoint(&rogue, loopback(), Arc::new(AllowAll)).unwrap();
        let conn = ep.connect(b.addr(), "novalink").unwrap().await.unwrap();
        sleep(Duration::from_millis(700)).await;
        let (mut send, mut recv) = conn.open_bi().await.unwrap();
        send.write_all(&encode_control(&hello("stale")))
            .await
            .unwrap();
        let mut dec = ControlDecoder::default();
        let mut buf = [0u8; 4096];
        while !matches!(dec.next(), Ok(Some(Control::Hello { .. }))) {
            let n = recv.read(&mut buf).await.unwrap().unwrap();
            dec.push(&buf[..n]);
        }
        send.write_all(&encode_control(&Control::PairConfirm { accepted: false }))
            .await
            .unwrap();
        timeout(Duration::from_secs(2), conn.closed())
            .await
            .expect("acceptor closes the probe");
        sleep(Duration::from_millis(50)).await;
        assert_cooling_down(&b, &mut eb).await;
    }

    #[tokio::test]
    async fn channel_round_trip() {
        let (a, ma, mut ea, b, mb, mut eb) = paired_managers().await;
        wait_connected(&mut ea).await;
        wait_connected(&mut eb).await;
        let (mut asend, mut arecv) = ma.open_channel(b.id(), "compute").await.unwrap();
        asend.write_all(b"ping").await.unwrap();
        let (peer, service, mut bsend, mut brecv) = next_matching(&mut eb, |e| match e {
            ManagerEvent::Channel {
                peer,
                service,
                send,
                recv,
            } => Some((peer, service, send, recv)),
            _ => None,
        })
        .await;
        assert_eq!(peer, a.id());
        assert_eq!(service, "compute");
        let mut buf = [0u8; 4];
        brecv.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");
        bsend.write_all(b"pong").await.unwrap();
        arecv.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"pong");
        drop(mb);
    }

    #[tokio::test]
    async fn channel_refused_when_unpaired() {
        let (_a, ma, _ea, b, _mb, mut eb) = unpaired_managers().await;
        assert!(ma.open_channel(b.id(), "compute").await.is_err());
        sleep(Duration::from_millis(300)).await;
        while let Ok(e) = eb.try_recv() {
            assert!(!matches!(e, ManagerEvent::Channel { .. }), "{e:?}");
        }
    }

    #[tokio::test]
    async fn manager_refuses_bad_service_names() {
        let (_a, ma, mut ea, b, _mb, mut eb) = paired_managers().await;
        wait_connected(&mut ea).await;
        wait_connected(&mut eb).await;
        for name in ["", "Bad", "a b", &"a".repeat(33)] {
            assert!(ma.open_channel(b.id(), name).await.is_err(), "{name:?}");
        }
    }

    #[tokio::test]
    async fn cannot_pair_with_self() {
        let a = Dev::new("alpha");
        let (ma, _ea) = a.start().await;
        assert!(ma.request_pair(a.id()).await.is_err());
    }
}
