//! Proof that the thing on the API port is the real agent, before anyone sends it the token.
//! Another local user could bind 127.0.0.1:47820 while the agent is down; the client sends
//! a fresh nonce and only trusts an answer that is an HMAC of it keyed by the token, which
//! only the real agent (and this user) can compute.

use hmac::{Hmac, Mac};
use sha2::Sha256;

const DOMAIN: &[u8] = b"novagear-agent-proof:";

/// A nonce is 16–64 bytes as hex; anything else is refused before any work is done.
pub fn valid_nonce(nonce: &str) -> bool {
    (32..=128).contains(&nonce.len()) && nonce.bytes().all(|b| b.is_ascii_hexdigit())
}

pub fn agent_proof(token: &str, nonce: &str) -> String {
    let mut mac =
        Hmac::<Sha256>::new_from_slice(token.as_bytes()).expect("HMAC takes any key length");
    mac.update(DOMAIN);
    mac.update(nonce.as_bytes());
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn new_nonce() -> anyhow::Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("no randomness: {e}"))?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

/// Equal-time compare so secrets can't be guessed byte by byte from response timing.
pub fn same(a: &str, b: &str) -> bool {
    a.len() == b.len()
        && a.bytes()
            .zip(b.bytes())
            .fold(0u8, |acc, (x, y)| acc | (x ^ y))
            == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proof_depends_on_token_and_nonce() {
        let n = new_nonce().unwrap();
        assert!(valid_nonce(&n));
        assert_eq!(agent_proof("t1", &n), agent_proof("t1", &n));
        assert_ne!(
            agent_proof("t1", &n),
            agent_proof("t2", &n),
            "a different token can't answer"
        );
        assert_ne!(
            agent_proof("t1", &n),
            agent_proof("t1", &new_nonce().unwrap()),
            "answers don't replay"
        );
        assert_eq!(agent_proof("t1", &n).len(), 64);
    }

    #[test]
    fn matches_the_shared_vector() {
        // The Mac app's NovaGearKitTests check the same vector, so both sides agree.
        assert_eq!(
            agent_proof("test-token", "00112233445566778899aabbccddeeff"),
            "497365c0eb80adc31462d13f0537c71930645cb7d67b3112472900796e485e8c"
        );
    }

    #[test]
    fn nonce_and_compare_rules() {
        assert!(!valid_nonce("abc"));
        assert!(!valid_nonce(&"z".repeat(32)));
        assert!(!valid_nonce(&"a".repeat(130)));
        assert!(same("abc", "abc"));
        assert!(!same("abc", "abd"));
        assert!(!same("abc", "ab"));
    }
}
