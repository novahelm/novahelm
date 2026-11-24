//! novalink-service: the SYSTEM-side helper for driving Windows' protected screens.

// Only the Windows service uses these; elsewhere they are exercised by their tests alone.
#[cfg_attr(not(windows), allow(dead_code))]
mod decide;
#[cfg(windows)]
mod helper;
#[cfg(windows)]
mod service;
#[cfg(windows)]
mod winutil;

#[cfg(windows)]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "--helper") {
        if let Err(e) = helper::run() {
            eprintln!("novalink-service --helper: {e:#}");
            std::process::exit(1);
        }
    } else if args.is_empty() {
        if let Err(e) = service::run() {
            eprintln!("novalink-service: {e:#}");
            std::process::exit(1);
        }
    } else {
        eprintln!("usage: novalink-service [--helper]");
        std::process::exit(2);
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("novalink-service runs on Windows");
    std::process::exit(1);
}
