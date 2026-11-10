//! Section 3 of the contract: the mutual proof both sides give on every
//! connection. Pure functions, so the fixed test vectors in the contract
//! appendix check them byte for byte.

use std::num::NonZeroU32;

use ring::{hmac, pbkdf2};

/// Protocol version spoken here (`v` in `hello` and `auth`).
pub const VERSION: u64 = 1;
/// PBKDF2-HMAC-SHA256 iterations for the pairing key K.
pub const PBKDF2_ITERATIONS: u32 = 200_000;
/// Salt prefix; the web username follows it.
pub const SALT_PREFIX: &str = "sentryusb-link|";

/// A 32-byte secret: K in pair mode, the link key in key mode. Zeroed on
/// drop and never printed.
#[derive(Clone)]
pub struct Secret([u8; 32]);

impl Secret {
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Secret(bytes)
    }

    /// Parses exactly 64 lowercase hex characters.
    pub fn from_hex(s: &str) -> Option<Self> {
        if !is_hex64(s) {
            return None;
        }
        let mut out = [0u8; 32];
        hex::decode_to_slice(s, &mut out).ok()?;
        Some(Secret(out))
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }

    fn key(&self) -> hmac::Key {
        hmac::Key::new(hmac::HMAC_SHA256, &self.0)
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        // Volatile writes so the compiler keeps the wipe.
        for b in self.0.iter_mut() {
            unsafe { std::ptr::write_volatile(b, 0) };
        }
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(<redacted>)")
    }
}

/// `K = PBKDF2-HMAC-SHA256(password, "sentryusb-link|" + username, 200000, 32)`.
/// Both strings are used as their UTF-8 bytes, with no normalization.
/// About a second on a Pi Zero 2: call it off the async runtime.
pub fn derive_pairing_key(username: &str, password: &str) -> Secret {
    let salt = format!("{SALT_PREFIX}{username}");
    let mut out = [0u8; 32];
    pbkdf2::derive(
        pbkdf2::PBKDF2_HMAC_SHA256,
        NonZeroU32::new(PBKDF2_ITERATIONS).expect("non-zero"),
        salt.as_bytes(),
        password.as_bytes(),
        &mut out,
    );
    Secret(out)
}

fn pi_message(manager_nonce: &str, pi_nonce: &str, fp: &str, id: &str) -> String {
    format!("pi|{manager_nonce}|{pi_nonce}|{fp}|{id}")
}

fn mgr_message(manager_nonce: &str, pi_nonce: &str, fp: &str, id: &str) -> String {
    format!("mgr|{pi_nonce}|{manager_nonce}|{fp}|{id}")
}

/// The Pi's proof, lowercase hex:
/// `HMAC-SHA256(S, "pi|" + managerNonce + "|" + piNonce + "|" + fp + "|" + id)`.
pub fn pi_proof(s: &Secret, manager_nonce: &str, pi_nonce: &str, fp: &str, id: &str) -> String {
    let tag = hmac::sign(&s.key(), pi_message(manager_nonce, pi_nonce, fp, id).as_bytes());
    hex::encode(tag.as_ref())
}

/// Manager's proof, lowercase hex:
/// `HMAC-SHA256(S, "mgr|" + piNonce + "|" + managerNonce + "|" + fp + "|" + id)`.
pub fn mgr_proof(s: &Secret, manager_nonce: &str, pi_nonce: &str, fp: &str, id: &str) -> String {
    let tag = hmac::sign(&s.key(), mgr_message(manager_nonce, pi_nonce, fp, id).as_bytes());
    hex::encode(tag.as_ref())
}

/// Checks Manager's proof in constant time (`ring::hmac::verify`). A proof
/// that is not 64 hex characters fails without touching the key.
pub fn verify_mgr_proof(
    s: &Secret,
    manager_nonce: &str,
    pi_nonce: &str,
    fp: &str,
    id: &str,
    proof_hex: &str,
) -> bool {
    if !is_hex64(proof_hex) {
        return false;
    }
    let mut tag = [0u8; 32];
    if hex::decode_to_slice(proof_hex, &mut tag).is_err() {
        return false;
    }
    hmac::verify(&s.key(), mgr_message(manager_nonce, pi_nonce, fp, id).as_bytes(), &tag).is_ok()
}

/// Exactly 64 lowercase hex characters: nonces, fingerprints, keys, proofs.
pub fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Constant-time equality for equal-length strings (fingerprints).
pub fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// 32 random bytes as 64 lowercase hex characters.
pub fn random_hex32() -> anyhow::Result<String> {
    use ring::rand::SecureRandom;
    let mut b = [0u8; 32];
    ring::rand::SystemRandom::new()
        .fill(&mut b)
        .map_err(|_| anyhow::anyhow!("system random failed"))?;
    Ok(hex::encode(b))
}

#[cfg(test)]
mod tests {
    use super::*;

    // The test vectors from the contract appendix (docs/sentry-manager-link.md).
    // They were also computed independently with Python's hashlib/hmac.
    const PASSWORD: &str = "correct horse battery staple";
    const USERNAME: &str = "admin";
    const K_HEX: &str = "42dbcc8ba07c3b03314a2cec3c9cae2adf011049eb445394ef1d3a11b94f6205";
    const MANAGER_NONCE: &str = "000102030405060708090a0b0c0d0e0f101112131415161718191a1b1c1d1e1f";
    const PI_NONCE: &str = "202122232425262728292a2b2c2d2e2f303132333435363738393a3b3c3d3e3f";
    const FP: &str = "61ecb6872005d76f9c85b9abee8c635c413381103bf5df251ec263fc331d6c27";
    const ID: &str = "0123456789abcdef0123456789abcdef";
    const LINK_KEY: &str = "404142434445464748494a4b4c4d4e4f505152535455565758595a5b5c5d5e5f";
    const PAIR_PI: &str = "1425df08a1362d43a7d44a6059ba7f078df99375d282bc35221ccd364acebbcf";
    const PAIR_MGR: &str = "2f864f5224cad380d328c3b8fa62104855ca1ce1eaaccf5dc12a49535cd2ce1e";
    const KEY_PI: &str = "852e6aa89c5f0fd2bd95f2ab6471d17353ffa98b3782d8b014db20c9bf865e50";
    const KEY_MGR: &str = "cc691f70debff4e37baa54f32bdd250a5fcdfa8efcffeca8f1c7de92a2c54bef";

    #[test]
    fn pairing_key_matches_the_vector() {
        assert_eq!(derive_pairing_key(USERNAME, PASSWORD).to_hex(), K_HEX);
    }

    #[test]
    fn fingerprint_vector_is_sha256_of_its_label() {
        let d = ring::digest::digest(&ring::digest::SHA256, b"sentry-manager test certificate");
        assert_eq!(hex::encode(d.as_ref()), FP);
    }

    #[test]
    fn pair_mode_proofs_match_the_vectors() {
        let k = Secret::from_hex(K_HEX).unwrap();
        assert_eq!(pi_proof(&k, MANAGER_NONCE, PI_NONCE, FP, ID), PAIR_PI);
        assert_eq!(mgr_proof(&k, MANAGER_NONCE, PI_NONCE, FP, ID), PAIR_MGR);
        assert!(verify_mgr_proof(&k, MANAGER_NONCE, PI_NONCE, FP, ID, PAIR_MGR));
    }

    #[test]
    fn key_mode_proofs_match_the_vectors() {
        let s = Secret::from_hex(LINK_KEY).unwrap();
        assert_eq!(pi_proof(&s, MANAGER_NONCE, PI_NONCE, FP, ID), KEY_PI);
        assert_eq!(mgr_proof(&s, MANAGER_NONCE, PI_NONCE, FP, ID), KEY_MGR);
        assert!(verify_mgr_proof(&s, MANAGER_NONCE, PI_NONCE, FP, ID, KEY_MGR));
    }

    #[test]
    fn proofs_are_bound_to_every_input() {
        let k = Secret::from_hex(K_HEX).unwrap();
        // The Pi's own proof echoed back is not Manager's proof (reflection).
        assert!(!verify_mgr_proof(&k, MANAGER_NONCE, PI_NONCE, FP, ID, PAIR_PI));
        // A proof for another certificate (a man in the middle) fails.
        let other_fp = "f".repeat(64);
        assert!(!verify_mgr_proof(&k, MANAGER_NONCE, PI_NONCE, &other_fp, ID, PAIR_MGR));
        // A replayed proof from an earlier connection (other Pi nonce) fails.
        let other_nonce = "e".repeat(64);
        assert!(!verify_mgr_proof(&k, MANAGER_NONCE, &other_nonce, FP, ID, PAIR_MGR));
        // Another device id fails.
        assert!(!verify_mgr_proof(&k, MANAGER_NONCE, PI_NONCE, FP, "x", PAIR_MGR));
        // The pair-mode proof does not pass in key mode.
        let s = Secret::from_hex(LINK_KEY).unwrap();
        assert!(!verify_mgr_proof(&s, MANAGER_NONCE, PI_NONCE, FP, ID, PAIR_MGR));
        // Uppercase or malformed proofs are refused before any comparison.
        assert!(!verify_mgr_proof(&k, MANAGER_NONCE, PI_NONCE, FP, ID, &PAIR_MGR.to_uppercase()));
        assert!(!verify_mgr_proof(&k, MANAGER_NONCE, PI_NONCE, FP, ID, ""));
    }

    #[test]
    fn hex_and_secret_parsing() {
        assert!(is_hex64(FP));
        assert!(!is_hex64(&FP.to_uppercase()));
        assert!(!is_hex64(&FP[..63]));
        assert!(Secret::from_hex("zz").is_none());
        assert_eq!(format!("{:?}", Secret::from_hex(K_HEX).unwrap()), "Secret(<redacted>)");
        assert!(ct_eq(FP, FP) && !ct_eq(FP, PI_NONCE) && !ct_eq("a", "ab"));
        let n = random_hex32().unwrap();
        assert!(is_hex64(&n) && n != random_hex32().unwrap());
    }
}
