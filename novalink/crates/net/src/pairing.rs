//! Pairing primitives: commit-then-reveal short authentication string.
//!
//! Flow (driven by the session layer): the dialer sends `commitment(its id,
//! its nonce)` first; the acceptor then reveals its nonce; the dialer then
//! reveals its nonce, and the acceptor checks it against the commitment. Both
//! sides compute `sas_code` over both ids, both nonces and a `binding`, the
//! TLS keying-material exporter of the session. Committing first stops a
//! LAN attacker from grinding keypairs offline until the codes collide, and
//! the binding ties the code to this exact TLS session.
use novalink_proto::DeviceId;
use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};

/// Fresh 32-byte nonce from the OS RNG.
pub fn new_nonce() -> [u8; 32] {
    let mut n = [0u8; 32];
    OsRng.fill_bytes(&mut n);
    n
}

/// `sha256("novalink-pair-commit-v1" || id || nonce)`.
pub fn commitment(id: &DeviceId, nonce: &[u8; 32]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"novalink-pair-commit-v1");
    h.update(id.0);
    h.update(nonce);
    h.finalize().into()
}

/// Six digits, identical on both sides regardless of argument order.
pub fn sas_code(
    a: &DeviceId,
    nonce_a: &[u8; 32],
    b: &DeviceId,
    nonce_b: &[u8; 32],
    binding: &[u8],
) -> String {
    let (first, second) = if a <= b {
        ((a, nonce_a), (b, nonce_b))
    } else {
        ((b, nonce_b), (a, nonce_a))
    };
    let mut h = Sha256::new();
    h.update(b"novalink-pair-sas-v1");
    for (id, nonce) in [first, second] {
        h.update(id.0);
        h.update(nonce);
    }
    h.update(binding);
    let d = h.finalize();
    let n = u64::from_be_bytes(d[..8].try_into().expect("8 bytes")) % 1_000_000;
    format!("{n:06}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: DeviceId = DeviceId([1; 32]);
    const B: DeviceId = DeviceId([2; 32]);
    const NA: [u8; 32] = [10; 32];
    const NB: [u8; 32] = [20; 32];

    #[test]
    fn sas_is_symmetric_and_six_digits() {
        let c = sas_code(&A, &NA, &B, &NB, b"x");
        assert_eq!(c, sas_code(&B, &NB, &A, &NA, b"x"));
        assert_eq!(c.len(), 6);
        assert!(c.bytes().all(|b| b.is_ascii_digit()));
    }

    #[test]
    fn sas_changes_with_every_input() {
        let base = sas_code(&A, &NA, &B, &NB, b"x");
        assert_ne!(base, sas_code(&A, &[11; 32], &B, &NB, b"x"));
        assert_ne!(base, sas_code(&A, &NA, &B, &[21; 32], b"x"));
        assert_ne!(base, sas_code(&A, &NA, &B, &NB, b"y"));
        assert_ne!(base, sas_code(&A, &NA, &DeviceId([3; 32]), &NB, b"x"));
    }

    #[test]
    fn commitment_differs_per_nonce_and_id() {
        assert_ne!(commitment(&A, &NA), commitment(&A, &NB));
        assert_ne!(commitment(&A, &NA), commitment(&B, &NA));
        assert_eq!(commitment(&A, &NA), commitment(&A, &NA));
        assert_ne!(new_nonce(), new_nonce());
    }

    #[test]
    fn golden_vector() {
        assert_eq!(sas_code(&A, &NA, &B, &NB, b"binding"), "978866");
    }
}
