//! Golden JSON for the Mac app's tests, captured from the real agent router.
//!
//! The Swift tests in apple/Tests/NovaLinkKitTests decode these files, so the app is
//! checked against what the agent really sends. Volatile values (random ids, codes, ports,
//! timings) are replaced with fixed ones; keys, nesting and enum spellings are untouched.
//! To refresh after an API change: `NOVALINK_UPDATE_FIXTURES=1 cargo test -p novalink-client
//! --features novalink-agent/fake --test golden`.

use novalink_agent::api::router;
use novalink_agent::engine::EngineHandle;
use novalink_agent::testutil::{one_engine, two_engines, wait_until};
use novalink_client::Client;
use novalink_net::discovery::Discovered;
use reqwest::Method;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apple/Tests/NovaLinkKitTests/Fixtures")
}

async fn serve(h: EngineHandle) -> String {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", l.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(l, router(h, "tok".into())).await.unwrap();
    });
    base
}

async fn get(base: &str, path: &'static str) -> Value {
    let base = base.to_string();
    tokio::task::spawn_blocking(move || {
        Client::with(&base, "tok".into())
            .request(Method::GET, path, None)
            .unwrap()
    })
    .await
    .unwrap()
}

/// Replaces the random parts of a reply with stable ones.
struct Normalizer {
    ids: HashMap<String, String>,
}

impl Normalizer {
    fn new() -> Self {
        Normalizer {
            ids: HashMap::new(),
        }
    }

    fn id(&mut self, s: &str) -> String {
        let n = self.ids.len();
        self.ids
            .entry(s.to_string())
            .or_insert_with(|| {
                let digit = char::from_digit(n as u32 + 10, 16).expect("few ids");
                digit.to_string().repeat(64)
            })
            .clone()
    }

    fn value(&mut self, key: &str, v: Value) -> Value {
        match v {
            Value::String(s) if s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()) => {
                Value::String(self.id(&s))
            }
            Value::String(_) if key == "code" => json!("123456"),
            Value::String(s) if key == "detail" && s.contains("round trip") => {
                let head = s.split("round trip").next().unwrap_or_default();
                Value::String(format!("{head}round trip 0.4 ms"))
            }
            Value::String(s) if key == "addrs" || key == "addr" => {
                let ip = s.rsplit_once(':').map_or(s.as_str(), |(ip, _)| ip);
                Value::String(format!("{ip}:47979"))
            }
            Value::Number(_) if key == "rtt_us" => json!(412),
            Value::Number(_) if key == "clock_offset_us" => json!(-35),
            Value::Array(a) => Value::Array(a.into_iter().map(|x| self.value(key, x)).collect()),
            Value::Object(m) => Value::Object(
                m.into_iter()
                    .map(|(k, v)| {
                        let v = self.value(&k, v);
                        (k, v)
                    })
                    .collect(),
            ),
            other => other,
        }
    }
}

/// Writes the file when asked to, otherwise fails if it differs from `v`.
fn check(name: &str, v: &Value) {
    let path = fixtures_dir().join(name);
    let text = serde_json::to_string_pretty(v).unwrap() + "\n";
    if std::env::var_os("NOVALINK_UPDATE_FIXTURES").is_some() {
        std::fs::create_dir_all(fixtures_dir()).unwrap();
        std::fs::write(&path, text).unwrap();
        return;
    }
    let on_disk = std::fs::read_to_string(&path).unwrap_or_default();
    let on_disk: Value = serde_json::from_str(&on_disk).unwrap_or(Value::Null);
    assert_eq!(
        &on_disk,
        v,
        "{} is out of date; rerun with NOVALINK_UPDATE_FIXTURES=1",
        path.display()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn status_and_doctor_with_a_peer_and_a_layout_link() {
    let (a, _pa, _b, _pb) = two_engines().await;
    let base = serve(a).await;
    let mut n = Normalizer::new();
    let status = n.value("", get(&base, "/v1/status").await);
    assert_eq!(status["layout"]["links"][0]["from_edge"], "Right");
    let mut doctor = n.value("", get(&base, "/v1/doctor").await);
    // The fixture is the Mac's answer. Windows adds two checks of its own, whose results depend on the machine the
    // test runs on (its network profile and firewall rules), so they are left out of the comparison there.
    if cfg!(windows)
        && let Some(checks) = doctor["checks"].as_array_mut()
    {
        checks.retain(|c| c["name"] != "network" && c["name"] != "firewall");
    }
    check("status.json", &status);
    check("doctor.json", &doctor);
}

#[tokio::test(flavor = "multi_thread")]
async fn pending_and_discovered() {
    let (ha, _pa) = one_engine().await;
    let (hb, _pb) = one_engine().await;
    ha.manager().hint_peer(Discovered {
        id: hb.status().me,
        name: "Desk-PC".into(),
        addrs: vec![hb.manager().local_addr()],
    });
    let base = serve(ha.clone()).await;
    let mut n = Normalizer::new();
    n.id(&ha.status().me.to_string());
    let discovered = n.value("", get(&base, "/v1/discovered").await);
    ha.request_pair(hb.status().me).await.unwrap();
    wait_until(|| !ha.pending_pairs().is_empty()).await;
    let pending = n.value("", get(&base, "/v1/pair/pending").await);
    check("discovered.json", &discovered);
    check("pending.json", &pending);
}
