//! Actions on the computer itself: opening apps and websites, typing, hotkeys and volume.
//! Each uses the platform's own tool (`open`/`osascript`, `xdg-open`/`wpctl`/`xdotool`,
//! `start`) or, for keystrokes on macOS, CGEvents. All of it blocks briefly, so the agent
//! calls these on its blocking pool.

use anyhow::{Context, Result, bail};
use std::process::Command;

fn run(cmd: &mut Command) -> Result<String> {
    let out = cmd
        .output()
        .with_context(|| format!("running {:?}", cmd.get_program()))?;
    if !out.status.success() {
        // Fixed text: the tool's stderr can echo its arguments (typed text, hotkeys, links).
        bail!("{:?} failed ({})", cmd.get_program(), out.status);
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Only web and mail links: an imported or edited key must not become a way to open
/// arbitrary URL schemes.
pub fn safe_url(url: &str) -> bool {
    let url = url.trim();
    let lower = url.to_ascii_lowercase();
    // Quotes, whitespace and control characters never belong in a link and are how
    // arguments get split or smuggled on any platform.
    let clean = !url
        .chars()
        .any(|c| c.is_whitespace() || c.is_control() || c == '"');
    clean
        && ["http://", "https://", "mailto:"]
            .iter()
            .any(|p| lower.starts_with(p))
}

pub fn open_url(url: &str) -> Result<()> {
    if !safe_url(url) {
        // Not echoed: a link can carry a secret in its query string.
        bail!("only http, https and mailto links can be opened");
    }
    open(url)
}

pub fn open_app(path: &str) -> Result<()> {
    let path = path.trim().trim_matches('"');
    if path.contains("://") {
        // Not echoed: a link can carry a secret in its query string.
        bail!("that is a link, not an app; use open_url");
    }
    // A leading dash would be read as an option by open, explorer or xdg tools.
    if path.is_empty() || path.starts_with('-') {
        bail!("that isn't an app path");
    }
    if cfg!(target_os = "macos") {
        run(Command::new("open").arg(path)).map(drop)
    } else if cfg!(windows) {
        // explorer.exe opens a path itself; nothing goes through cmd.exe, which would run
        // `&`, `|` and friends inside the path as commands.
        Command::new("explorer.exe")
            .arg(path)
            .spawn()
            .with_context(|| format!("opening {path}"))
            .map(drop)
    } else if path.ends_with(".desktop") {
        run(Command::new("gtk-launch").arg(path.trim_end_matches(".desktop"))).map(drop)
    } else {
        Command::new(path)
            .spawn()
            .with_context(|| format!("starting {path}"))
            .map(drop)
    }
}

fn open(target: &str) -> Result<()> {
    if cfg!(target_os = "macos") {
        run(Command::new("open").arg(target)).map(drop)
    } else if cfg!(windows) {
        // The shell's URL handler opens the link directly, never through cmd.exe.
        Command::new("rundll32")
            .args(["url.dll,FileProtocolHandler", target])
            .spawn()
            .context("opening the link")
            .map(drop)
    } else {
        run(Command::new("xdg-open").arg(target)).map(drop)
    }
}

// ---- volume ----

#[cfg(target_os = "macos")]
fn osascript(script: &str) -> Result<String> {
    run(Command::new("osascript").args(["-e", script]))
}

pub fn volume_step(step: i16, input: bool) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let which = if input { "input" } else { "output" };
        osascript(&format!(
            "set volume {which} volume (({which} volume of (get volume settings)) + {step})"
        ))
        .map(drop)
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let target = if input {
            "@DEFAULT_AUDIO_SOURCE@"
        } else {
            "@DEFAULT_AUDIO_SINK@"
        };
        let amount = format!(
            "{}%{}",
            step.unsigned_abs(),
            if step < 0 { "-" } else { "+" }
        );
        run(Command::new("wpctl").args(["set-volume", "-l", "1.0", target, &amount])).map(drop)
    }
    #[cfg(windows)]
    {
        let _ = (step, input);
        bail!("volume control isn't supported on Windows yet")
    }
}

/// macOS has no input mute, so the mic "mutes" to 0 and comes back to where it was.
#[cfg(target_os = "macos")]
static MIC_BEFORE_MUTE: std::sync::Mutex<Option<u8>> = std::sync::Mutex::new(None);

/// Toggles mute. Returns the new muted state when it is known without a second read.
pub fn mute_toggle(input: bool) -> Result<Option<bool>> {
    #[cfg(target_os = "macos")]
    {
        if !input {
            return osascript(
                "set volume output muted (not (output muted of (get volume settings)))",
            )
            .map(|_| None);
        }
        let now: u8 = osascript("input volume of (get volume settings)")?
            .parse()
            .unwrap_or(0);
        let mut before = MIC_BEFORE_MUTE.lock().unwrap_or_else(|e| e.into_inner());
        let next = if now > 0 {
            *before = Some(now);
            0
        } else {
            before.take().unwrap_or(70)
        };
        osascript(&format!("set volume input volume {next}"))?;
        Ok(Some(next == 0))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let target = if input {
            "@DEFAULT_AUDIO_SOURCE@"
        } else {
            "@DEFAULT_AUDIO_SINK@"
        };
        run(Command::new("wpctl").args(["set-mute", target, "toggle"])).map(|_| None)
    }
    #[cfg(windows)]
    {
        let _ = input;
        bail!("mute isn't supported on Windows yet")
    }
}

// ---- keys ----

/// A parsed hotkey: modifiers plus one key, as a name or a raw platform code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hotkey {
    pub cmd: bool,
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub key: Key,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Key {
    Named(String),
    Code(u16),
}

pub fn parse_hotkey(s: &str) -> Result<Hotkey> {
    let mut h = Hotkey {
        cmd: false,
        ctrl: false,
        alt: false,
        shift: false,
        key: Key::Named(String::new()),
    };
    let parts: Vec<String> = s
        .split('+')
        .map(|p| p.trim().to_ascii_lowercase())
        .collect();
    let (last, mods) = parts.split_last().context("empty hotkey")?;
    for m in mods {
        match m.as_str() {
            "cmd" | "command" | "super" | "win" | "meta" => h.cmd = true,
            "ctrl" | "control" => h.ctrl = true,
            "alt" | "opt" | "option" => h.alt = true,
            "shift" => h.shift = true,
            _ => bail!("unknown modifier in a hotkey (use cmd, ctrl, alt or shift)"),
        }
    }
    h.key = match last.strip_prefix("code:") {
        Some(n) => Key::Code(n.parse().context("bad key code in a hotkey")?),
        None if !last.is_empty() => Key::Named(last.clone()),
        None => bail!("no key in a hotkey"),
    };
    Ok(h)
}

/// macOS virtual key codes (ANSI layout) for the keys hotkeys use.
pub fn mac_keycode(name: &str) -> Option<u16> {
    const LETTERS: &[(char, u16)] = &[
        ('a', 0),
        ('s', 1),
        ('d', 2),
        ('f', 3),
        ('h', 4),
        ('g', 5),
        ('z', 6),
        ('x', 7),
        ('c', 8),
        ('v', 9),
        ('b', 11),
        ('q', 12),
        ('w', 13),
        ('e', 14),
        ('r', 15),
        ('y', 16),
        ('t', 17),
        ('1', 18),
        ('2', 19),
        ('3', 20),
        ('4', 21),
        ('6', 22),
        ('5', 23),
        ('=', 24),
        ('9', 25),
        ('7', 26),
        ('-', 27),
        ('8', 28),
        ('0', 29),
        (']', 30),
        ('o', 31),
        ('u', 32),
        ('[', 33),
        ('i', 34),
        ('p', 35),
        ('l', 37),
        ('j', 38),
        ('\'', 39),
        ('k', 40),
        (';', 41),
        ('\\', 42),
        (',', 43),
        ('/', 44),
        ('n', 45),
        ('m', 46),
        ('.', 47),
        ('`', 50),
    ];
    let mut chars = name.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        return LETTERS.iter().find(|(l, _)| *l == c).map(|(_, k)| *k);
    }
    Some(match name {
        "return" | "enter" => 36,
        "tab" => 48,
        "space" => 49,
        "delete" | "backspace" => 51,
        "escape" | "esc" => 53,
        "forwarddelete" => 117,
        "home" => 115,
        "end" => 119,
        "pageup" => 116,
        "pagedown" => 121,
        "left" => 123,
        "right" => 124,
        "down" => 125,
        "up" => 126,
        "f1" => 122,
        "f2" => 120,
        "f3" => 99,
        "f4" => 118,
        "f5" => 96,
        "f6" => 97,
        "f7" => 98,
        "f8" => 100,
        "f9" => 101,
        "f10" => 109,
        "f11" => 103,
        "f12" => 111,
        _ => return None,
    })
}

/// The name of a macOS key code, for showing imported hotkeys; unknown codes stay `code:N`.
pub fn mac_key_name(code: u16) -> String {
    const NAMES: &[&str] = &[
        "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p", "q", "r",
        "s", "t", "u", "v", "w", "x", "y", "z", "0", "1", "2", "3", "4", "5", "6", "7", "8", "9",
        "return", "tab", "space", "delete", "escape", "left", "right", "down", "up", "home", "end",
        "pageup", "pagedown", "f1", "f2", "f3", "f4", "f5", "f6", "f7", "f8", "f9", "f10", "f11",
        "f12", "-", "=", "[", "]", ";", "'", ",", ".", "/", "\\", "`",
    ];
    NAMES
        .iter()
        .find(|n| mac_keycode(n) == Some(code))
        .map_or_else(|| format!("code:{code}"), |n| n.to_string())
}

#[cfg(target_os = "macos")]
mod mac {
    use super::{Hotkey, Key, mac_keycode};
    use anyhow::{Context, Result, bail};
    use core_foundation::base::TCFType;
    use core_foundation::boolean::CFBoolean;
    use core_foundation::dictionary::{CFDictionary, CFDictionaryRef};
    use core_foundation::string::{CFString, CFStringRef};
    use core_graphics::event::{CGEvent, CGEventFlags, CGEventTapLocation};
    use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
    use std::sync::atomic::{AtomicBool, Ordering};

    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXIsProcessTrustedWithOptions(options: CFDictionaryRef) -> bool;
        static kAXTrustedCheckOptionPrompt: CFStringRef;
    }

    /// Asks macOS whether keys may be pressed. The first refusal in a run also asks macOS to
    /// prompt, which is what puts NovaGear in the Accessibility list at all; a plain query
    /// leaves it out, and the list has nothing to switch on.
    fn check_trusted() -> Result<()> {
        static PROMPTED: AtomicBool = AtomicBool::new(false);
        let prompt = !PROMPTED.swap(true, Ordering::Relaxed);
        // SAFETY: the key is a constant from ApplicationServices, and the dictionary lives
        // until the call returns.
        let trusted = unsafe {
            let key = CFString::wrap_under_get_rule(kAXTrustedCheckOptionPrompt);
            let options = CFDictionary::from_CFType_pairs(&[(key, CFBoolean::from(prompt))]);
            AXIsProcessTrustedWithOptions(options.as_concrete_TypeRef())
        };
        if trusted {
            return Ok(());
        }
        bail!(
            "NovaGear needs Accessibility permission to press keys: System Settings › Privacy & Security › Accessibility, then switch on NovaGear"
        )
    }

    fn source() -> Result<CGEventSource> {
        CGEventSource::new(CGEventSourceStateID::HIDSystemState)
            .ok()
            .context("no keyboard event source")
    }

    pub fn press(h: &Hotkey) -> Result<()> {
        check_trusted()?;
        let code = match &h.key {
            Key::Code(c) => *c,
            Key::Named(n) => mac_keycode(n).context("no macOS key with that name")?,
        };
        let mut flags = CGEventFlags::empty();
        if h.cmd {
            flags |= CGEventFlags::CGEventFlagCommand;
        }
        if h.ctrl {
            flags |= CGEventFlags::CGEventFlagControl;
        }
        if h.alt {
            flags |= CGEventFlags::CGEventFlagAlternate;
        }
        if h.shift {
            flags |= CGEventFlags::CGEventFlagShift;
        }
        for down in [true, false] {
            let e = CGEvent::new_keyboard_event(source()?, code, down)
                .ok()
                .context("making a key event")?;
            e.set_flags(flags);
            e.post(CGEventTapLocation::HID);
        }
        Ok(())
    }

    pub fn type_text(text: &str) -> Result<()> {
        check_trusted()?;
        // Unicode strings go in small chunks; the event carries the text, not key codes, so
        // any layout and emoji work.
        let chars: Vec<char> = text.chars().collect();
        for chunk in chars.chunks(16) {
            let s: String = chunk.iter().collect();
            for down in [true, false] {
                let e = CGEvent::new_keyboard_event(source()?, 0, down)
                    .ok()
                    .context("making a key event")?;
                e.set_string(&s);
                e.post(CGEventTapLocation::HID);
            }
        }
        Ok(())
    }
}

#[cfg(all(unix, not(target_os = "macos")))]
fn xdotool_combo(h: &Hotkey) -> Result<String> {
    let key = match &h.key {
        Key::Named(n) => match n.as_str() {
            "enter" => "Return".to_string(),
            "esc" => "Escape".to_string(),
            "space" => "space".to_string(),
            other => other.to_string(),
        },
        Key::Code(_) => bail!("raw key codes from macOS don't carry over to Linux; edit this key"),
    };
    let mut parts = Vec::new();
    for (on, m) in [
        (h.ctrl, "ctrl"),
        (h.alt, "alt"),
        (h.shift, "shift"),
        (h.cmd, "super"),
    ] {
        if on {
            parts.push(m.to_string());
        }
    }
    parts.push(key);
    Ok(parts.join("+"))
}

pub fn hotkey(keys: &str) -> Result<()> {
    let h = parse_hotkey(keys)?;
    #[cfg(target_os = "macos")]
    {
        mac::press(&h)
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        run(Command::new("xdotool").args(["key", &xdotool_combo(&h)?]))
            .context("hotkeys on Linux need xdotool installed")
            .map(drop)
    }
    #[cfg(windows)]
    {
        let _ = h;
        bail!("hotkeys aren't supported on Windows yet")
    }
}

pub fn type_text(text: &str, enter: bool) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        mac::type_text(text)?;
        if enter {
            hotkey("return")?;
        }
        Ok(())
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        run(Command::new("xdotool").args(["type", "--", text]))
            .context("typing on Linux needs xdotool installed")?;
        if enter {
            hotkey("return")?;
        }
        Ok(())
    }
    #[cfg(windows)]
    {
        let _ = (text, enter);
        bail!("typing text isn't supported on Windows yet")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_hotkeys() {
        let h = parse_hotkey("cmd+shift+4").unwrap();
        assert!(h.cmd && h.shift && !h.ctrl && !h.alt);
        assert_eq!(h.key, Key::Named("4".into()));
        assert_eq!(parse_hotkey("ctrl+code:90").unwrap().key, Key::Code(90));
        assert!(parse_hotkey("hyper+a").is_err());
        assert!(parse_hotkey("cmd+").is_err());
    }

    #[test]
    fn mac_keycodes_match_elgatos_profiles() {
        // From real Stream Deck profiles: cmd+ctrl+49 (emoji), cmd+shift+21 / +23 (screenshots).
        assert_eq!(mac_keycode("space"), Some(49));
        assert_eq!(mac_keycode("4"), Some(21));
        assert_eq!(mac_keycode("5"), Some(23));
        assert_eq!(mac_key_name(21), "4");
        assert_eq!(mac_key_name(126), "up");
        assert_eq!(mac_key_name(999), "code:999");
        for n in ["a", "z", "return", "f12", "/", "up"] {
            assert_eq!(mac_key_name(mac_keycode(n).unwrap()), n, "{n} round-trips");
        }
    }

    #[test]
    fn only_web_and_mail_links_open() {
        assert!(safe_url("https://youtube.com"));
        assert!(safe_url("mailto:me@example.com"));
        assert!(!safe_url("file:///etc/passwd"));
        assert!(!safe_url("javascript:alert(1)"));
        assert!(open_url("smb://host/share").is_err());
        assert!(
            !safe_url("https://x.com/a\" & calc"),
            "quotes and spaces are refused"
        );
        assert!(!safe_url("https://x.com/\nnext"));
        assert!(open_app("https://x.com").is_err());
        assert!(open_app("-a Terminal").is_err(), "no option smuggling");
    }
}
