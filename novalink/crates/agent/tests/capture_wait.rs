//! The agent starts and stays useful while input permissions are missing.

use novalink_agent::engine::CaptureState;
use novalink_agent::testutil::{one_engine_with, wait_until_within};
use std::time::Duration;

#[tokio::test]
async fn capture_starts_after_permission_appears() {
    let (engine, platform) = one_engine_with(|p| p.deny_capture(true)).await;
    assert_eq!(engine.status().capture, CaptureState::WaitingForPermission);
    platform.deny_capture(false);
    wait_until_within(Duration::from_secs(2), || {
        engine.status().capture == CaptureState::Running
    })
    .await;
    // Local input is only recorded while a peer has focus, so the flight recorder's
    // capture entry is the observable here.
    assert!(
        engine
            .recent_events(50)
            .iter()
            .any(|e| e["kind"] == "capture" && e["state"] == "running")
    );
}

#[tokio::test]
async fn failure_with_permissions_granted_is_failed_not_waiting() {
    let (engine, platform) = one_engine_with(|p| {
        p.deny_capture(true);
        p.set_permissions(vec![]);
    })
    .await;
    // The fake reports every listed permission as not granted while denied; with none
    // listed there is nothing missing to wait for.
    assert_eq!(engine.status().capture, CaptureState::Failed);
    platform.deny_capture(false);
    // The slow retry (5 s) still gets there.
    wait_until_within(Duration::from_secs(8), || {
        engine.status().capture == CaptureState::Running
    })
    .await;
}
