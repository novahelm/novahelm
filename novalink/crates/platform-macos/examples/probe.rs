//! Manual probe: prints displays + permissions, captures events with their
//! `physical` flag, injects `a` after 3 s, then suppresses input for 5 s.
//! Needs Accessibility + Input Monitoring granted to the launching terminal.

#[cfg(target_os = "macos")]
fn main() -> anyhow::Result<()> {
    use novalink_core::{InputEvent, Platform};
    use novalink_platform_macos::MacPlatform;
    use std::time::Duration;

    let p = std::sync::Arc::new(MacPlatform::new()?);
    println!("displays:");
    for d in p.displays() {
        println!("  {d:?}");
    }
    println!("permissions:");
    for (name, ok) in p.permissions() {
        println!("  {name}: {ok}");
    }
    println!("cursor: {:?}", p.cursor_pos());

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    if let Err(e) = p.start_capture(tx) {
        println!("capture NOT started: {e:#}");
        println!("grant Accessibility and Input Monitoring to your terminal, then re-run.");
        return Ok(());
    }
    println!(
        "capture running; type / move the mouse. Injecting 'a' in 3 s, suppress 5 s after that."
    );

    let q = p.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_secs(3));
        let a = novalink_proto::keys::A;
        let _ = q.inject(&InputEvent::Key {
            usage: a,
            down: true,
        });
        let _ = q.inject(&InputEvent::Key {
            usage: a,
            down: false,
        });
        println!("-- injected 'a' --");
        q.set_suppress(true);
        println!("-- suppress ON (5 s): cursor should be frozen+hidden, typing swallowed --");
        std::thread::sleep(Duration::from_secs(5));
        q.set_suppress(false);
        println!("-- suppress OFF --");
        std::thread::sleep(Duration::from_secs(3));
        std::process::exit(0);
    });

    loop {
        match rx.try_recv() {
            Ok(e) => println!("{:?} physical: {} pos: {:?}", e.event, e.physical, e.pos),
            Err(_) => std::thread::sleep(Duration::from_millis(2)),
        }
    }
}

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("probe is macOS-only");
}
