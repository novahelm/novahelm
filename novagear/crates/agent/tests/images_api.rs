//! `GET /api/images/{name}`: the panel's key previews. A real router on an ephemeral port.

use novagear_agent::{Agent, api};
use novagear_core::API_PORT;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const TOKEN: &str = "agent-token-0123456789012345678901234567890123";

struct Reply {
    code: u16,
    head: String,
    body: Vec<u8>,
}

impl Reply {
    fn header(&self, name: &str) -> Option<String> {
        self.head.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.eq_ignore_ascii_case(name).then(|| v.trim().to_string())
        })
    }
}

async fn serve(tag: &str) -> (Arc<Agent>, SocketAddr, std::path::PathBuf) {
    let dir =
        std::env::temp_dir().join(format!("novagear-images-api-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let agent = Agent::new(dir.join("config.json"), TOKEN.to_string()).unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let served = agent.clone();
    tokio::spawn(async move {
        axum::serve(listener, api::router(served)).await.unwrap();
    });
    (agent, addr, dir)
}

async fn get(addr: SocketAddr, path: &str, token: &str) -> Reply {
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    let req = format!(
        "GET /api{path} HTTP/1.1\r\nHost: 127.0.0.1:{API_PORT}\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
    );
    s.write_all(req.as_bytes()).await.unwrap();
    let mut out = Vec::new();
    s.read_to_end(&mut out).await.unwrap();
    let at = out.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
    let head = String::from_utf8_lossy(&out[..at]).to_string();
    let code = head.split(' ').nth(1).unwrap().parse().unwrap();
    Reply {
        code,
        head,
        body: out[at + 4..].to_vec(),
    }
}

fn png() -> Vec<u8> {
    let mut out = Vec::new();
    image::RgbaImage::from_pixel(4, 4, image::Rgba([9, 99, 199, 255]))
        .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .unwrap();
    out
}

fn assert_safe_headers(r: &Reply) {
    assert_eq!(
        r.header("x-content-type-options").as_deref(),
        Some("nosniff")
    );
    let csp = r.header("content-security-policy").unwrap_or_default();
    assert!(csp.contains("sandbox"), "csp: {csp:?}");
}

#[tokio::test]
async fn a_stored_image_is_served_with_its_type_and_safe_headers() {
    let (agent, addr, _dir) = serve("user").await;
    let bytes = png();
    let name = agent.store_image(&bytes).unwrap();
    let r = get(addr, &format!("/images/{name}"), TOKEN).await;
    assert_eq!(r.code, 200);
    assert_eq!(r.header("content-type").as_deref(), Some("image/png"));
    assert_eq!(r.body, bytes);
    assert_safe_headers(&r);
}

#[tokio::test]
async fn a_bundled_svg_is_served_as_svg_that_cannot_run_script() {
    let (_agent, addr, _dir) = serve("bundled").await;
    let r = get(addr, "/images/lucide%2Fhand.svg", TOKEN).await;
    assert_eq!(r.code, 200, "{}", r.head);
    assert_eq!(r.header("content-type").as_deref(), Some("image/svg+xml"));
    assert!(String::from_utf8_lossy(&r.body).contains("<svg"));
    assert_safe_headers(&r);
}

#[tokio::test]
async fn a_legacy_icon_is_found_and_the_images_folder_wins_over_it() {
    let (agent, addr, dir) = serve("legacy").await;
    std::fs::create_dir_all(dir.join("icons")).unwrap();
    std::fs::write(dir.join("icons/old.png"), b"legacy-bytes").unwrap();
    let r = get(addr, "/images/old.png", TOKEN).await;
    assert_eq!((r.code, r.body.as_slice()), (200, &b"legacy-bytes"[..]));
    // The same name in the user's images folder is the one the deck draws.
    std::fs::create_dir_all(agent.images_dir()).unwrap();
    std::fs::write(agent.images_dir().join("old.png"), b"user-bytes").unwrap();
    let r = get(addr, "/images/old.png", TOKEN).await;
    assert_eq!(r.body, b"user-bytes");
}

#[tokio::test]
async fn names_that_are_not_plain_never_reach_the_disk() {
    let (_agent, addr, dir) = serve("names").await;
    std::fs::write(dir.join("secret.txt"), b"nope").unwrap();
    for name in [
        "..%2Fsecret.txt",
        "%2e%2e%2fsecret.txt",
        "..%5Csecret.txt",
        "sub%2F..%2Fsecret.txt",
        "%2Fetc%2Fpasswd",
        "a%00b.png",
    ] {
        let r = get(addr, &format!("/images/{name}"), TOKEN).await;
        assert!(
            matches!(r.code, 400 | 404),
            "{name} answered {}: {:?}",
            r.code,
            String::from_utf8_lossy(&r.body)
        );
        assert!(!String::from_utf8_lossy(&r.body).contains("nope"));
    }
    let r = get(addr, "/images/missing.png", TOKEN).await;
    assert_eq!(r.code, 404);
}

#[tokio::test]
async fn the_route_needs_a_staff_token_and_a_client_cannot_use_it() {
    let (agent, addr, _dir) = serve("auth").await;
    let name = agent.store_image(&png()).unwrap();
    assert_eq!(
        get(addr, &format!("/images/{name}"), "wrong").await.code,
        401
    );
    let token = agent.clients.lock().unwrap().add("nova").unwrap();
    assert_eq!(
        get(addr, &format!("/images/{name}"), &token).await.code,
        403
    );
    assert!(!novagear_agent::clients::client_may(
        "GET",
        "/images/anything.png"
    ));
}
