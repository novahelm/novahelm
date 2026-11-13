//! Soak mode under network impairment. Its own test binary so that `NOVALINK_IMPAIR`
//! cannot leak into other tests.
#![cfg(feature = "fake")]
use novalink_agent::soak::{self, SoakConfig};
use novalink_agent::testutil::two_engines;
use std::sync::Once;
use std::time::Duration;

fn impair() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        // SAFETY: runs before any engine or session of this process exists, and
        // nothing else in this binary touches the environment concurrently.
        unsafe {
            std::env::set_var(
                "NOVALINK_IMPAIR",
                "latency=5ms,jitter=3ms,loss=0.05,flap=1s/5s",
            );
        }
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn short_soak_under_impairment_has_no_stuck_keys() {
    impair();
    let (a, _pa, b, _pb) = two_engines().await;
    let r = soak::run(&a, Duration::from_secs(20), b.status().me).await;
    eprintln!("{r:?}");
    assert!(r.crossings > 0 && r.keys_sent > 100, "{r:?}");
    assert_eq!(r.stuck_after_release, 0, "{r:?}");
    assert_eq!(r.unanswered_leaves, 0, "{r:?}");
    assert!(r.leaves_lost_to_disconnect <= r.reconnects, "{r:?}");
    assert!(r.reconnects >= 2, "{r:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn frequent_returns_with_keys_held_are_all_reported_clean() {
    impair();
    let (a, _pa, b, pb) = two_engines().await;
    let cfg = SoakConfig {
        return_every: (Duration::from_millis(700), Duration::from_millis(1500)),
        ..SoakConfig::default()
    };
    let r = soak::run_with(&a, Duration::from_secs(20), b.status().me, cfg).await;
    eprintln!("{r:?}");
    assert!(r.leaves >= 3, "{r:?}");
    assert!(r.held_reports >= 3, "receiver must answer Leaves: {r:?}");
    assert_eq!(r.stuck_after_release, 0, "{r:?}");
    assert_eq!(r.unanswered_leaves, 0, "{r:?}");
    assert!(r.leaves_lost_to_disconnect <= r.reconnects, "{r:?}");
    // Whatever the soak left held was released on the receiver.
    assert!(pb.held_now().is_empty());
}
