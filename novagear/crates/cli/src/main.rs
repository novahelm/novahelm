//! `novagear`: talks to the local agent through the shared std-only client, so the CLI
//! starts instantly and pulls in nothing heavy.

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use novagear_core::client::{self, call, open_url, panel_url};
use serde_json::{Value, json};

#[derive(Parser)]
#[command(
    name = "novagear",
    about = "Control your lights, Stream Deck, prompter and mice through the NovaGear agent"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// List connected devices.
    Devices {
        /// Print raw JSON.
        #[arg(long)]
        json: bool,
    },
    /// Open the control panel in your browser.
    Open,
    /// Show the agent's log, newest first.
    Logs {
        /// Only this level and worse: trace, debug, info, warn or error.
        #[arg(long)]
        level: Option<String>,
        /// Only lines containing this text.
        #[arg(short, long)]
        q: Option<String>,
        /// How many lines (at most 2000).
        #[arg(short = 'n', long, default_value_t = 100)]
        n: usize,
    },
    /// Lights: on, off, toggle, or set brightness / temperature.
    Light {
        #[command(subcommand)]
        op: LightOp,
        /// Only this light (an id from `novagear devices`); default all.
        #[arg(long, global = true)]
        id: Option<String>,
    },
    /// Prompter: play/pause, restart, faster, slower.
    Prompter { op: String },
    /// Set the Razer mouse DPI.
    Dpi { dpi: u16 },
    /// Run any action as JSON, e.g. '{"do":"light_toggle"}'.
    Action { json: String },
    /// Replace the deck's pages with your Elgato Stream Deck profiles (keeps config.json.bak).
    ImportStreamdeck,
    /// Show a deck page by name.
    Page { name: String },
    /// Programs allowed to push values to the deck and receive named events.
    Client {
        #[command(subcommand)]
        op: ClientOp,
    },
    /// Send a test approval to the deck and wait for the answer (up to 10 minutes). Hold
    /// Approve on the deck for a second to say yes; there is no other way.
    ApproveTest {
        #[arg(long, default_value = "Deploy web → production?")]
        title: String,
        #[arg(long, value_enum, default_value_t = RiskArg::Low)]
        risk: RiskArg,
    },
    /// Push a value to a deck panel, or remove it.
    Feed {
        #[command(subcommand)]
        op: FeedOp,
    },
}

#[derive(Subcommand)]
enum ClientOp {
    /// Add a client and print its token (shown once).
    Add {
        name: String,
    },
    List,
    Remove {
        name: String,
    },
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum RiskArg {
    Low,
    High,
}

#[derive(Clone, Copy, clap::ValueEnum)]
enum FeedStateArg {
    Ok,
    Warn,
    Bad,
}

#[derive(Subcommand)]
enum FeedOp {
    /// Set a feed: `novagear feed set build.status passing --state ok`.
    Set {
        key: String,
        /// A number, or text.
        value: String,
        #[arg(long)]
        state: Option<FeedStateArg>,
        /// Seconds before the value is shown as stale.
        #[arg(long, default_value_t = 120)]
        ttl: u64,
    },
    Clear {
        key: String,
    },
}

/// A finite number stays a number so panels can draw it as a gauge; anything else is text.
fn feed_value(s: &str) -> Value {
    match s.parse::<f64>() {
        Ok(n) if n.is_finite() => json!(n),
        _ => json!(s),
    }
}

#[derive(Subcommand)]
enum LightOp {
    On,
    Off,
    Toggle,
    Set {
        #[arg(long)]
        brightness: Option<u8>,
        #[arg(long)]
        kelvin: Option<u16>,
    },
}

fn action(a: Value) -> Result<()> {
    client::action(&a)
}

/// "fw 1.0.3 (200)", with the newer one when there is one; "dev 0.00" for a USB device
/// version, which is not firmware; "fw -" when unknown.
fn firmware_text(d: &Value) -> String {
    let fw = &d["firmware"];
    let installed = fw["installed"].as_str().unwrap_or("-");
    let label = if fw["installed_is_device_version"].as_bool() == Some(true) {
        "dev"
    } else {
        "fw"
    };
    match (fw["update_available"].as_bool(), fw["latest"].as_str()) {
        (Some(true), Some(latest)) => {
            format!("{label} {installed} -> {latest} (update available)")
        }
        (Some(true), None) => format!("{label} {installed} (update available)"),
        _ => format!("{label} {installed}"),
    }
}

/// Drops control characters (USB product strings can reach log messages) but keeps tabs.
fn strip_control(s: &str) -> String {
    s.chars()
        .filter(|c| *c == '\t' || !c.is_control())
        .collect()
}

/// One log line: time, level, message, then the other fields as `key=value`.
fn format_log_line(l: &Value) -> String {
    let fields = l["fields"].as_object();
    let mut out = format!(
        "{} {:<5} {}",
        l["timestamp"].as_str().unwrap_or(""),
        l["level"].as_str().unwrap_or(""),
        fields
            .and_then(|f| f.get("message"))
            .and_then(Value::as_str)
            .unwrap_or("")
    );
    for (k, v) in fields
        .into_iter()
        .flatten()
        .filter(|(k, _)| *k != "message")
    {
        let v = v.as_str().map_or_else(|| v.to_string(), str::to_string);
        out.push_str(&format!(" {k}={v}"));
    }
    strip_control(&out)
}

fn print_log_line(l: &Value) {
    println!("{}", format_log_line(l));
}

/// Percent-encodes everything but unreserved characters, for a query value.
fn url_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn print_devices(devs: &Value) {
    let Some(list) = devs.as_array() else { return };
    for d in list {
        let status = &d["status"];
        let state = match status["state"].as_str() {
            Some("busy") => format!("busy: {}", status["reason"].as_str().unwrap_or("")),
            Some(s) => s.to_string(),
            None => String::new(),
        };
        let extra = match d["kind"].as_str() {
            Some("light") if d["state"]["on"].is_boolean() => format!(
                "{} {}% {}K",
                if d["state"]["on"] == json!(true) {
                    "on"
                } else {
                    "off"
                },
                d["state"]["brightness"],
                d["state"]["kelvin"]
            ),
            Some("mouse") if d["state"]["dpi"].is_number() => format!("{} dpi", d["state"]["dpi"]),
            _ => String::new(),
        };
        println!(
            "{:<12} {:<28} {:<10} {:<14} {}  [{}]",
            d["kind"].as_str().unwrap_or(""),
            strip_control(d["name"].as_str().unwrap_or("")),
            state,
            extra,
            firmware_text(d),
            d["id"].as_str().unwrap_or("")
        );
    }
}

/// The name of the temporary client `approve-test` uses.
const TEST_CLIENT: &str = "approve-test";

/// Sends one approval as a temporary client called `approve-test` (made for the run, removed
/// after it, along with anything it left waiting) and prints how it ended.
fn approve_test(title: &str, risk: &str) -> Result<()> {
    // A leftover client from an interrupted run has a token nobody holds: replace it.
    let clients = call("GET", "/api/clients", None)?;
    if clients
        .as_array()
        .into_iter()
        .flatten()
        .any(|c| c["name"] == TEST_CLIENT)
    {
        call("DELETE", &format!("/api/clients/{TEST_CLIENT}"), None)?;
    }
    let token = client::client_add(TEST_CLIENT)?;
    let result = ask_and_wait(&token, title, risk);
    // Removing the client also cancels the request if it is still waiting.
    let _ = call("DELETE", &format!("/api/clients/{TEST_CLIENT}"), None);
    println!("{}", result?);
    Ok(())
}

fn ask_and_wait(token: &str, title: &str, risk: &str) -> Result<String> {
    use std::time::{Duration, Instant};
    let made = client::call_as(
        token,
        "POST",
        "/api/approvals",
        Some(&json!({
            "title": title,
            "lines": ["Sent by novagear approve-test"],
            "source": "approve-test",
            "risk": risk,
            "expires_in_s": 600,
        })),
        Duration::from_secs(10),
    )?;
    let id = made["id"].as_str().context("agent gave no id")?;
    println!("Waiting on the deck: hold Approve for a second to approve, tap Deny to refuse.");
    let until = Instant::now() + Duration::from_secs(600);
    while Instant::now() < until {
        let r = client::call_as(
            token,
            "GET",
            &format!("/api/approvals/{id}?wait=30"),
            None,
            Duration::from_secs(45),
        )?;
        match r["status"].as_str() {
            Some("waiting") | None => {}
            Some(done) => return Ok(done.to_string()),
        }
    }
    Ok("expired".to_string())
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Devices { json } => {
            let d = call("GET", "/api/devices", None)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&d)?);
            } else {
                print_devices(&d);
            }
        }
        Cmd::Logs { level, q, n } => {
            let mut path = format!("/api/logs?limit={n}");
            if let Some(l) = level {
                path += &format!("&level={}", url_encode(&l));
            }
            if let Some(q) = q {
                path += &format!("&q={}", url_encode(&q));
            }
            let lines = call("GET", &path, None)?;
            for l in lines.as_array().into_iter().flatten() {
                print_log_line(l);
            }
        }
        Cmd::Open => open_url(&panel_url("")?)?,
        Cmd::Light { op, id } => action(match op {
            LightOp::On => json!({ "do": "light_set", "id": id, "on": true }),
            LightOp::Off => json!({ "do": "light_set", "id": id, "on": false }),
            LightOp::Toggle => json!({ "do": "light_toggle", "id": id }),
            LightOp::Set { brightness, kelvin } => {
                json!({ "do": "light_set", "id": id, "brightness": brightness, "kelvin": kelvin })
            }
        })?,
        Cmd::Prompter { op } => action(match op.as_str() {
            "play" | "pause" | "toggle" => json!({ "do": "prompter_play_pause" }),
            "restart" => json!({ "do": "prompter_restart" }),
            "faster" => json!({ "do": "prompter_speed_step", "step": 10 }),
            "slower" => json!({ "do": "prompter_speed_step", "step": -10 }),
            other => bail!("unknown prompter command {other:?} (play, restart, faster, slower)"),
        })?,
        Cmd::Dpi { dpi } => action(json!({ "do": "mouse_dpi", "dpi": dpi }))?,
        Cmd::ImportStreamdeck => {
            let r = call("POST", "/api/import/streamdeck", None)?;
            println!(
                "Imported {} pages, {} icons; the deck starts on {:?}.",
                r["pages"],
                r["icons"],
                r["start_page"].as_str().unwrap_or("")
            );
            if let Some(list) = r["placeholders"].as_array().filter(|l| !l.is_empty()) {
                println!("Kept as placeholders (NovaGear can't run these yet):");
                for p in list {
                    println!("  {} ×{}", p[0].as_str().unwrap_or(""), p[1]);
                }
            }
            if let Some(list) = r["skipped_profiles"].as_array().filter(|l| !l.is_empty()) {
                println!(
                    "Skipped (not a Stream Deck +): {}",
                    list.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                );
            }
        }
        Cmd::Client { op } => match op {
            ClientOp::Add { name } => {
                let token = client::client_add(&name)?;
                println!("Client {name} added. Its token:\n\n{token}\n");
                println!(
                    "This token will not be shown again. Keep it in the client's own secret store."
                );
                println!("It can push feeds named {name}.* and receive events sent to {name}.");
            }
            ClientOp::List => {
                let list = call("GET", "/api/clients", None)?;
                for c in list.as_array().into_iter().flatten() {
                    println!("{}", strip_control(c["name"].as_str().unwrap_or("")));
                }
            }
            ClientOp::Remove { name } => {
                call(
                    "DELETE",
                    &format!("/api/clients/{}", url_encode(&name)),
                    None,
                )?;
                println!("Removed {name}; its token no longer works.");
            }
        },
        Cmd::Feed { op } => match op {
            FeedOp::Set {
                key,
                value,
                state,
                ttl,
            } => client::feed_set(
                &key,
                feed_value(&value),
                state.map(|s| match s {
                    FeedStateArg::Ok => "ok",
                    FeedStateArg::Warn => "warn",
                    FeedStateArg::Bad => "bad",
                }),
                Some(ttl),
            )?,
            FeedOp::Clear { key } => client::feed_clear(&key)?,
        },
        Cmd::ApproveTest { title, risk } => approve_test(
            &title,
            match risk {
                RiskArg::Low => "low",
                RiskArg::High => "high",
            },
        )?,
        Cmd::Page { name } => action(json!({ "do": "page_go", "page": name }))?,
        Cmd::Action { json } => {
            action(serde_json::from_str(&json).context("action is not valid JSON")?)?
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn firmware_column_says_when_an_update_exists() {
        let d = json!({"firmware": {"installed": "1.0.3 (200)", "latest": "1.0.3 (222)", "update_available": true}});
        assert_eq!(
            firmware_text(&d),
            "fw 1.0.3 (200) -> 1.0.3 (222) (update available)"
        );
        assert_eq!(firmware_text(&json!({})), "fw -");
    }

    #[test]
    fn device_versions_are_labelled_dev() {
        let d = json!({"firmware": {"installed": "0.00", "installed_is_device_version": true}});
        assert_eq!(firmware_text(&d), "dev 0.00");
    }

    #[test]
    fn log_lines_append_fields_and_drop_control_characters() {
        let l = json!({
            "timestamp": "t", "level": "WARN",
            "fields": {"message": "bad \u{1b}[31mname\u{7}\tok", "action": "type_text, 12 chars"}
        });
        let out = format_log_line(&l);
        assert!(!out.contains('\u{1b}') && !out.contains('\u{7}'));
        assert!(out.contains("bad [31mname\tok"));
        assert!(out.ends_with("action=type_text, 12 chars"));
    }

    #[test]
    fn feed_values_are_numbers_only_when_finite() {
        assert_eq!(feed_value("42"), json!(42.0));
        assert_eq!(feed_value("-1.5"), json!(-1.5));
        assert_eq!(feed_value("passing"), json!("passing"));
        assert_eq!(feed_value("NaN"), json!("NaN"));
        assert_eq!(feed_value("inf"), json!("inf"));
        assert_eq!(feed_value(""), json!(""));
    }

    #[test]
    fn query_values_are_encoded() {
        assert_eq!(url_encode("a b&c"), "a%20b%26c");
    }
}
