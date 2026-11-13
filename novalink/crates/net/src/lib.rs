//! Network layer: identity, trust, pairing, interface filter, and the pinned-key QUIC session.
pub mod discovery;
pub mod identity;
pub mod impair;
pub mod manager;
pub mod netif;
pub mod pairing;
pub mod session;
pub mod tls;
pub mod trust;

/// Longest peer-supplied device name we keep.
pub const MAX_NAME_CHARS: usize = 64;

/// Makes a peer-supplied name safe to store and print: drops control characters (C0, DEL,
/// C1) and bidi overrides, trims, and keeps at most [`MAX_NAME_CHARS`] characters.
pub fn sanitize_name(raw: &str) -> String {
    let kept: String = raw
        .chars()
        .filter(|c| {
            !c.is_control() && !matches!(*c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}')
        })
        .collect();
    kept.trim()
        .chars()
        .take(MAX_NAME_CHARS)
        .collect::<String>()
        .trim()
        .to_string()
}

#[cfg(test)]
mod sanitize_tests {
    use super::*;

    #[test]
    fn strips_controls_trims_and_truncates() {
        assert_eq!(
            sanitize_name("  Mac\x1b[31m\u{9b}\x7f book\n "),
            "Mac[31m book"
        );
        assert_eq!(sanitize_name("a\u{202e}b"), "ab");
        assert_eq!(sanitize_name(&"x".repeat(100)).chars().count(), 64);
        assert_eq!(sanitize_name("\x07\x08"), "");
        assert_eq!(sanitize_name("Büro-Mac"), "Büro-Mac");
    }
}
