//! Executes the install plan with Win32 calls.

use crate::firewall;
use crate::plan::{Installed, Mode, Step, payload_problem, plan, still_running_text};
use novalink_agent::winnet::{self, Firewall};
use novalink_client::{Client, ClientError};
use std::net::{SocketAddr, TcpStream};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::{ERROR_CANCELLED, ERROR_SUCCESS, HANDLE, WIN32_ERROR};
use windows::Win32::Storage::FileSystem::{
    MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoUninitialize, IPersistFile,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, KEY_SET_VALUE, REG_DWORD, REG_OPTION_NON_VOLATILE, REG_SZ,
    RegCloseKey, RegCreateKeyExW, RegDeleteKeyW, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW,
    RegSetValueExW,
};
use windows::Win32::System::Threading::{
    EVENT_MODIFY_STATE, GetExitCodeProcess, OpenEventW, OpenProcess, PROCESS_ACCESS_RIGHTS,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
    QueryFullProcessImageNameW, SetEvent, TerminateProcess, WaitForSingleObject,
};
use windows::Win32::UI::Shell::{
    IShellLinkW, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW, ShellLink,
};
use windows::Win32::UI::WindowsAndMessaging::{
    IDYES, MB_ICONERROR, MB_ICONINFORMATION, MB_ICONQUESTION, MB_OK, MB_SETFOREGROUND, MB_TOPMOST,
    MB_YESNO, MESSAGEBOX_STYLE, MessageBoxW, SW_HIDE,
};
use windows::core::{Interface, Owned, PCWSTR, PWSTR};

mod payload {
    include!(concat!(env!("OUT_DIR"), "/payload.rs"));
}

const VERSION: &str = env!("CARGO_PKG_VERSION");
const API_ADDR: &str = "127.0.0.1:7979";
const TRAY_EXE: &str = "novalink-tray.exe";
const AGENT_EXE: &str = "novalink-agent.exe";
const SETUP_EXE: &str = "NovaLink-Setup.exe";
const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_VALUE: &str = "NovaLink";
const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\NovaLink";
const TRAY_QUIT_EVENT: &str = r"Local\NovaLinkTrayQuit";
const DETACHED_PROCESS: u32 = 0x0000_0008;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
/// The SYNCHRONIZE access right, needed to wait on a process handle.
const SYNCHRONIZE: PROCESS_ACCESS_RIGHTS = PROCESS_ACCESS_RIGHTS(0x0010_0000);

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn message_box(text: &str, style: MESSAGEBOX_STYLE) -> bool {
    let (t, c) = (wide("NovaLink Setup"), wide(text));
    // SAFETY: both buffers are NUL-terminated and outlive the call.
    let r = unsafe {
        MessageBoxW(
            None,
            PCWSTR(c.as_ptr()),
            PCWSTR(t.as_ptr()),
            style | MB_TOPMOST | MB_SETFOREGROUND,
        )
    };
    r == IDYES
}

fn info(text: &str) {
    message_box(text, MB_OK | MB_ICONINFORMATION);
}

fn failure(text: &str) {
    message_box(text, MB_OK | MB_ICONERROR);
}

fn ask(text: &str) -> bool {
    message_box(text, MB_YESNO | MB_ICONQUESTION)
}

fn env_path(name: &str) -> Result<PathBuf, String> {
    std::env::var_os(name)
        .map(PathBuf::from)
        .ok_or_else(|| format!("Windows didn't tell setup where your {name} folder is"))
}

fn install_dir() -> Result<PathBuf, String> {
    Ok(env_path("LOCALAPPDATA")?.join("Programs").join("NovaLink"))
}

fn shortcut_path() -> Result<PathBuf, String> {
    Ok(env_path("APPDATA")?
        .join(r"Microsoft\Windows\Start Menu\Programs")
        .join("NovaLink.lnk"))
}

fn port_open() -> bool {
    let addr: SocketAddr = API_ADDR.parse().expect("literal address");
    TcpStream::connect_timeout(&addr, Duration::from_millis(300)).is_ok()
}

fn wait_until(limit: Duration, mut done: impl FnMut() -> bool) -> bool {
    let end = Instant::now() + limit;
    loop {
        if done() {
            return true;
        }
        if Instant::now() >= end {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn detect(dir: &Path) -> Installed {
    Installed {
        dir_exists: dir.is_dir(),
        version: std::fs::read_to_string(dir.join("version.txt"))
            .ok()
            .map(|v| v.trim().to_string()),
        agent_running: port_open(),
        run_key_set: run_key_set(),
    }
}

fn run_key_set() -> bool {
    let sub = wide(RUN_KEY);
    let name = wide(RUN_VALUE);
    let mut key = HKEY::default();
    // SAFETY: both strings are NUL-terminated; the key is closed below.
    unsafe {
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(sub.as_ptr()),
            None,
            KEY_READ,
            &mut key,
        ) != ERROR_SUCCESS
        {
            return false;
        }
        let r = RegQueryValueExW(key, PCWSTR(name.as_ptr()), None, None, None, None);
        let _ = RegCloseKey(key);
        r == ERROR_SUCCESS
    }
}

pub fn run() {
    let uninstall = std::env::args().skip(1).any(|a| a == "--uninstall");
    let mode = if uninstall {
        Mode::Uninstall
    } else {
        Mode::Install
    };
    if uninstall
        && !ask(
            "Remove NovaLink? Your pairings are kept unless you also delete %APPDATA%\\NovaLink.",
        )
    {
        return;
    }
    if !uninstall
        && let Some(why) = payload_problem(&[
            ("novalink-agent.exe", payload::AGENT.len()),
            ("novalink.exe", payload::CLI.len()),
            (TRAY_EXE, payload::TRAY.len()),
        ])
    {
        failure(why);
        return;
    }
    match execute(mode) {
        Ok(()) if uninstall => info("NovaLink is removed."),
        Ok(()) => info(&installed_text()),
        Err(why) => failure(&why),
    }
}

fn execute(mode: Mode) -> Result<(), String> {
    let dir = install_dir()?;
    let mut files_in_place = false;
    let current = detect(&dir);
    for step in plan(&current, VERSION, mode) {
        run_step(step, &dir, &current).map_err(|e| {
            if files_in_place {
                format!("{e} The new NovaLink files are installed; run setup again to finish.")
            } else {
                e
            }
        })?;
        files_in_place |= step == Step::WriteFiles;
    }
    Ok(())
}

fn run_step(step: Step, dir: &Path, current: &Installed) -> Result<(), String> {
    match step {
        Step::StopTray => stop_tray(dir),
        Step::StopAgent => stop_agent(current),
        Step::WriteFiles => write_files(dir),
        Step::WriteVersion => write_version(dir),
        Step::SetRunKey => set_run_key(dir),
        Step::CreateStartMenuShortcut => create_shortcut(dir),
        Step::WriteUninstallEntry => write_uninstall_entry(dir),
        Step::AllowFirewall => {
            allow_firewall(dir);
            Ok(())
        }
        Step::LaunchTray => launch_tray(dir),
        Step::ClearOffMarker => clear_off_marker(),
        Step::RemoveRunKey => remove_run_key(),
        Step::RemoveShortcut => remove_shortcut(),
        Step::RemoveUninstallEntry => remove_uninstall_entry(),
        Step::RemoveFiles => remove_files(dir),
        // Pairings and keys live in %APPDATA%\NovaLink, which setup never touches.
        Step::KeepUserData => Ok(()),
    }
}

// --- stopping running copies -------------------------------------------------------------

/// Removes the Turn Off marker (%APPDATA%\NovaLink\off, the file the tray reads).
fn clear_off_marker() -> Result<(), String> {
    match std::fs::remove_file(novalink_agent::paths::config_dir().join("off")) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(fail_text("clear an earlier Turn Off", e)),
    }
}

fn stop_agent(current: &Installed) -> Result<(), String> {
    match Client::local().map(|c| c.shutdown()) {
        Ok(Ok(())) | Ok(Err(ClientError::NotRunning)) => {}
        Ok(Err(ClientError::Impostor)) => {
            return Err(
                "Something else is using NovaLink's port 7979, so setup can't tell \
                        whether NovaLink is running. Close the program using that port, then \
                        run setup again."
                    .into(),
            );
        }
        // The agent may be too old or too stuck to answer; the port decides below.
        Ok(Err(_)) | Err(_) => {}
    }
    if wait_until(Duration::from_secs(5), || !port_open()) {
        Ok(())
    } else {
        // An agent from before the tray answers 404 to /v1/shutdown and keeps running.
        Err(still_running_text(current).into())
    }
}

/// Handles to every novalink-tray.exe running from `dir`.
fn tray_processes(dir: &Path) -> Vec<Owned<HANDLE>> {
    let want = dir.join(TRAY_EXE).to_string_lossy().to_lowercase();
    let mut found = Vec::new();
    // SAFETY: the snapshot and process handles are owned and closed on drop; the entry
    // struct is sized as the API requires and every buffer outlives the call that fills it.
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return found;
        };
        let snap = Owned::new(snap);
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut more = Process32FirstW(*snap, &mut entry).is_ok();
        while more {
            let len = entry
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..len]);
            if name.eq_ignore_ascii_case(TRAY_EXE)
                && let Ok(h) = OpenProcess(
                    PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_TERMINATE | SYNCHRONIZE,
                    false,
                    entry.th32ProcessID,
                )
            {
                let h = Owned::new(h);
                let mut buf = [0u16; 1024];
                let mut n = buf.len() as u32;
                let ok = QueryFullProcessImageNameW(
                    *h,
                    PROCESS_NAME_WIN32,
                    PWSTR(buf.as_mut_ptr()),
                    &mut n,
                )
                .is_ok();
                if ok && String::from_utf16_lossy(&buf[..n as usize]).to_lowercase() == want {
                    found.push(h);
                }
            }
            more = Process32NextW(*snap, &mut entry).is_ok();
        }
    }
    found
}

fn stop_tray(dir: &Path) -> Result<(), String> {
    let name = wide(TRAY_QUIT_EVENT);
    // SAFETY: `name` is NUL-terminated; the opened handle is owned and closed on drop.
    unsafe {
        if let Ok(event) = OpenEventW(EVENT_MODIFY_STATE, false, PCWSTR(name.as_ptr())) {
            let event = Owned::new(event);
            let _ = SetEvent(*event);
        }
    }
    if wait_until(Duration::from_secs(3), || tray_processes(dir).is_empty()) {
        return Ok(());
    }
    // The tray can be busy building its icon for up to a minute. It holds no input state,
    // so ending it is safe.
    for h in tray_processes(dir) {
        // SAFETY: the handle was opened with PROCESS_TERMINATE and SYNCHRONIZE.
        unsafe {
            let _ = TerminateProcess(*h, 0);
            let _ = WaitForSingleObject(*h, 2000);
        }
    }
    if tray_processes(dir).is_empty() {
        Ok(())
    } else {
        Err(
            "The NovaLink tray is still running and Windows wouldn't close it. Quit it from \
             the tray icon, then run setup again."
                .into(),
        )
    }
}

// --- files -------------------------------------------------------------------------------

fn fail_text(what: &str, why: impl std::fmt::Display) -> String {
    format!("Setup couldn't {what}: {why}.")
}

fn move_over(from: &Path, to: &Path) -> windows::core::Result<()> {
    let (f, t) = (wide(&from.to_string_lossy()), wide(&to.to_string_lossy()));
    // SAFETY: both paths are NUL-terminated and outlive the call.
    unsafe {
        MoveFileExW(
            PCWSTR(f.as_ptr()),
            PCWSTR(t.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
}

fn with_suffix(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(suffix);
    PathBuf::from(s)
}

fn write_version(dir: &Path) -> Result<(), String> {
    let (target, staged) = (dir.join("version.txt"), dir.join("version.txt.new"));
    std::fs::write(&staged, VERSION).map_err(|e| fail_text("write version.txt", e))?;
    move_over(&staged, &target).map_err(|e| {
        let _ = std::fs::remove_file(&staged);
        fail_text("write version.txt", e.message().trim().to_string())
    })
}

/// Puts the new files in place all together or not at all. Each file is written as `.new`;
/// then each existing target is renamed to `.old` (a running exe can be renamed) and its
/// `.new` moved in. If anything fails, the files already swapped are put back from `.old`.
fn write_files(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| fail_text(&format!("create {}", dir.display()), e))?;
    // Leftovers from an earlier run whose cleanup was blocked by a running exe.
    if let Ok(entries) = std::fs::read_dir(dir) {
        for e in entries.flatten() {
            let p = e.path();
            if matches!(p.extension().and_then(|x| x.to_str()), Some("old" | "new")) {
                let _ = std::fs::remove_file(p);
            }
        }
    }
    let me = std::env::current_exe().map_err(|e| fail_text("find itself", e))?;
    let own_copy = dir.join(SETUP_EXE);
    let untouched = "Nothing was changed.";

    let payload: [(&str, &[u8]); 3] = [
        ("novalink-agent.exe", payload::AGENT),
        ("novalink.exe", payload::CLI),
        (TRAY_EXE, payload::TRAY),
    ];
    let mut targets: Vec<PathBuf> = Vec::new();
    let discard = |targets: &[PathBuf]| {
        for t in targets {
            let _ = std::fs::remove_file(with_suffix(t, ".new"));
        }
    };
    for (name, bytes) in payload {
        let t = dir.join(name);
        targets.push(t.clone());
        if let Err(e) = std::fs::write(with_suffix(&t, ".new"), bytes) {
            discard(&targets);
            return Err(format!(
                "{} {untouched}",
                fail_text(&format!("write {name}"), e)
            ));
        }
    }
    // Running the installed copy again can't replace itself; it is already in place.
    if me != own_copy {
        targets.push(own_copy.clone());
        if let Err(e) = std::fs::copy(&me, with_suffix(&own_copy, ".new")) {
            discard(&targets);
            return Err(format!(
                "{} {untouched}",
                fail_text(&format!("copy {SETUP_EXE}"), e)
            ));
        }
    }

    // (target, whether an old file was set aside)
    let mut swapped: Vec<(&PathBuf, bool)> = Vec::new();
    for t in &targets {
        let old = with_suffix(t, ".old");
        let had_old = t.exists();
        let step = (|| {
            if had_old {
                move_over(t, &old)?;
            }
            swapped.push((t, had_old));
            move_over(&with_suffix(t, ".new"), t)
        })();
        if let Err(e) = step {
            let why = e.message().trim().to_string();
            let name = t
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            // The file whose rename succeeded but whose move failed is in `swapped` too.
            let mut restored = true;
            for (t, had_old) in swapped.iter().rev() {
                let ok = if *had_old {
                    move_over(&with_suffix(t, ".old"), t).is_ok()
                } else {
                    std::fs::remove_file(t).is_ok()
                };
                restored &= ok;
            }
            discard(&targets);
            let state = if restored {
                untouched
            } else {
                "Some files could not be put back, so NovaLink may not start. Run setup again to repair it."
            };
            return Err(format!(
                "{} Close anything using NovaLink, then run setup again. {state}",
                fail_text(&format!("replace {name}"), why)
            ));
        }
    }
    for t in &targets {
        // Can fail while an old exe is still running; the next run clears it.
        let _ = std::fs::remove_file(with_suffix(t, ".old"));
    }
    Ok(())
}

fn remove_files(dir: &Path) -> Result<(), String> {
    // A running exe can be renamed but not deleted: move this copy out of the way first.
    if let Ok(me) = std::env::current_exe()
        && me.starts_with(dir)
    {
        let parked =
            std::env::temp_dir().join(format!("NovaLink-Setup-removed-{}.exe", std::process::id()));
        let _ = move_over(&me, &parked);
    }
    match std::fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!(
            "NovaLink is removed from Windows, but setup couldn't delete its folder {}: {e}. \
             Close anything using it (for example a File Explorer window or a terminal running novalink), then delete that folder yourself.",
            dir.display()
        )),
    }
}

// --- registry ----------------------------------------------------------------------------

struct Key(HKEY);

impl Key {
    fn create(path: &str) -> Result<Key, String> {
        let sub = wide(path);
        let mut key = HKEY::default();
        // SAFETY: `sub` is NUL-terminated; the key is closed on drop.
        let r = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(sub.as_ptr()),
                None,
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_SET_VALUE,
                None,
                &mut key,
                None,
            )
        };
        if r == ERROR_SUCCESS {
            Ok(Key(key))
        } else {
            Err(fail_text(
                "open the Windows registry",
                format!("error {}", r.0),
            ))
        }
    }

    fn set_str(&self, name: &str, value: &str) -> Result<(), String> {
        let n = wide(name);
        let bytes: Vec<u8> = wide(value).iter().flat_map(|u| u.to_le_bytes()).collect();
        // SAFETY: the name is NUL-terminated and the buffer outlives the call.
        let r = unsafe { RegSetValueExW(self.0, PCWSTR(n.as_ptr()), None, REG_SZ, Some(&bytes)) };
        check(name, r)
    }

    fn set_dword(&self, name: &str, value: u32) -> Result<(), String> {
        let n = wide(name);
        // SAFETY: the name is NUL-terminated and the buffer outlives the call.
        let r = unsafe {
            RegSetValueExW(
                self.0,
                PCWSTR(n.as_ptr()),
                None,
                REG_DWORD,
                Some(&value.to_le_bytes()),
            )
        };
        check(name, r)
    }

    fn delete_value(&self, name: &str) {
        let n = wide(name);
        // SAFETY: the name is NUL-terminated. A missing value is already the state asked for.
        unsafe {
            let _ = RegDeleteValueW(self.0, PCWSTR(n.as_ptr()));
        }
    }
}

fn check(name: &str, r: WIN32_ERROR) -> Result<(), String> {
    if r == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(fail_text(
            &format!("save the {name} setting"),
            format!("error {}", r.0),
        ))
    }
}

impl Drop for Key {
    fn drop(&mut self) {
        // SAFETY: the key was opened in `create`.
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

fn set_run_key(dir: &Path) -> Result<(), String> {
    // Same shape the tray writes for "Start with Windows": the quoted path to its own exe.
    let value = format!("\"{}\"", dir.join(TRAY_EXE).display());
    Key::create(RUN_KEY)?.set_str(RUN_VALUE, &value)
}

fn remove_run_key() -> Result<(), String> {
    Key::create(RUN_KEY)?.delete_value(RUN_VALUE);
    Ok(())
}

fn write_uninstall_entry(dir: &Path) -> Result<(), String> {
    let key = Key::create(UNINSTALL_KEY)?;
    key.set_str("DisplayName", "NovaLink")?;
    key.set_str("DisplayVersion", VERSION)?;
    key.set_str("Publisher", "Nova")?;
    key.set_str("InstallLocation", &dir.to_string_lossy())?;
    key.set_str(
        "UninstallString",
        &format!("\"{}\" --uninstall", dir.join(SETUP_EXE).display()),
    )?;
    key.set_dword("NoModify", 1)?;
    key.set_dword("NoRepair", 1)
}

fn remove_uninstall_entry() -> Result<(), String> {
    let sub = wide(UNINSTALL_KEY);
    // SAFETY: `sub` is NUL-terminated. A missing key is already the state asked for.
    unsafe {
        let _ = RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR(sub.as_ptr()));
    }
    Ok(())
}

// --- Windows Firewall --------------------------------------------------------------------

/// The closing message, with advice when Windows treats this network as Public.
fn installed_text() -> String {
    let mut text = "NovaLink is installed. Look for its icon in the tray (you may need to click ^ \
                    to see it)."
        .to_string();
    let facts = install_dir().and_then(|d| winnet::probe(&d.join(AGENT_EXE)));
    if let Ok(f) = facts {
        let public = winnet::public_networks(&f);
        if !public.is_empty() {
            text.push_str("\n\n");
            text.push_str(&winnet::public_advice(&public));
        }
    }
    text
}

/// Asks to let the agent through Windows Firewall when nothing does yet, then adds the rule
/// with one administrator prompt. Declining, or a failure, leaves the install standing.
fn allow_firewall(dir: &Path) {
    let exe = dir.join(AGENT_EXE);
    let state = winnet::probe(&exe).map(|f| winnet::firewall(&f));
    if matches!(state, Ok(Firewall::Allowed | Firewall::Off)) || !ask(firewall::ASK) {
        return;
    }
    match run_elevated_powershell(&firewall::script(&exe.to_string_lossy())) {
        Ok(()) | Err(None) => {}
        Err(Some(why)) => failure(&format!(
            "Setup couldn't allow NovaLink through Windows Firewall: {why}. NovaLink may not \
             find your other computers; run setup again to try again."
        )),
    }
}

/// Runs `script` in an elevated, hidden PowerShell and waits for it. `Err(None)` means the
/// user said no to the administrator prompt.
fn run_elevated_powershell(script: &str) -> Result<(), Option<String>> {
    let verb = wide("runas");
    let file = wide("powershell.exe");
    let args = wide(&firewall::powershell_args(script));
    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(args.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    // SAFETY: every string is NUL-terminated and outlives the call; the process handle
    // ShellExecuteExW returns is owned here and closed on drop.
    unsafe {
        if let Err(e) = ShellExecuteExW(&mut info) {
            return Err(
                (e.code() != ERROR_CANCELLED.to_hresult()).then(|| e.message().trim().to_string())
            );
        }
        if info.hProcess.is_invalid() {
            return Ok(());
        }
        let process = Owned::new(info.hProcess);
        WaitForSingleObject(*process, 60_000);
        let mut code = 0;
        GetExitCodeProcess(*process, &mut code)
            .map_err(|e| Some(e.message().trim().to_string()))?;
        if code != 0 {
            return Err(Some(format!("PowerShell stopped with code {code}")));
        }
    }
    Ok(())
}

// --- shortcut and launch -----------------------------------------------------------------

fn create_shortcut(dir: &Path) -> Result<(), String> {
    let lnk = shortcut_path()?;
    if let Some(parent) = lnk.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| fail_text("create the Start menu folder", e))?;
    }
    let tray = wide(&dir.join(TRAY_EXE).to_string_lossy());
    let work = wide(&dir.to_string_lossy());
    let desc = wide("NovaLink");
    let target = wide(&lnk.to_string_lossy());
    // SAFETY: COM is initialised for this thread before use and released after the interface
    // pointers are dropped; every string is NUL-terminated and outlives its call.
    let result = unsafe {
        let init = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let result = (|| -> windows::core::Result<()> {
            let link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
            link.SetPath(PCWSTR(tray.as_ptr()))?;
            link.SetWorkingDirectory(PCWSTR(work.as_ptr()))?;
            link.SetDescription(PCWSTR(desc.as_ptr()))?;
            let file: IPersistFile = link.cast()?;
            file.Save(PCWSTR(target.as_ptr()), true)
        })();
        if init.is_ok() {
            CoUninitialize();
        }
        result
    };
    result.map_err(|e| {
        fail_text(
            "create the Start menu shortcut",
            e.message().trim().to_string(),
        )
    })
}

fn remove_shortcut() -> Result<(), String> {
    match std::fs::remove_file(shortcut_path()?) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(format!(
            "Setup couldn't remove the Start menu shortcut: {e}."
        )),
        _ => Ok(()),
    }
}

fn launch_tray(dir: &Path) -> Result<(), String> {
    std::process::Command::new(dir.join(TRAY_EXE))
        .current_dir(dir)
        .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP)
        .spawn()
        .map(drop)
        .map_err(|e| {
            format!(
                "NovaLink is installed, but its tray wouldn't start: {e}. Start NovaLink from the Start menu."
            )
        })
}
