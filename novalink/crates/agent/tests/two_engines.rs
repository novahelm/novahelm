#![cfg(feature = "fake")]
use novalink_agent::settings::Settings;
use novalink_agent::testutil::{WakePair, two_engines, wait_until, wait_until_within};
use novalink_core::InputEvent;
use novalink_proto::{Control, DisplayInfo, Edge, EntryPoint, keys};
use std::time::Duration;
use tokio::time::sleep;

#[tokio::test]
async fn cross_type_and_return_leaves_nothing_held() {
    let (a, pa, b, pb) = two_engines().await;
    pa.push_physical_motion_to((1919.5, 540.));
    wait_until(|| a.status().focus == b.status().me).await;
    pa.push_physical_key(keys::LEFT_SHIFT, true);
    pa.push_physical_key(keys::A, true);
    wait_until(|| {
        pb.injected_contains(&InputEvent::Key {
            usage: keys::A,
            down: true,
        })
    })
    .await;
    assert_eq!(pb.last_warp(), Some((2.0, 540.0)));
    pb.push_injected_motion_to((0.5, 540.));
    wait_until(|| a.status().focus == a.status().me).await;
    wait_until(|| pb.held_now().is_empty()).await;
    assert!(!pa.suppressed());
    assert_eq!(b.metrics().safety_releases, 0);
}

#[tokio::test]
async fn touching_b_takes_control_and_releases() {
    let (a, pa, b, pb) = two_engines().await;
    pa.push_physical_motion_to((1919.5, 540.));
    wait_until(|| a.status().focus == b.status().me).await;
    pa.push_physical_key(keys::LEFT_CTRL, true);
    wait_until(|| !pb.held_now().is_empty()).await;
    pb.push_physical_key(keys::B, true);
    wait_until(|| b.status().controller == b.status().me && a.status().controller == b.status().me)
        .await;
    assert!(pb.held_now().iter().all(|k| *k != keys::LEFT_CTRL));
    assert!(!pa.suppressed());
}

#[tokio::test]
async fn killing_focus_returns_cursor_and_counts_safety_release() {
    let (a, pa, b, _pb) = two_engines().await;
    pa.push_physical_motion_to((1919.5, 540.));
    wait_until(|| a.status().focus == b.status().me).await;
    b.shutdown().await;
    wait_until_within(Duration::from_millis(500), || {
        a.status().focus == a.status().me
    })
    .await;
    assert!(!pa.suppressed());
}

#[tokio::test]
async fn killing_focus_with_keys_held_counts_safety_release() {
    let (a, pa, b, _pb) = two_engines().await;
    pa.push_physical_motion_to((1919.5, 540.));
    wait_until(|| a.status().focus == b.status().me).await;
    pa.push_physical_key(keys::A, true);
    sleep(Duration::from_millis(50)).await;
    b.shutdown().await;
    wait_until_within(Duration::from_millis(500), || {
        a.metrics().safety_releases == 1
    })
    .await;
}

#[tokio::test]
async fn controller_dying_releases_injected_keys() {
    let (a, pa, b, pb) = two_engines().await;
    pa.push_physical_motion_to((1919.5, 540.));
    wait_until(|| a.status().focus == b.status().me).await;
    pa.push_physical_key(keys::LEFT_SHIFT, true);
    wait_until(|| !pb.held_now().is_empty()).await;
    a.shutdown().await;
    wait_until_within(Duration::from_millis(500), || {
        pb.held_now().is_empty() && b.status().controller == b.status().me
    })
    .await;
}

#[tokio::test]
async fn display_change_keeps_entry_on_screen() {
    let (a, pa, b, pb) = two_engines().await;
    pb.set_displays(vec![DisplayInfo {
        id: 2,
        x: 0.,
        y: 0.,
        width: 1280.,
        height: 720.,
        scale: 1.,
    }]);
    sleep(Duration::from_secs(3)).await;
    pa.push_physical_motion_to((1919.5, 1070.));
    wait_until(|| a.status().focus == b.status().me).await;
    wait_until(|| pb.last_warp().is_some()).await;
    let (x, y) = pb.last_warp().unwrap();
    assert!((0. ..1280.).contains(&x) && (0. ..720.).contains(&y));
}

#[tokio::test]
async fn latency_is_recorded_per_peer() {
    let (a, pa, b, _pb) = two_engines().await;
    pa.push_physical_motion_to((1919.5, 540.));
    wait_until(|| a.status().focus == b.status().me).await;
    for _ in 0..50 {
        pa.push_physical_motion_delta(1.0, 0.0);
    }
    wait_until(|| {
        b.metrics()
            .per_peer
            .first()
            .map(|p| p.1.count >= 40)
            .unwrap_or(false)
    })
    .await;
    // Loopback, one process clock: the measured one-way latency must be tiny. A clock
    // offset on a different epoch than the capture stamps (Ruling 30) shows up here.
    let hour = b.metrics().per_peer[0].2;
    assert!(hour.count >= 40, "hour window has the samples: {hour:?}");
    assert!(
        hour.p99_us < 50_000,
        "loopback hour p99 must be < 50 ms, got {hour:?}"
    );
}

#[tokio::test]
async fn enter_while_suppressing_leaves_old_focus_first() {
    let (a, pa, b, pb) = two_engines().await;
    pa.push_physical_motion_to((1919.5, 540.));
    wait_until(|| a.status().focus == b.status().me).await;
    pa.push_physical_key(keys::A, true);
    wait_until(|| !pb.held_now().is_empty()).await;
    // B enters A while A is suppressing with B as its focus.
    b.manager()
        .send_control(
            a.status().me,
            &Control::Enter {
                entry: EntryPoint {
                    edge: Edge::Right,
                    frac: 0.5,
                },
            },
        )
        .await
        .unwrap();
    wait_until(|| !pa.suppressed() && pb.held_now().is_empty()).await;
    assert_eq!(a.status().controller, b.status().me);
    assert_eq!(a.status().focus, a.status().me);
}

#[tokio::test]
async fn three_machines_never_have_two_controllers() {
    let [(a, pa), (b, pb), (c, pc)] = novalink_agent::testutil::three_engines().await;
    pa.push_physical_motion_to((1919.5, 540.));
    wait_until(|| a.status().focus == b.status().me).await;
    pa.push_physical_key(keys::LEFT_SHIFT, true);
    wait_until(|| !pb.held_now().is_empty()).await;
    // C's mouse enters B (C is to B's right).
    pc.push_physical_motion_to((0.5, 540.));
    wait_until(|| {
        !pa.suppressed()
            && a.status().controller == c.status().me
            && b.status().controller == c.status().me
            && c.status().focus == b.status().me
    })
    .await;
    assert_eq!(b.status().focus, b.status().me);
    wait_until(|| pb.held_now().is_empty() && pa.held_now().is_empty() && pc.held_now().is_empty())
        .await;
    assert!(pc.suppressed());
}

#[tokio::test]
async fn health_events_dump_the_recorder() {
    let (a, pa, _b, _pb) = two_engines().await;
    pa.push_health_event("hook_reinstalled");
    wait_until(|| {
        a.recent_events(50)
            .iter()
            .any(|e| e["kind"] == "health" && e["event"] == "hook_reinstalled")
    })
    .await;
}

#[tokio::test]
async fn edge_hit_is_sent_once_per_arrival_and_only_with_a_neighbor() {
    let (a, pa, b, pb) = two_engines().await;
    pa.push_physical_motion_to((1919.5, 540.));
    wait_until(|| a.status().focus == b.status().me).await;
    // B's top edge has no neighbor: nothing is sent.
    for x in 0..20 {
        pb.push_injected_motion_to((100. + f64::from(x), 0.5));
    }
    // 100 motions along B's left edge (its neighbor is A): exactly one EdgeHit.
    for y in 0..100 {
        pb.push_injected_motion_to((0.5, 100. + f64::from(y)));
    }
    sleep(Duration::from_millis(200)).await;
    let hits = b
        .recent_events(20_000)
        .iter()
        .filter(|e| e["kind"] == "edge_hit")
        .count();
    assert_eq!(hits, 1);
}

#[tokio::test]
async fn key_up_does_not_claim_control() {
    let (a, pa, b, pb) = two_engines().await;
    pa.push_physical_motion_to((1919.5, 540.));
    wait_until(|| a.status().focus == b.status().me).await;
    pa.push_physical_key(keys::LEFT_CTRL, true);
    wait_until(|| !pb.held_now().is_empty()).await;
    pb.push_physical_key(keys::LEFT_SHIFT, false); // a stray physical release on B
    sleep(Duration::from_millis(100)).await;
    assert_eq!(b.status().controller, a.status().me);
    assert!(!pb.held_now().is_empty());
}

#[tokio::test]
async fn lost_peer_clears_cached_clock_offset() {
    let (a, _pa, b, _pb) = two_engines().await;
    let bid = b.status().me;
    wait_until(|| a.cached_offset(bid).is_some()).await;
    b.shutdown().await;
    wait_until_within(Duration::from_millis(500), || {
        a.cached_offset(bid).is_none()
    })
    .await;
}

#[tokio::test]
async fn engine_panic_while_locked_releases_everything() {
    let (a, pa, b, pb) = two_engines().await;
    pa.push_physical_motion_to((1919.5, 540.));
    wait_until(|| a.status().focus == b.status().me).await;
    pa.push_physical_key(keys::A, true);
    wait_until(|| !pb.held_now().is_empty()).await;
    assert!(pa.suppressed());
    // B is driven by A and holds A; now B's engine dies while holding its lock.
    b.panic_for_test();
    let b = b;
    let exit = tokio::time::timeout(Duration::from_secs(2), b.supervise())
        .await
        .expect("supervisor notices the dead engine");
    assert_eq!(exit, novalink_agent::engine::EngineExit::Panicked);
    assert!(pb.held_now().is_empty());
    assert!(!pb.suppressed());
    // A (controller) still works as a normal machine and is not stuck.
    a.panic_for_test();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(2), a.supervise())
            .await
            .unwrap(),
        novalink_agent::engine::EngineExit::Panicked
    );
    assert!(!pa.suppressed(), "suppressed input must be released");
}

#[tokio::test]
async fn keys_held_locally_across_a_crossing_are_not_forwarded() {
    use novalink_proto::MouseButton;
    let (a, pa, b, pb) = two_engines().await;
    // Pressed while the focus is still local: the local OS got these downs.
    pa.push_physical_motion_to((1000., 540.));
    pa.push_physical_key(keys::A, true);
    pa.push_physical_button(MouseButton::Left, true);
    // Let the engine see those while the cursor is still mid-screen (the fake's cursor
    // moves synchronously; the engine reads it when it handles each event).
    sleep(Duration::from_millis(100)).await;
    pa.push_physical_motion_to((1919.5, 540.));
    wait_until(|| a.status().focus == b.status().me).await;
    // Auto-repeat and the eventual ups of those held inputs, then a marker key.
    pa.push_physical_key(keys::A, true);
    pa.push_physical_key(keys::A, false);
    pa.push_physical_button(MouseButton::Left, false);
    pa.push_physical_key(keys::B, true);
    pa.push_physical_key(keys::B, false);
    let b_up = InputEvent::Key {
        usage: keys::B,
        down: false,
    };
    wait_until(|| pb.injected_contains(&b_up)).await;
    // Controls arrive in order: the marker landed, so anything before it would have.
    for ev in [
        InputEvent::Key {
            usage: keys::A,
            down: true,
        },
        InputEvent::Key {
            usage: keys::A,
            down: false,
        },
        InputEvent::Button {
            button: MouseButton::Left,
            down: false,
        },
    ] {
        assert!(
            !pb.injected_contains(&ev),
            "stray {ev:?} reached the new focus"
        );
    }
    // Once released, a fresh press of the same key is forwarded normally.
    pa.push_physical_key(keys::A, true);
    wait_until(|| {
        pb.injected_contains(&InputEvent::Key {
            usage: keys::A,
            down: true,
        })
    })
    .await;
}

#[tokio::test]
async fn swap_setting_turns_ctrl_into_cmd_and_releases_on_change() {
    let (a, pa, b, pb) = two_engines().await;
    b.set_settings(Settings {
        swap_ctrl_cmd: true,
    })
    .unwrap();
    pa.push_physical_motion_to((1919.5, 540.));
    wait_until(|| a.status().focus == b.status().me).await;
    pa.push_physical_key(keys::LEFT_CTRL, true);
    pa.push_physical_key(keys::C, true);
    wait_until(|| pb.held_now() == vec![keys::LEFT_GUI, keys::C]).await;
    assert!(b.status().settings.swap_ctrl_cmd);

    // Turning it off mid-hold lets nothing stick, and is not a safety release.
    b.set_settings(Settings {
        swap_ctrl_cmd: false,
    })
    .unwrap();
    assert!(pb.held_now().is_empty());
    assert_eq!(b.metrics().safety_releases, 0);
    pa.push_physical_key(keys::C, false);
    pa.push_physical_key(keys::LEFT_CTRL, false);
    pa.push_physical_key(keys::LEFT_CTRL, true);
    wait_until(|| pb.held_now() == vec![keys::LEFT_CTRL]).await;
}

// ---- service channels ----

mod channels {
    use novalink_agent::api::{
        encode_hex, new_nonce, server_proof, verify_forwarder_proof, verify_proof,
    };
    use novalink_agent::channels;
    use novalink_agent::engine::EngineHandle;
    use novalink_agent::testutil::two_engines;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    const TOKEN: &str = "fwd-token";

    async fn forwarder(h: &EngineHandle) -> u16 {
        let l = channels::bind_on(0).await.unwrap();
        let port = l.local_addr().unwrap().port();
        tokio::spawn(channels::serve_on(l, h.clone(), TOKEN.into()));
        port
    }

    async fn line(s: &mut TcpStream) -> String {
        let mut out = Vec::new();
        loop {
            let b = s.read_u8().await.unwrap();
            if b == b'\n' {
                return String::from_utf8(out).unwrap();
            }
            out.push(b);
        }
    }

    async fn connect(port: u16, preface: &str) -> TcpStream {
        let mut s = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
        s.write_all(preface.as_bytes()).await.unwrap();
        s
    }

    #[tokio::test]
    async fn forwarded_bytes_arrive_with_peer_header() {
        let (a, _pa, b, _pb) = two_engines().await;
        let svc = TcpListener::bind("127.0.0.1:0").await.unwrap();
        b.services()
            .set("compute", svc.local_addr().unwrap().port(), "s3cret")
            .unwrap();
        let fport = forwarder(&a).await;
        let (a_id, b_id) = (a.status().me, b.status().me);
        let mut c = connect(fport, &format!("NOVALINK-CONNECT {b_id} compute {TOKEN}\n")).await;
        assert_eq!(line(&mut c).await, "OK");
        c.write_all(b"hello over quic").await.unwrap();
        let (mut srv, _) = svc.accept().await.unwrap();
        assert_eq!(line(&mut srv).await, format!("NOVALINK-FROM {a_id} s3cret"));
        let mut buf = [0u8; 15];
        srv.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"hello over quic");
        srv.write_all(b"and back").await.unwrap();
        let mut back = [0u8; 8];
        c.read_exact(&mut back).await.unwrap();
        assert_eq!(&back, b"and back");
    }

    #[tokio::test]
    async fn bad_token_refused() {
        let (a, _pa, b, _pb) = two_engines().await;
        let fport = forwarder(&a).await;
        let b_id = b.status().me;
        let mut c = connect(fport, &format!("NOVALINK-CONNECT {b_id} compute wrong\n")).await;
        assert_eq!(line(&mut c).await, "ERR unauthorised");
        assert_eq!(c.read(&mut [0u8; 1]).await.unwrap(), 0, "socket closes");
    }

    #[tokio::test]
    async fn unknown_service_refused() {
        let (a, _pa, b, _pb) = two_engines().await;
        let fport = forwarder(&a).await;
        let b_id = b.status().me;
        let mut c = connect(fport, &format!("NOVALINK-CONNECT {b_id} nothing {TOKEN}\n")).await;
        assert_eq!(line(&mut c).await, "ERR no such service");
        assert_eq!(c.read(&mut [0u8; 1]).await.unwrap(), 0);
    }

    #[tokio::test]
    async fn dead_local_service_is_refused() {
        let (a, _pa, b, _pb) = two_engines().await;
        let port = {
            let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
            l.local_addr().unwrap().port()
        };
        b.services().set("compute", port, "x").unwrap();
        let fport = forwarder(&a).await;
        let b_id = b.status().me;
        let mut c = connect(fport, &format!("NOVALINK-CONNECT {b_id} compute {TOKEN}\n")).await;
        assert_eq!(line(&mut c).await, "ERR no such service");
    }

    #[tokio::test]
    async fn malformed_prefaces_are_refused() {
        let (a, _pa, b, _pb) = two_engines().await;
        let fport = forwarder(&a).await;
        let b_id = b.status().me;
        for (p, want) in [
            ("HELLO\n".to_string(), "ERR bad preface"),
            (
                format!("NOVALINK-CONNECT {b_id} compute\n"),
                "ERR bad preface",
            ),
            (
                format!("NOVALINK-CONNECT zz compute {TOKEN}\n"),
                "ERR bad peer",
            ),
            (
                format!("NOVALINK-CONNECT {b_id} Bad_Name {TOKEN}\n"),
                "ERR bad service",
            ),
            (format!("{}\n", "x".repeat(600)), "ERR bad preface"),
        ] {
            let mut c = connect(fport, &p).await;
            assert_eq!(line(&mut c).await, want, "{p:.40}");
        }
    }

    #[tokio::test]
    async fn slow_preface_times_out() {
        let (a, _pa, _b, _pb) = two_engines().await;
        let fport = forwarder(&a).await;
        let mut c = connect(fport, "NOVALINK-CONN").await;
        let l = tokio::time::timeout(std::time::Duration::from_secs(8), line(&mut c)).await;
        assert_eq!(l.unwrap(), "ERR bad preface");
    }

    #[tokio::test]
    async fn unregistering_stops_forwarding() {
        let (a, _pa, b, _pb) = two_engines().await;
        let svc = TcpListener::bind("127.0.0.1:0").await.unwrap();
        b.services()
            .set("compute", svc.local_addr().unwrap().port(), "s")
            .unwrap();
        assert!(b.services().remove("compute"));
        let fport = forwarder(&a).await;
        let b_id = b.status().me;
        let mut c = connect(fport, &format!("NOVALINK-CONNECT {b_id} compute {TOKEN}\n")).await;
        assert_eq!(line(&mut c).await, "ERR no such service");
    }

    // ---- the optional hello: the forwarder proves it holds the token first ----

    /// Sends the hello with a fresh nonce and returns the connection, the nonce and the proof.
    async fn hello(port: u16) -> (TcpStream, [u8; 16], String) {
        let nonce = new_nonce();
        let mut c = connect(port, &format!("NOVALINK-HELLO {}\n", encode_hex(&nonce))).await;
        let answer = line(&mut c).await;
        let proof = answer.strip_prefix("PROOF ").expect("a proof").to_string();
        (c, nonce, proof)
    }

    #[tokio::test]
    async fn hello_is_answered_with_a_proof_then_connect_works() {
        let (a, _pa, b, _pb) = two_engines().await;
        let svc = TcpListener::bind("127.0.0.1:0").await.unwrap();
        b.services()
            .set("compute", svc.local_addr().unwrap().port(), "s3cret")
            .unwrap();
        let fport = forwarder(&a).await;
        let (a_id, b_id) = (a.status().me, b.status().me);
        let (mut c, nonce, proof) = hello(fport).await;
        assert!(verify_forwarder_proof(TOKEN, &nonce, &proof));
        assert!(!verify_forwarder_proof("other-token", &nonce, &proof));
        assert!(!verify_forwarder_proof(TOKEN, &new_nonce(), &proof));
        // Same connection, the usual connect line.
        c.write_all(format!("NOVALINK-CONNECT {b_id} compute {TOKEN}\n").as_bytes())
            .await
            .unwrap();
        assert_eq!(line(&mut c).await, "OK");
        c.write_all(b"ping").await.unwrap();
        let (mut srv, _) = svc.accept().await.unwrap();
        assert_eq!(line(&mut srv).await, format!("NOVALINK-FROM {a_id} s3cret"));
        let mut buf = [0u8; 4];
        srv.read_exact(&mut buf).await.unwrap();
        assert_eq!(&buf, b"ping");
    }

    /// `/v1/hello` answers anyone, so its proofs must be worthless on this port (and the
    /// other way round): a program that took one port can't borrow the other's answer.
    #[tokio::test]
    async fn forwarder_proof_is_not_the_api_proof() {
        let (a, _pa, _b, _pb) = two_engines().await;
        let fport = forwarder(&a).await;
        let (_c, nonce, proof) = hello(fport).await;
        assert_ne!(proof, server_proof(TOKEN, &nonce));
        assert!(!verify_proof(TOKEN, &nonce, &proof));
        assert!(!verify_forwarder_proof(
            TOKEN,
            &nonce,
            &server_proof(TOKEN, &nonce)
        ));
    }

    #[tokio::test]
    async fn bad_hello_nonce_is_refused() {
        let (a, _pa, _b, _pb) = two_engines().await;
        let fport = forwarder(&a).await;
        for (p, want) in [
            ("NOVALINK-HELLO zz\n".to_string(), "ERR bad nonce"),
            ("NOVALINK-HELLO \n".to_string(), "ERR bad nonce"),
            (
                format!("NOVALINK-HELLO {}\n", "ab".repeat(15)),
                "ERR bad nonce",
            ),
            (
                format!("NOVALINK-HELLO {}\n", "ab".repeat(17)),
                "ERR bad nonce",
            ),
            (
                format!("NOVALINK-HELLO {}g\n", "a".repeat(31)),
                "ERR bad nonce",
            ),
            (
                format!("NOVALINK-HELLO {} x\n", "ab".repeat(16)),
                "ERR bad nonce",
            ),
            ("NOVALINK-HELLO\n".to_string(), "ERR bad preface"),
            (
                format!("NOVALINK-HELLO {}\n", "a".repeat(600)),
                "ERR bad preface",
            ),
        ] {
            let mut c = connect(fport, &p).await;
            assert_eq!(line(&mut c).await, want, "{p:.40}");
            assert_eq!(c.read(&mut [0u8; 1]).await.unwrap(), 0, "socket closes");
        }
    }

    #[tokio::test]
    async fn hello_does_not_replace_the_token() {
        let (a, _pa, b, _pb) = two_engines().await;
        let fport = forwarder(&a).await;
        let b_id = b.status().me;
        for (after, want) in [
            (
                format!("NOVALINK-CONNECT {b_id} compute wrong\n"),
                "ERR unauthorised",
            ),
            (
                format!("NOVALINK-CONNECT {b_id} compute\n"),
                "ERR bad preface",
            ),
            // Only one hello per connection: no second proof.
            (
                format!("NOVALINK-HELLO {}\n", "ab".repeat(16)),
                "ERR bad preface",
            ),
            (format!("{}\n", "x".repeat(600)), "ERR bad preface"),
        ] {
            let (mut c, _, _) = hello(fport).await;
            c.write_all(after.as_bytes()).await.unwrap();
            assert_eq!(line(&mut c).await, want, "{after:.40}");
            assert_eq!(c.read(&mut [0u8; 1]).await.unwrap(), 0, "socket closes");
        }
    }

    #[tokio::test]
    async fn slow_preface_after_hello_times_out() {
        let (a, _pa, _b, _pb) = two_engines().await;
        let fport = forwarder(&a).await;
        let started = std::time::Instant::now();
        let (mut c, _, _) = hello(fport).await;
        c.write_all(b"NOVALINK-CONN").await.unwrap();
        let l = tokio::time::timeout(std::time::Duration::from_secs(8), line(&mut c)).await;
        assert_eq!(l.unwrap(), "ERR bad preface");
        // The hello does not buy a second 5 s: both lines share one limit.
        assert!(started.elapsed() < std::time::Duration::from_millis(6500));
    }
}

#[tokio::test]
async fn desktop_switch_releases_without_safety_count() {
    let (a, pa, b, pb) = two_engines().await;
    pa.push_physical_motion_to((1919.5, 540.));
    wait_until(|| a.status().focus == b.status().me).await;
    pa.push_physical_key(keys::LEFT_GUI, true);
    pa.push_physical_key(keys::L, true);
    wait_until(|| pb.held_now().len() == 2).await;
    pb.push_desktop_switch();
    wait_until(|| pb.held_now().is_empty()).await;
    assert_eq!(b.metrics().safety_releases, 0);
    assert!(
        b.recent_events(50)
            .iter()
            .any(|e| e["kind"] == "desktop_switch")
    );
}

#[tokio::test]
async fn pushing_into_a_sleeping_peer_wakes_it_and_crosses_when_it_returns() {
    let mut w = WakePair::new().await;
    let b_id = w.b_id();
    wait_until(|| w.a.status().peers.iter().any(|p| p.can_wake)).await;
    w.stop_b().await;
    wait_until(|| w.a.status().peers.iter().all(|p| !p.connected)).await;
    w.pa.push_physical_motion_to((1919.5, 540.));
    wait_until(|| w.waker.sent() == vec![vec![[2, 0, 0, 0, 0, 0xb]]]).await;
    assert!(w.a.status().peers[0].waking);
    w.start_b().await;
    wait_until(|| w.a.status().focus == b_id).await;
}

#[tokio::test]
async fn returning_peer_does_not_steal_the_pointer_after_it_moved_away() {
    let mut w = WakePair::new().await;
    w.stop_b().await;
    w.pa.push_physical_motion_to((1919.5, 540.));
    wait_until(|| !w.waker.sent().is_empty()).await;
    w.pa.push_physical_motion_to((900., 540.));
    w.start_b().await;
    wait_until(|| w.a.status().peers.iter().all(|p| p.connected && !p.waking)).await;
    assert_eq!(w.a.status().focus, w.a.status().me);
}

#[tokio::test]
async fn wake_fails_after_the_deadline() {
    let mut w = WakePair::new().await;
    w.stop_b().await;
    w.pa.push_physical_motion_to((1919.5, 540.));
    wait_until_within(Duration::from_secs(5), || w.a.status().peers[0].wake_failed).await;
    assert!(
        w.a.recent_events(50)
            .iter()
            .any(|e| e["kind"] == "wake_failed")
    );
}

#[tokio::test]
async fn pointer_is_forced_visible_only_while_a_remote_drives_it() {
    let (a, pa, b, pb) = two_engines().await;
    assert!(!pb.remote_pointer());
    pa.push_physical_motion_to((1919.5, 540.));
    wait_until(|| a.status().focus == b.status().me).await;
    wait_until(|| pb.remote_pointer()).await;
    assert!(!pa.remote_pointer());
    pb.push_injected_motion_to((0.5, 540.));
    wait_until(|| a.status().focus == a.status().me).await;
    wait_until(|| !pb.remote_pointer()).await;
}

#[tokio::test]
async fn shutdown_stops_forcing_the_pointer() {
    let (a, pa, b, pb) = two_engines().await;
    pa.push_physical_motion_to((1919.5, 540.));
    wait_until(|| a.status().focus == b.status().me).await;
    wait_until(|| pb.remote_pointer()).await;
    b.shutdown().await;
    assert!(!pb.remote_pointer());
}
