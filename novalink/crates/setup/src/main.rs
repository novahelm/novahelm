//! NovaLink-Setup: installs NovaLink for the current Windows user, or removes it.

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg_attr(not(windows), allow(dead_code))]
mod firewall;
#[cfg_attr(not(windows), allow(dead_code))]
mod plan;

#[cfg(windows)]
mod win;

#[cfg(windows)]
fn main() {
    win::run();
}

#[cfg(not(windows))]
fn main() {
    eprintln!("NovaLink-Setup runs on Windows");
    std::process::exit(1);
}
