//! Embeds the app icon in the Windows tray executable.

use std::path::PathBuf;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    embed_icon();
}

/// Gives the Windows executable the NovaGear app icon (brand/icons/novagear.ico, made by build-icons.sh).
fn embed_icon() {
    if std::env::var("CARGO_CFG_TARGET_OS").is_ok_and(|os| os != "windows") {
        return;
    }
    let manifest =
        PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let ico = manifest.join("../../../brand/icons/novagear.ico");
    println!("cargo:rerun-if-changed={}", ico.display());
    let ico = ico
        .canonicalize()
        .expect("brand/icons/novagear.ico is missing");
    let rc = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR")).join("icon.rc");
    let path = ico.display().to_string().replace('\\', "/");
    std::fs::write(&rc, format!("1 ICON \"{path}\"\n")).expect("write icon.rc");
    embed_resource::compile(&rc, embed_resource::NONE)
        .manifest_required()
        .expect("embedding the icon");
}
