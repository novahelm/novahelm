//! Ruling 30: every `t_us` a session stamps is on the process-wide `mono_us()` clock, the
//! same clock capture and the engine use. Its own process so `mono_us()`'s epoch is set
//! here, well before the session exists.
use novalink_core::time::mono_us;
use novalink_net::identity::{Identity, MemoryStore};
use novalink_net::session::Session;
use novalink_net::tls::{self, PeerPolicy};
use novalink_proto::{
    Control, ControlDecoder, Datagram, DeviceId, PROTO_VERSION, decode_datagram, encode_control,
    encode_datagram,
};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::timeout;

struct AllowAll;
impl PeerPolicy for AllowAll {
    fn allow(&self, _: &DeviceId) -> bool {
        true
    }
}

fn hello(name: &str) -> Control {
    Control::Hello {
        proto_version: PROTO_VERSION,
        name: name.into(),
        displays: vec![],
    }
}

fn ident() -> Identity {
    Identity::load_or_create(&MemoryStore::default()).unwrap()
}

/// Tolerance for "same clock": loopback scheduling jitter, far below the 400 ms skew.
const SAME_CLOCK_US: i64 = 100_000;

#[tokio::test]
async fn session_timestamps_are_on_the_mono_us_clock() {
    // Start the process clock, then establish the session much later.
    let _ = mono_us();
    tokio::time::sleep(Duration::from_millis(400)).await;

    let (ida, idb) = (ident(), ident());
    let lo = "127.0.0.1:0".parse().unwrap();
    let a = tls::endpoint(&ida, lo, Arc::new(AllowAll)).unwrap();
    let b = tls::endpoint(&idb, lo, Arc::new(AllowAll)).unwrap();
    let baddr = b.local_addr().unwrap();
    let mb = idb.device_id();
    let server = tokio::spawn(async move {
        let conn = b.accept().await.unwrap().await.unwrap();
        let (s, _) = Session::establish(conn, mb, hello("b")).await.unwrap();
        (s, b)
    });

    // A raw peer that speaks the wire protocol and reads the session's stamps directly.
    let conn = a.connect(baddr, "novalink").unwrap().await.unwrap();
    let (mut send, mut recv) = conn.open_bi().await.unwrap();
    send.write_all(&encode_control(&hello("raw")))
        .await
        .unwrap();
    let mut dec = ControlDecoder::default();
    let mut buf = [0u8; 4096];
    loop {
        if let Some(Control::Hello { .. }) = dec.next().unwrap() {
            break;
        }
        let n = recv.read(&mut buf).await.unwrap().unwrap();
        dec.push(&buf[..n]);
    }
    let (_session, _b) = server.await.unwrap();

    conn.send_datagram(
        encode_datagram(&Datagram::Heartbeat {
            seq: 0,
            t_us: mono_us(),
        })
        .into(),
    )
    .unwrap();

    let (mut saw_hb, mut saw_ack) = (false, false);
    while !(saw_hb && saw_ack) {
        let bytes = timeout(Duration::from_secs(2), conn.read_datagram())
            .await
            .expect("session sends heartbeats and acks")
            .unwrap();
        let now = mono_us() as i64;
        match decode_datagram(&bytes).unwrap() {
            Datagram::Heartbeat { t_us, .. } => {
                assert!(
                    (now - t_us as i64).abs() < SAME_CLOCK_US,
                    "heartbeat t_us {t_us} is not on mono_us clock (now {now})"
                );
                saw_hb = true;
            }
            Datagram::HeartbeatAck { t_us, .. } => {
                assert!(
                    (now - t_us as i64).abs() < SAME_CLOCK_US,
                    "ack t_us {t_us} is not on mono_us clock (now {now})"
                );
                saw_ack = true;
            }
            _ => {}
        }
    }
}
