//! `novalink`: talks to the local agent API on 127.0.0.1:7979.

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use novalink_client::{Client, ClientError};
use reqwest::Method;
use serde_json::{Value, json};
use std::process::ExitCode;

const IMPOSTOR: &str =
    "Something else is listening on 127.0.0.1:7979 — refusing to send the NovaLink token.";
const NOT_RUNNING: &str = "NovaLink agent isn't running. Start it with: novalink-agent";

#[derive(Parser)]
#[command(name = "novalink", about = "Control the NovaLink agent")]
struct Cli {
    /// Print the raw JSON instead of a readable summary.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// This machine, its peers and the layout.
    Status,
    /// Paired devices.
    Devices,
    /// Devices seen on the network that are not paired.
    Discovered,
    /// Pair with a device: `pair <name|id>`, `pair pending`, `pair accept <id>`, `pair reject <id>`.
    Pair {
        #[arg(num_args = 1..=2, required = true)]
        args: Vec<String>,
    },
    /// Forget a paired device.
    Unpair { device: String },
    /// Say which device sits on an edge of this machine.
    Layout {
        #[arg(value_parser = ["left", "right", "top", "bottom"])]
        edge: String,
        peer: String,
    },
    /// Latency percentiles and counters.
    Metrics,
    /// Recent flight-recorder events.
    Events {
        #[arg(long, default_value_t = 200)]
        limit: usize,
    },
    /// Check permissions and connections, with advice.
    Doctor,
    /// Drive synthetic input at a peer and count stuck keys: `soak --hours 24 --peer <name|id>`,
    /// or `soak --status` for progress and the latest report. Run the peer with --dry-inject.
    Soak {
        #[arg(long, requires = "peer", conflicts_with_all = ["status", "stop"])]
        hours: Option<f64>,
        #[arg(long)]
        peer: Option<String>,
        /// Also drive a peer that is not running with --dry-inject (it will type!).
        #[arg(long)]
        allow_live: bool,
        /// End the running soak after its final accounting.
        #[arg(long, conflicts_with = "status")]
        stop: bool,
        /// Show progress or the latest report instead of starting a run.
        #[arg(long)]
        status: bool,
    },
}

enum Request {
    Get(String),
    Post(&'static str, Value),
    Put(&'static str, Value),
    Delete(String),
}

enum PairCmd {
    Start(String),
    Pending,
    Answer(String, bool),
}

fn parse_pair(args: &[String]) -> Result<PairCmd> {
    Ok(match args {
        [p] if p == "pending" => PairCmd::Pending,
        [a, id] if a == "accept" => PairCmd::Answer(id.clone(), true),
        [a, id] if a == "reject" => PairCmd::Answer(id.clone(), false),
        [target] => PairCmd::Start(target.clone()),
        _ => bail!("usage: novalink pair <name|id> | pending | accept <id> | reject <id>"),
    })
}

fn request_for(cmd: &Cmd) -> Result<Request> {
    Ok(match cmd {
        Cmd::Status | Cmd::Devices => Request::Get("/v1/status".into()),
        Cmd::Discovered => Request::Get("/v1/discovered".into()),
        Cmd::Pair { args } => match parse_pair(args)? {
            PairCmd::Pending => Request::Get("/v1/pair/pending".into()),
            PairCmd::Start(t) => Request::Post("/v1/pair", json!({ "name": t })),
            PairCmd::Answer(id, accept) => {
                Request::Post("/v1/pair/answer", json!({ "id": id, "accept": accept }))
            }
        },
        Cmd::Unpair { device } => Request::Delete(format!("/v1/devices/{device}")),
        Cmd::Layout { edge, peer } => {
            Request::Put("/v1/layout", json!({ "edge": edge, "peer": peer }))
        }
        Cmd::Metrics => Request::Get("/v1/metrics".into()),
        Cmd::Events { limit } => Request::Get(format!("/v1/events?limit={limit}")),
        Cmd::Doctor => Request::Get("/v1/doctor".into()),
        Cmd::Soak { stop: true, .. } => Request::Delete("/v1/soak".into()),
        Cmd::Soak { status: true, .. } => Request::Get("/v1/soak".into()),
        Cmd::Soak {
            hours: Some(h),
            peer: Some(p),
            allow_live,
            ..
        } => Request::Post(
            "/v1/soak",
            json!({ "peer": p, "hours": h, "allow_live": allow_live }),
        ),
        Cmd::Soak { .. } => {
            bail!("usage: novalink soak --hours <n> --peer <name|id> | --status | --stop")
        }
    })
}

/// Drops control characters and bidi overrides from text that came off the network, so
/// it cannot drive the terminal.
fn clean(s: &str) -> String {
    s.chars()
        .filter(|c| {
            !c.is_control() && !matches!(*c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
        })
        .collect()
}

fn clean_value(v: Value) -> Value {
    match v {
        Value::String(s) => Value::String(clean(&s)),
        Value::Array(a) => Value::Array(a.into_iter().map(clean_value).collect()),
        Value::Object(m) => Value::Object(
            m.into_iter()
                .map(|(k, v)| (clean(&k), clean_value(v)))
                .collect(),
        ),
        other => other,
    }
}

fn short(id: &Value) -> &str {
    id.as_str().map(|s| &s[..s.len().min(8)]).unwrap_or("?")
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "-".into(),
        other => other.to_string(),
    }
}

fn ms(us: &Value) -> String {
    us.as_u64()
        .map(|u| format!("{:.1} ms", u as f64 / 1000.0))
        .unwrap_or_else(|| "-".into())
}

fn render(cmd: &Cmd, v: &Value) -> String {
    let mut out = String::new();
    let mut line = |s: String| {
        out.push_str(&s);
        out.push('\n');
    };
    match cmd {
        Cmd::Status => {
            line(format!("{} ({})", text(&v["name"]), short(&v["me"])));
            line(format!(
                "controller {}  focus {}",
                short(&v["controller"]),
                short(&v["focus"])
            ));
            if let Some(c) = v["capture"].as_str().filter(|&c| c != "running") {
                line(match c {
                    "waiting_for_permission" => "Input capture: waiting for permission".into(),
                    other => format!("Input capture: {}", other.replace('_', " ")),
                });
            }
            for p in v["peers"].as_array().into_iter().flatten() {
                line(format!(
                    "  {} {}  {}",
                    if p["connected"] == true {
                        "up  "
                    } else {
                        "down"
                    },
                    text(&p["name"]),
                    short(&p["id"])
                ));
            }
            for l in v["layout"]["links"].as_array().into_iter().flatten() {
                line(format!(
                    "  layout: {} {} -> {}",
                    short(&l["from"]),
                    text(&l["from_edge"]),
                    short(&l["to"])
                ));
            }
        }
        Cmd::Devices => {
            let peers = v["peers"].as_array().cloned().unwrap_or_default();
            if peers.is_empty() {
                line("No paired devices.".into());
            }
            for p in &peers {
                line(format!(
                    "{:<20} {}  {}  {}  rtt {}",
                    text(&p["name"]),
                    short(&p["id"]),
                    if p["connected"] == true {
                        "connected"
                    } else {
                        "offline"
                    },
                    text(&p["path"]),
                    ms(&p["rtt_us"])
                ));
            }
        }
        Cmd::Discovered => {
            let a = v.as_array().cloned().unwrap_or_default();
            if a.is_empty() {
                line("No unpaired devices found.".into());
            }
            for d in &a {
                line(format!("{:<20} {}", text(&d["name"]), short(&d["id"])));
            }
        }
        Cmd::Pair { args } => match parse_pair(args) {
            Ok(PairCmd::Pending) => {
                let a = v.as_array().cloned().unwrap_or_default();
                if a.is_empty() {
                    line("No pairing requests waiting.".into());
                }
                for p in &a {
                    line(format!(
                        "{}  code {}  id {}",
                        text(&p["name"]),
                        text(&p["code"]),
                        short(&p["peer"])
                    ));
                }
            }
            Ok(PairCmd::Start(_)) => {
                line("Pairing started. Check the code with: novalink pair pending".into())
            }
            _ => line("Done.".into()),
        },
        Cmd::Unpair { .. } => line("Unpaired.".into()),
        Cmd::Layout { edge, peer } => line(format!("{peer} is now on the {edge} edge.")),
        Cmd::Metrics => {
            line(format!(
                "safety releases {}  reconnects {}",
                text(&v["safety_releases"]),
                text(&v["reconnects"])
            ));
            for r in v["per_peer"].as_array().into_iter().flatten() {
                let w = |i: usize, n: &str| {
                    format!(
                        "{n} p50 {} p99 {} (n={})",
                        ms(&r[i]["p50_us"]),
                        ms(&r[i]["p99_us"]),
                        text(&r[i]["count"])
                    )
                };
                line(format!(
                    "{}: {}; {}; {}",
                    short(&r[0]),
                    w(1, "min"),
                    w(2, "hour"),
                    w(3, "day")
                ));
            }
        }
        Cmd::Events { .. } => {
            for e in v.as_array().into_iter().flatten() {
                line(e.to_string());
            }
        }
        Cmd::Soak { stop: true, .. } => {
            line("Stop requested. See the result with: novalink soak --status".into())
        }
        Cmd::Soak { status: false, .. } => {
            line("Soak started. Check on it with: novalink soak --status".into())
        }
        Cmd::Soak { .. } => match text(&v["state"]).as_str() {
            "running" => line(format!(
                "running: {:.0}/{:.0} s, crossings {}, leaves {}, keys sent {}",
                v["elapsed_secs"].as_f64().unwrap_or(0.0),
                v["duration_secs"].as_f64().unwrap_or(0.0),
                text(&v["crossings"]),
                text(&v["leaves"]),
                text(&v["keys_sent"])
            )),
            state @ ("done" | "stopped" | "aborted") => {
                let r = &v["report"];
                line(format!("run {state}"));
                if !r["abort_reason"].is_null() {
                    line(format!("reason: {}", text(&r["abort_reason"])));
                }
                for k in [
                    "duration",
                    "crossings",
                    "leaves",
                    "held_reports",
                    "keys_sent",
                    "held_before_release_total",
                    "stuck_after_release",
                    "unanswered_leaves",
                    "leaves_lost_to_disconnect",
                    "safety_releases",
                    "reconnects",
                ] {
                    line(format!("{k}: {}", text(&r[k])));
                }
                let verdict = |bad: bool, what: &str| {
                    format!(
                        "{what}: {}",
                        if bad { "FOUND (fail)" } else { "none (pass)" }
                    )
                };
                line(verdict(r["stuck_after_release"] != 0, "stuck keys"));
                line(verdict(
                    r["unanswered_leaves"] != 0,
                    "unanswered release checks",
                ));
                line(format!(
                    "(leaves lost to a disconnect, not a failure: {})",
                    text(&r["leaves_lost_to_disconnect"])
                ));
            }
            "failed" => line(format!("failed: {}", text(&v["error"]))),
            _ => line("No soak has run.".into()),
        },
        Cmd::Doctor => {
            for c in v["checks"].as_array().into_iter().flatten() {
                line(format!(
                    "[{}] {}: {}",
                    if c["ok"] == true { "ok" } else { "!!" },
                    text(&c["name"]),
                    text(&c["detail"])
                ));
                if c["ok"] != true && !text(&c["advice"]).is_empty() {
                    line(format!("     {}", text(&c["advice"])));
                }
            }
        }
    }
    out
}

enum Failure {
    NotRunning,
    Impostor,
    Other(String),
}

impl From<ClientError> for Failure {
    fn from(e: ClientError) -> Failure {
        match e {
            ClientError::NotRunning | ClientError::TokenMissing => Failure::NotRunning,
            ClientError::Impostor => Failure::Impostor,
            ClientError::Api { status, message } => {
                Failure::Other(format!("{} (HTTP {status})", clean(&message)))
            }
            ClientError::Other(m) => Failure::Other(clean(&m)),
        }
    }
}

fn run(cli: &Cli) -> Result<String, Failure> {
    let req = request_for(&cli.cmd).map_err(|e| Failure::Other(e.to_string()))?;
    let client = Client::local()?;
    let v = match req {
        Request::Get(p) => client.request(Method::GET, &p, None),
        Request::Post(p, b) => client.request(Method::POST, p, Some(b)),
        Request::Put(p, b) => client.request(Method::PUT, p, Some(b)),
        Request::Delete(p) => client.request(Method::DELETE, &p, None),
    }?;
    let v = clean_value(v);
    if cli.json {
        return Ok(format!(
            "{}\n",
            serde_json::to_string_pretty(&v).unwrap_or_default()
        ));
    }
    Ok(render(&cli.cmd, &v))
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(out) => {
            print!("{out}");
            ExitCode::SUCCESS
        }
        Err(Failure::NotRunning) => {
            eprintln!("{NOT_RUNNING}");
            ExitCode::from(2)
        }
        Err(Failure::Impostor) => {
            eprintln!("{IMPOSTOR}");
            ExitCode::from(3)
        }
        Err(Failure::Other(m)) => {
            eprintln!("{m}");
            ExitCode::from(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pair_subcommands_parse() {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        assert!(matches!(parse_pair(&s(&["pending"])), Ok(PairCmd::Pending)));
        assert!(matches!(
            parse_pair(&s(&["accept", "ab"])),
            Ok(PairCmd::Answer(_, true))
        ));
        assert!(matches!(
            parse_pair(&s(&["reject", "ab"])),
            Ok(PairCmd::Answer(_, false))
        ));
        assert!(matches!(parse_pair(&s(&["laptop"])), Ok(PairCmd::Start(_))));
        assert!(parse_pair(&s(&["a", "b"])).is_err());
    }

    #[test]
    fn json_flag_is_global_and_layout_parses() {
        let c = Cli::try_parse_from(["novalink", "layout", "right", "mac", "--json"]).unwrap();
        assert!(c.json);
        assert!(Cli::try_parse_from(["novalink", "layout", "up", "mac"]).is_err());
        let c = Cli::try_parse_from(["novalink", "events", "--limit", "5"]).unwrap();
        assert!(matches!(c.cmd, Cmd::Events { limit: 5 }));
    }

    #[test]
    fn soak_command_builds_requests() {
        let c = Cli::try_parse_from(["novalink", "soak", "--hours", "24", "--peer", "pc"]).unwrap();
        match request_for(&c.cmd).unwrap() {
            Request::Post("/v1/soak", b) => {
                assert_eq!(b, json!({"peer":"pc","hours":24.0,"allow_live":false}))
            }
            _ => panic!("expected POST /v1/soak"),
        }
        let c = Cli::try_parse_from(["novalink", "soak", "--status"]).unwrap();
        assert!(matches!(request_for(&c.cmd), Ok(Request::Get(p)) if p == "/v1/soak"));
        let c = Cli::try_parse_from(["novalink", "soak"]).unwrap();
        assert!(request_for(&c.cmd).is_err());
        assert!(Cli::try_parse_from(["novalink", "soak", "--hours", "1"]).is_err());
        let done = json!({"state":"done","report":{"stuck_after_release":0,"unanswered_leaves":0,"crossings":3}});
        let c = Cli::try_parse_from(["novalink", "soak", "--stop"]).unwrap();
        assert!(matches!(request_for(&c.cmd), Ok(Request::Delete(p)) if p == "/v1/soak"));
        let c = Cli::try_parse_from([
            "novalink",
            "soak",
            "--hours",
            "1",
            "--peer",
            "pc",
            "--allow-live",
        ])
        .unwrap();
        match request_for(&c.cmd).unwrap() {
            Request::Post(_, b) => assert_eq!(b["allow_live"], true),
            _ => panic!(),
        }
        let s = render(
            &Cli::try_parse_from(["novalink", "soak", "--status"])
                .unwrap()
                .cmd,
            &done,
        );
        assert!(s.contains("none (pass)") && s.contains("crossings: 3"));
    }

    #[test]
    fn network_text_is_stripped_of_control_characters() {
        assert_eq!(clean("a\u{1b}[2Jb\u{85}c\u{7f}\u{202e}d"), "a[2Jbcd");
        let v = clean_value(json!({"name\u{7}": ["x\u{9b}y"]}));
        assert_eq!(v, json!({"name": ["xy"]}));
    }

    #[test]
    fn listings_show_short_ids_next_to_names() {
        let id = "ab".repeat(32);
        let s = render(&Cmd::Discovered, &json!([{"name":"mac","id":id}]));
        assert!(s.contains("mac") && s.contains("abababab") && !s.contains(&id));
        let cmd = Cmd::Pair {
            args: vec!["pending".into()],
        };
        let s = render(&cmd, &json!([{"name":"mac","code":"123456","peer":id}]));
        assert!(s.contains("abababab") && !s.contains(&id));
    }

    #[test]
    fn devices_and_layout_print_strings_without_quotes() {
        let id = "cd".repeat(32);
        let v = json!({"peers":[{"name":"pc","id":id,"connected":true,"path":"Ethernet","rtt_us":1500}]});
        let s = render(&Cmd::Devices, &v);
        assert!(s.contains("connected  Ethernet  rtt 1.5 ms"), "{s}");
        assert!(!s.contains('"'), "{s}");
        let v = json!({"me":id,"focus":id,"controller":id,"peers":[],
            "layout":{"links":[{"from":id,"from_edge":"Right","to":id}]}});
        let s = render(&Cmd::Status, &v);
        assert!(s.contains("layout: cdcdcdcd Right -> cdcdcdcd"), "{s}");
    }

    #[test]
    fn status_says_when_input_capture_is_waiting() {
        let id = "cd".repeat(32);
        let mut v = json!({"me":id,"focus":id,"controller":id,"peers":[],
            "layout":{"links":[]},"capture":"waiting_for_permission"});
        let s = render(&Cmd::Status, &v);
        assert!(s.contains("Input capture: waiting for permission"), "{s}");
        v["capture"] = json!("running");
        assert!(!render(&Cmd::Status, &v).contains("Input capture"));
    }

    #[test]
    fn doctor_renders_advice_for_failures() {
        let v = json!({"checks":[{"name":"a","ok":false,"detail":"bad","advice":"Fix it."},
                                 {"name":"b","ok":true,"detail":"fine","advice":""}]});
        let s = render(&Cmd::Doctor, &v);
        assert!(s.contains("[!!] a: bad") && s.contains("Fix it.") && s.contains("[ok] b"));
    }
}
