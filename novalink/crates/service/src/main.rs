//! novalink-service: the SYSTEM-side helper for driving Windows' protected screens.

// `decide::helper_action` is wired into the service's session-watching loop in a later
// task; until then it is exercised only by its own tests.
#[allow(dead_code)]
mod decide;
#[cfg(windows)]
mod helper;

#[cfg(windows)]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--helper") {
        if let Err(e) = helper::run() {
            eprintln!("novalink-service --helper: {e:#}");
            std::process::exit(1);
        }
    } else {
        eprintln!("usage: novalink-service --helper");
        std::process::exit(2);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("novalink-service runs on Windows");
    std::process::exit(1);
}
