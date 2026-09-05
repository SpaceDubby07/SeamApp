//! 6-digit pairing code derivation (Tier 7.6, M8).
//!
//! Both sides display this code on first connection and a human confirms
//! they match before either one pins the other's certificate fingerprint.
//! This is what actually defeats a MITM: an attacker sitting on the
//! connection can't just relay it transparently once TLS is in play (it
//! doesn't have the private key for either real node's certificate), and
//! substituting its OWN certificate for one side changes that side's
//! fingerprint — which changes the code the human sees, making the
//! mismatch visible before either fingerprint gets pinned.

use crate::net::tls::Fingerprint;

/// Derives the pairing code from both sides' certificate fingerprints.
/// Order-independent (the two fingerprints are sorted before hashing) so it
/// doesn't matter which side is "local" vs "peer" — both nodes compute the
/// identical code from the identical pair of fingerprints.
///
/// # Panics
/// Never in practice — the internal 4-byte slice-to-array conversion is
/// statically guaranteed to succeed (a BLAKE3 hash is always 32 bytes).
#[must_use]
pub fn pairing_code(a: Fingerprint, b: Fingerprint) -> String {
    let (first, second) = if a.as_bytes() <= b.as_bytes() {
        (a, b)
    } else {
        (b, a)
    };

    let mut hasher = blake3::Hasher::new();
    hasher.update(first.as_bytes());
    hasher.update(second.as_bytes());
    let hash = hasher.finalize();

    // Six digits, zero-padded — 20 bits' worth of entropy from the hash is
    // plenty for a human-compared "did the screens match" check, not a
    // cryptographic secret in its own right (the actual security property
    // comes from the fingerprints/certificates, not the code's bit length).
    let n = u32::from_be_bytes(hash.as_bytes()[0..4].try_into().expect("4 bytes"));
    format!("{:06}", n % 1_000_000)
}

#[cfg(test)]
mod tests {
    use super::pairing_code;
    use crate::net::tls::NodeIdentity;

    #[test]
    fn code_is_six_digits() {
        let a = NodeIdentity::generate().expect("identity a").fingerprint;
        let b = NodeIdentity::generate().expect("identity b").fingerprint;
        let code = pairing_code(a, b);
        assert_eq!(code.len(), 6);
        assert!(code.chars().all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn code_is_order_independent() {
        let a = NodeIdentity::generate().expect("identity a").fingerprint;
        let b = NodeIdentity::generate().expect("identity b").fingerprint;
        assert_eq!(pairing_code(a, b), pairing_code(b, a));
    }

    #[test]
    fn different_fingerprint_pairs_produce_different_codes() {
        let a = NodeIdentity::generate().expect("identity a").fingerprint;
        let b = NodeIdentity::generate().expect("identity b").fingerprint;
        let c = NodeIdentity::generate().expect("identity c").fingerprint;
        assert_ne!(pairing_code(a, b), pairing_code(a, c));
    }
}
