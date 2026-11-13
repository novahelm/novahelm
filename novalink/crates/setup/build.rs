//! Embeds the three Windows executables setup installs, and the app icon.
//!
//! `NOVALINK_SETUP_PAYLOAD` names a directory holding them. When it is set, every file must
//! be there. When it is unset (tests, `cargo test --workspace` on Windows CI) empty slices
//! stand in, with a warning when building for Windows; that setup refuses to install.

use std::path::PathBuf;

const FILES: [(&str, &str); 3] = [
    ("AGENT", "novalink-agent.exe"),
    ("CLI", "novalink.exe"),
    ("TRAY", "novalink-tray.exe"),
];

fn main() {
    println!("cargo:rerun-if-env-changed=NOVALINK_SETUP_PAYLOAD");
    println!("cargo:rerun-if-changed=build.rs");
    let windows = std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os == "windows");
    let dir = std::env::var_os("NOVALINK_SETUP_PAYLOAD").map(PathBuf::from);

    if windows && dir.is_none() {
        println!(
            "cargo:warning=NOVALINK_SETUP_PAYLOAD is not set, so this NovaLink-Setup.exe has no \
             programs inside it and will refuse to install. Point it at a folder holding \
             novalink-agent.exe, novalink.exe and novalink-tray.exe to build a real one."
        );
    }
    let mut code = String::new();
    for (name, file) in FILES {
        let Some(dir) = dir.as_ref().filter(|_| windows) else {
            code += &format!("pub const {name}: &[u8] = &[];\n");
            continue;
        };
        let path = dir.join(file);
        if !path.is_file() {
            panic!(
                "NOVALINK_SETUP_PAYLOAD ({}) has no {file}. Build the Windows agent, CLI and tray first.",
                dir.display()
            );
        }
        println!("cargo:rerun-if-changed={}", path.display());
        code += &format!(
            "pub const {name}: &[u8] = include_bytes!({:?});\n",
            path.display().to_string()
        );
    }
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR")).join("payload.rs");
    std::fs::write(out, code).expect("write payload.rs");
    embed_icon();
}

/// Gives the Windows executable the NovaLink app icon (brand/icons/novalink.ico, made by build-icons.sh).
fn embed_icon() {
    if std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os != "windows") {
        return;
    }
    let manifest =
        PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let ico = manifest.join("../../../brand/icons/novalink.ico");
    println!("cargo:rerun-if-changed={}", ico.display());
    let ico = ico
        .canonicalize()
        .expect("brand/icons/novalink.ico is missing");
    let rc = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR")).join("icon.rc");
    let path = ico.display().to_string().replace('\\', "/");
    std::fs::write(&rc, format!("1 ICON \"{path}\"\n")).expect("write icon.rc");
    embed_resource::compile(&rc, embed_resource::NONE)
        .manifest_required()
        .expect("embedding the icon");
}
