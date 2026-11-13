//! On macOS, embeds an Info.plist in the novalink-agent binary (`__TEXT,__info_plist`).
//!
//! The agent runs on its own under launchd, outside NovaLink.app's bundle, so macOS reads
//! its identity and its Local Network reason from the binary itself. Without them the
//! system never asks, and discovery and connections to other computers silently fail.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos") {
        return;
    }
    let version = std::env::var("CARGO_PKG_VERSION").expect("CARGO_PKG_VERSION");
    let plist = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>CFBundleIdentifier</key><string>com.novalink.agent</string>
  <key>CFBundleName</key><string>NovaLink</string>
  <key>CFBundleShortVersionString</key><string>{version}</string>
  <key>CFBundleVersion</key><string>{version}</string>
  <key>NSLocalNetworkUsageDescription</key><string>NovaLink finds and connects to your other computers on your home network.</string>
  <key>NSBonjourServices</key><array><string>_novalink._udp</string></array>
</dict>
</plist>
"#
    );
    let out = std::path::PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let path = out.join("Info.plist");
    std::fs::write(&path, plist).expect("write Info.plist");
    // One argument, so a path with spaces is fine.
    println!(
        "cargo:rustc-link-arg-bin=novalink-agent=-Wl,-sectcreate,__TEXT,__info_plist,{}",
        path.display()
    );
}
