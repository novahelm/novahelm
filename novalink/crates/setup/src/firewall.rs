//! The Windows Firewall rule setup offers to add. Pure, so the command can be tested anywhere.

use base64::Engine;

pub const ASK: &str = "Allow NovaLink through Windows Firewall? NovaLink needs this to find and \
     connect to your other computers on Private networks. Windows will ask for administrator \
     permission.";

/// The agent's default QUIC port (the tray starts it without `--port`).
const AGENT_PORT: u16 = 47474;
const MDNS_PORT: u16 = 5353;

/// A PowerShell script that drops every inbound rule for `exe` (a block wins over any allow,
/// and Windows makes one when its own prompt is dismissed) and lets it receive on its own
/// UDP ports only, on Private and Domain networks. The rule is left behind on uninstall:
/// removing it needs admin again, and a rule for a missing program does nothing.
pub fn script(exe: &str) -> String {
    let p = exe.replace('\'', "''");
    format!(
        "$ErrorActionPreference = 'Stop'\n\
         $p = '{p}'\n\
         Get-NetFirewallApplicationFilter -Program $p -ErrorAction SilentlyContinue | \
         Get-NetFirewallRule | Where-Object Direction -eq Inbound | Remove-NetFirewallRule\n\
         New-NetFirewallRule -DisplayName 'NovaLink' -Direction Inbound -Program $p \
         -Protocol UDP -LocalPort {AGENT_PORT},{MDNS_PORT} -Action Allow -Profile Private,Domain \
         | Out-Null\n"
    )
}

/// powershell.exe arguments that run `script` hidden. -EncodedCommand takes UTF-16LE in
/// base64, so the path needs no command-line quoting.
pub fn powershell_args(script: &str) -> String {
    let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    format!(
        "-NoProfile -NonInteractive -ExecutionPolicy Bypass -WindowStyle Hidden -EncodedCommand {}",
        base64::engine::general_purpose::STANDARD.encode(utf16)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_quotes_the_path_and_allows_private_only() {
        let s = script(r"C:\Users\O'Neil\AppData\Local\Programs\NovaLink\novalink-agent.exe");
        assert!(s.contains(r"$p = 'C:\Users\O''Neil\AppData"), "{s}");
        assert!(s.contains("Remove-NetFirewallRule") && s.contains("-Profile Private,Domain"));
        assert!(s.contains("-Protocol UDP -LocalPort 47474,5353"), "{s}");
        assert!(!s.contains("Public"), "{s}");
    }

    #[test]
    fn args_encode_utf16le_base64() {
        assert!(powershell_args("a").ends_with("-EncodedCommand YQA="));
    }
}
