//! The real agent binary with the firmware flag: it must answer and exit without starting
//! the agent (no port, no deck), so a flag the parent and the binary disagree on fails here
//! and not at runtime. The catalog is a closed local port, so nothing leaves this machine.

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn run(args: &[&str]) -> (std::process::ExitStatus, String, Duration) {
    // A closed port: bind, note it, drop.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let home = std::env::temp_dir().join(format!("novagear-fwchild-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    let t = Instant::now();
    let out = Command::new(env!("CARGO_BIN_EXE_novagear-agent"))
        .args(args)
        .env("HOME", &home)
        .env(
            "NOVAGEAR_TEST_CATALOG_URL",
            format!("http://127.0.0.1:{port}/"),
        )
        .stdin(Stdio::null())
        .output()
        .unwrap();
    (
        out.status,
        String::from_utf8_lossy(&out.stdout).to_string(),
        t.elapsed(),
    )
}

#[test]
fn the_flag_answers_with_one_json_line_and_exits() {
    let (status, out, took) = run(&["--firmware-check"]);
    assert!(status.success(), "exit {status}");
    assert!(took < Duration::from_secs(20), "took {took:?}");
    let v: serde_json::Value = serde_json::from_str(out.trim()).expect("one JSON line");
    assert!(v["failed"]["message"].is_string(), "{out}");
}

#[test]
fn an_etag_starting_with_a_dash_is_accepted() {
    let (status, out, _) = run(&["--firmware-check", "--firmware-etag=-W/\"abc\""]);
    assert!(status.success(), "clap refused the etag: exit {status}");
    assert!(out.contains("failed"), "{out}");
}
