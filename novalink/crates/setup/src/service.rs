//! The lock-screen service setup offers to install under Program Files. Pure script builders,
//! so the commands can be tested anywhere.

use crate::firewall;
use sha2::{Digest, Sha256};

pub const ASK: &str = "Let NovaLink work on the PC's lock and PIN screens? This installs a small \
     system service. Windows will ask for administrator permission.";

pub const ASK_REMOVE: &str = "Also remove NovaLink's lock-screen service? Windows will ask for \
     administrator permission.";

const SERVICE_EXE: &str = "novalink-service.exe";
const AGENT_EXE: &str = "novalink-agent.exe";

/// The file names the install script copies out of its payload folder.
pub const PAYLOAD_FILES: [&str; 2] = [SERVICE_EXE, AGENT_EXE];

/// Hex SHA-256, which is how the install script is told what it should have copied.
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Setup has nothing to offer when the installed copy is this version and the service is
/// registered. version.txt is written last by the install script, so it marks a finished one.
pub fn up_to_date(
    installed_version: Option<&str>,
    payload_version: &str,
    service_exists: bool,
) -> bool {
    service_exists && installed_version.is_some_and(|v| v.trim() == payload_version)
}

fn quote(s: &str) -> String {
    firewall::ps_quote(s)
}

/// Stops the service (waiting until it is stopped) and then ends any NovaLink program still
/// running from Program Files, so neither a copy nor a delete meets a locked exe.
const STOP_AND_CLEAR: &str = "\
$pf = Join-Path $env:ProgramFiles 'NovaLink'
$pfPrefix = $pf.TrimEnd('\\') + '\\'
$inPf = { Get-Process -Name novalink-service,novalink-agent -ErrorAction SilentlyContinue | \
Where-Object { $_.Path -and $_.Path.StartsWith($pfPrefix, [StringComparison]::OrdinalIgnoreCase) } }
$existing = Get-Service -Name NovaLinkService -ErrorAction SilentlyContinue
if ($existing -and $existing.Status -ne 'Stopped') {
  Stop-Service -Name NovaLinkService -Force
  $existing.WaitForStatus('Stopped', [TimeSpan]::FromSeconds(30))
}
& $inPf | ForEach-Object {
  try { Stop-Process -Id $_.Id -Force -ErrorAction Stop; $_.WaitForExit(10000) | Out-Null } catch { }
}
if (& $inPf) { throw 'NovaLink is still running from Program Files and would not stop' }
";

/// Installs `novalink-service.exe` and `novalink-agent.exe` into `%ProgramFiles%\NovaLink`,
/// registers the service and starts it, and lets the Program Files agent through the
/// firewall, all under one elevation. `payload_dir` is a folder a normal user can write to,
/// so each file is checked against the SHA-256 setup computed from its own embedded copy
/// *after* it has been copied to where only an administrator can write.
pub fn install_script(
    payload_dir: &str,
    version: &str,
    service_sha256: &str,
    agent_sha256: &str,
) -> String {
    let fw = firewall::script_for("(Join-Path $pf 'novalink-agent.exe')");
    let mut s = String::from("$ErrorActionPreference = 'Stop'\n");
    s += &format!("$src = '{}'\n", quote(payload_dir));
    s += STOP_AND_CLEAR;
    // A repair that fails part way must not leave the old version.txt saying it finished.
    s += "Remove-Item -LiteralPath (Join-Path $pf 'version.txt') -Force -ErrorAction SilentlyContinue\n";
    s += &format!(
        "$bin = '\"' + (Join-Path $pf '{SERVICE_EXE}') + '\"'\n\
         New-Item -ItemType Directory -Force -Path $pf | Out-Null\n\
         foreach ($f in @(@('{SERVICE_EXE}', '{svc}'), @('{AGENT_EXE}', '{agent}'))) {{\n\
         \x20 $to = Join-Path $pf $f[0]\n\
         \x20 Copy-Item -LiteralPath (Join-Path $src $f[0]) -Destination $to -Force\n\
         \x20 if ((Get-FileHash -LiteralPath $to -Algorithm SHA256).Hash -ne $f[1]) {{\n\
         \x20   Remove-Item -LiteralPath $to -Force\n\
         \x20   throw \"$($f[0]) is not the file inside NovaLink-Setup, so it was not installed\"\n\
         \x20 }}\n\
         }}\n\
         $cim = Get-CimInstance -ClassName Win32_Service -Filter \"Name='NovaLinkService'\"\n\
         if (-not $cim) {{\n\
         \x20 New-Service -Name NovaLinkService -DisplayName NovaLink -BinaryPathName $bin \
         -StartupType Automatic | Out-Null\n\
         }} else {{\n\
         \x20 if ($cim.PathName -ne $bin) {{\n\
         \x20   $r = Invoke-CimMethod -InputObject $cim -MethodName Change -Arguments @{{ PathName = $bin }}\n\
         \x20   if ($r.ReturnValue -ne 0) {{ throw \"Windows would not reset the service path (code $($r.ReturnValue))\" }}\n\
         \x20 }}\n\
         \x20 Set-Service -Name NovaLinkService -StartupType Automatic\n\
         }}\n\
         $cim = Get-CimInstance -ClassName Win32_Service -Filter \"Name='NovaLinkService'\"\n\
         if ($cim.PathName -ne $bin) {{ throw 'The NovaLink service does not point at Program Files' }}\n\
         sc.exe failure NovaLinkService reset= 60 actions= restart/1000/restart/5000/restart/30000 | Out-Null\n\
         if ($LASTEXITCODE -ne 0) {{ throw \"sc.exe failure stopped with code $LASTEXITCODE\" }}\n",
        svc = quote(service_sha256),
        agent = quote(agent_sha256),
    );
    s += &fw;
    s += "Start-Service -Name NovaLinkService\n";
    // Last, so version.txt means the rest finished.
    s += &format!(
        "Set-Content -LiteralPath (Join-Path $pf 'version.txt') -Value '{}' -Encoding ASCII\n",
        quote(version)
    );
    s
}

/// Stops and deletes the service and removes `%ProgramFiles%\NovaLink`. The rule in Windows
/// Firewall stays (a rule for a missing program does nothing) and so does `%ProgramData%\NovaLink`.
pub fn uninstall_script() -> String {
    let mut s = String::from("$ErrorActionPreference = 'Stop'\n");
    s += STOP_AND_CLEAR;
    s += "if ($existing) {\n\
          \x20 sc.exe delete NovaLinkService | Out-Null\n\
          \x20 if ($LASTEXITCODE -ne 0) { throw \"sc.exe delete stopped with code $LASTEXITCODE\" }\n\
          }\n\
          if (Test-Path -LiteralPath $pf) { Remove-Item -LiteralPath $pf -Recurse -Force }\n";
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    const SVC: &str = "aa11";
    const AGENT: &str = "bb22";

    #[test]
    fn install_script_targets_program_files_and_quotes() {
        let s = install_script(r"C:\Users\O'Neil\Temp\nl", "0.2.0", SVC, AGENT);
        assert!(s.contains(r"$src = 'C:\Users\O''Neil\Temp\nl'"), "{s}");
        assert!(s.contains("$env:ProgramFiles 'NovaLink'"), "{s}");
        assert!(s.contains("NovaLinkService"), "{s}");
        assert!(s.contains("-StartupType Automatic"), "{s}");
        assert!(s.contains("restart/1000/restart/5000/restart/30000"), "{s}");
        // The firewall part is for the Program Files agent, not the per-user copy.
        assert!(
            s.contains("$p = (Join-Path $pf 'novalink-agent.exe')"),
            "{s}"
        );
        assert!(
            s.contains("New-NetFirewallRule -DisplayName 'NovaLink'"),
            "{s}"
        );
        assert!(!s.contains("Programs"), "{s}");
        // The binary path is the quoted Program Files exe.
        assert!(
            s.contains("$bin = '\"' + (Join-Path $pf 'novalink-service.exe') + '\"'"),
            "{s}"
        );
        assert!(s.contains("-BinaryPathName $bin"), "{s}");
    }

    #[test]
    fn install_script_doubles_curly_quotes_too() {
        let s = install_script("C:\\Users\\O\u{2019}Neil\\Temp", "1", SVC, AGENT);
        assert!(
            s.contains("$src = 'C:\\Users\\O\u{2019}\u{2019}Neil\\Temp'"),
            "{s}"
        );
    }

    #[test]
    fn install_script_clears_version_before_copying_and_checks_before_registering() {
        let s = install_script("C:\\p", "1", SVC, AGENT);
        let rm = s
            .find("Remove-Item -LiteralPath (Join-Path $pf 'version.txt')")
            .unwrap();
        assert!(
            rm > s.find("throw 'NovaLink is still running").unwrap(),
            "{s}"
        );
        assert!(rm < s.find("Copy-Item").unwrap(), "{s}");
        let throw = s.find("is not the file inside NovaLink-Setup").unwrap();
        assert!(
            throw < s.find("New-Service").unwrap() && throw < s.find("Start-Service").unwrap(),
            "{s}"
        );
    }

    #[test]
    fn install_script_resets_a_service_that_points_elsewhere() {
        let s = install_script("C:\\p", "1", SVC, AGENT);
        assert!(s.contains("$cim.PathName -ne $bin"), "{s}");
        assert!(s.contains("-MethodName Change"), "{s}");
        // And refuses to go on if it still does.
        assert!(s.contains("does not point at Program Files"), "{s}");
    }

    #[test]
    fn install_script_stops_the_service_and_clears_exes_before_copying() {
        let s = install_script("C:\\p", "1", SVC, AGENT);
        let stop = s
            .find("WaitForStatus('Stopped'")
            .expect("waits for stopped");
        let kill = s.find("Stop-Process").expect("kills leftovers");
        let copy = s.find("Copy-Item").expect("copies");
        let start = s.find("Start-Service").expect("starts");
        assert!(stop < kill && kill < copy && copy < start, "{s}");
        assert!(s.contains("novalink-service,novalink-agent"), "{s}");
        assert!(s.contains("StartsWith($pfPrefix"), "{s}");
    }

    #[test]
    fn install_script_checks_the_copied_files_against_the_hashes() {
        let s = install_script("C:\\p", "1", SVC, AGENT);
        assert!(s.contains("@('novalink-service.exe', 'aa11')"), "{s}");
        assert!(s.contains("@('novalink-agent.exe', 'bb22')"), "{s}");
        let copy = s.find("Copy-Item").unwrap();
        let hash = s
            .find("Get-FileHash -LiteralPath $to -Algorithm SHA256")
            .unwrap();
        let throw = s.find("is not the file inside NovaLink-Setup").unwrap();
        assert!(copy < hash && hash < throw, "{s}");
        // The check is on the installed copy, and a bad one is removed.
        assert!(s.contains("Remove-Item -LiteralPath $to -Force"), "{s}");
    }

    #[test]
    fn install_script_writes_version_last() {
        let s = install_script("C:\\p", "0.2.0", SVC, AGENT);
        // An earlier line removes a stale version.txt; this is the write.
        let v = s
            .find("Set-Content -LiteralPath (Join-Path $pf 'version.txt')")
            .unwrap();
        assert!(v > s.find("Start-Service").unwrap(), "{s}");
        assert!(s.contains("-Value '0.2.0'"), "{s}");
    }

    #[test]
    fn uninstall_script_keeps_program_data() {
        let s = uninstall_script();
        assert!(s.contains("sc.exe delete NovaLinkService"), "{s}");
        assert!(
            s.contains("Remove-Item -LiteralPath $pf -Recurse -Force"),
            "{s}"
        );
        assert!(
            s.contains("WaitForStatus('Stopped'") && s.contains("Stop-Process"),
            "{s}"
        );
        assert!(!s.contains("ProgramData"), "{s}");
        assert!(
            s.find("sc.exe delete").unwrap() < s.find("Remove-Item -LiteralPath $pf").unwrap(),
            "{s}"
        );
        // Everything is cleared before anything is deleted.
        assert!(
            s.find("Stop-Process").unwrap() < s.find("sc.exe delete").unwrap(),
            "{s}"
        );
    }

    #[test]
    fn up_to_date_needs_the_same_version_and_a_service() {
        assert!(up_to_date(Some("0.2.0\r\n"), "0.2.0", true));
        assert!(!up_to_date(Some("0.1.0"), "0.2.0", true));
        assert!(!up_to_date(Some("0.2.0"), "0.2.0", false));
        assert!(!up_to_date(None, "0.2.0", true));
    }

    #[test]
    fn sha256_hex_is_lowercase_hex() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }
}
