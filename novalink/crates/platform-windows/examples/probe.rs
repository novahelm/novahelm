//! Manual probe (same behaviour as the macOS probe): prints displays + permissions,
//! captures events with their `physical` flag, injects `a` after 3 s, then suppresses
//! input for 5 s. Also reports watchdog reinstalls.

#[cfg(windows)]
fn main() -> anyhow::Result<()> {
    use novalink_core::{InputEvent, Platform};
    use novalink_platform_windows::WinPlatform;
    use std::time::Duration;

    let p = std::sync::Arc::new(WinPlatform::new()?);
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
    p.start_capture(tx)?;
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
        println!("-- suppress ON (5 s): cursor pinned, typing/clicks swallowed --");
        std::thread::sleep(Duration::from_secs(5));
        q.set_suppress(false);
        println!("-- suppress OFF --");
        std::thread::sleep(Duration::from_secs(3));
        std::process::exit(0);
    });

    loop {
        match rx.try_recv() {
            Ok(e) => println!("{:?} physical: {} pos: {:?}", e.event, e.physical, e.pos),
            Err(_) => {
                if p.take_hook_reinstalled() {
                    println!("!! watchdog: hook_reinstalled");
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }
}

#[cfg(not(windows))]
fn main() {
    println!("probe is Windows-only");
}
