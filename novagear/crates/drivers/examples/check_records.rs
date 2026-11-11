//! Checks device record files the way a contribution is checked in CI: each record parses,
//! ids are unique in the file, and there are no unknown keys (usually typos).
//!
//! `cargo run -p novagear-drivers --example check_records -- my-devices.toml [more.toml ...]`
//!
//! With no arguments it checks the built-in files in `crates/drivers/devices/`.

use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("devices");
    let mut files: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    if files.is_empty() {
        files = std::fs::read_dir(&dir)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().is_some_and(|x| x == "toml"))
            .collect();
        files.sort();
    }
    let mut bad = 0;
    for f in &files {
        let text = std::fs::read_to_string(f)?;
        let (problems, notes) = novagear_drivers::devices::check(&text);
        // A built-in file "replaces" its own records; the note only means something for a user file.
        let builtin = f
            .canonicalize()
            .ok()
            .and_then(|p| p.parent().map(PathBuf::from))
            == dir.canonicalize().ok();
        for n in notes.iter().filter(|_| !builtin) {
            println!("{}: note: {n}", f.display());
        }
        for p in &problems {
            println!("{}: error: {p}", f.display());
        }
        if problems.is_empty() {
            println!("{}: ok", f.display());
        }
        bad += problems.len();
    }
    if bad > 0 {
        anyhow::bail!("{bad} problem(s)");
    }
    Ok(())
}
