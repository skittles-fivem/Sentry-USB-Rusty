//! Section 3 of the contract: the mutual proof both sides give on every
//! connection. Pure functions, so the fixed test vectors in the contract
//! appendix check them byte for byte.

use std::num::NonZeroU32;

use ring::{hmac, pbkdf2};

/// Protocol version spoken here (`v` in `hello` and `auth`). Version 2 keys
/// pair mode with the pairing code as well as the web password; a peer on
/// any other version is refused with `{"t":"denied","why":"version"}`.
pub const VERSION: u64 = 2;
/// PBKDF2-HMAC-SHA256 iterations for the pairing key K.
pub const PBKDF2_ITERATIONS: u32 = 200_000;
/// Salt prefix; the web username follows it.
pub const SALT_PREFIX: &str = "sentryusb-link|";

/// Crockford base32: the pairing code's alphabet (no I, L, O or U).
pub const PAIR_CODE_ALPHABET: &str = "0123456789ABCDEFGHJKMNPQRSTVWXYZ";
/// Characters in a pairing code (60 bits), after normalization.
pub const PAIR_CODE_LEN: usize = 12;
/// The only error a malformed pairing code gives.
pub const BAD_PAIR_CODE: &str = "That isn't a valid pairing code";
/// Label before the normalized code in P's HMAC message.
pub const PAIR_CODE_LABEL: &str = "pair-code|";

/// A pairing code, normalized: 12 characters of `PAIR_CODE_ALPHABET`. Held
/// in memory only while a pairing window is open; never serialized,
/// printed or logged, and wiped on drop.
pub struct PairingCode(String);

impl PairingCode {
    /// Normalizes what the owner typed: uppercase, spaces and `-` dropped,
    /// `O` read as `0` and `I`/`L` as `1`. The result must be exactly 12
    /// characters from the alphabet, else `BAD_PAIR_CODE`.
    pub fn parse(input: &str) -> Result<Self, &'static str> {
        if input.len() > 64 {
            return Err(BAD_PAIR_CODE);
        }
        let mut out = String::with_capacity(PAIR_CODE_LEN);
        for c in input.chars() {
            let c = match c.to_ascii_uppercase() {
                ' ' | '-' => continue,
                'O' => '0',
                'I' | 'L' => '1',
                c => c,
            };
            if !PAIR_CODE_ALPHABET.contains(c) || out.len() == PAIR_CODE_LEN {
                return Err(BAD_PAIR_CODE);
            }
            out.push(c);
        }
        if out.len() != PAIR_CODE_LEN {
            return Err(BAD_PAIR_CODE);
        }
        Ok(PairingCode(out))
    }

    /// The normalized code. Only for deriving P (and tests).
    pub fn normalized(&self) -> &str {
        &self.0
    }
}

impl Drop for PairingCode {
    fn drop(&mut self) {
        let mut bytes = std::mem::take(&mut self.0).into_bytes();
        // Volatile writes so the compiler keeps the wipe.
        for b in bytes.iter_mut() {
            unsafe { std::ptr::write_volatile(b, 0) };
        }
    }
}

impl std::fmt::Debug for PairingCode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PairingCode(<redacted>)")
    }
}

/// A 32-byte secret: P in pair mode, the link key in key mode. Zeroed on
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

/// The pair-mode secret:
/// `P = HMAC-SHA256(key = K, message = "pair-code|" + normalizedCode)`.
pub fn derive_pair_secret(k: &Secret, code: &PairingCode) -> Secret {
    let msg = format!("{PAIR_CODE_LABEL}{}", code.normalized());
    let tag = hmac::sign(&k.key(), msg.as_bytes());
    let mut out = [0u8; 32];
    out.copy_from_slice(tag.as_ref());
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
    const CODE: &str = "K7QP-M2XD-9WRT";
    const CODE_NORMALIZED: &str = "K7QPM2XD9WRT";
    const P_HEX: &str = "ae8f34df1f97c33455555600e278f5e25fa728f6cb4d612e0f4b6befe88cb7cf";
    const PAIR_PI: &str = "a81633f8e74b24df86d3b5064bab2a7cd6705f884f8cf1d3baef7347e1ace662";
    const PAIR_MGR: &str = "d50cde97a5b18afcc5e8221803b93e8dc9163a9ec5bddf3e06aa04436ecbf6f0";
    // v1 pair mode keyed the proofs with K itself; never accepted now.
    const V1_PAIR_MGR: &str = "2f864f5224cad380d328c3b8fa62104855ca1ce1eaaccf5dc12a49535cd2ce1e";
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

    fn p() -> Secret {
        derive_pair_secret(&Secret::from_hex(K_HEX).unwrap(), &PairingCode::parse(CODE).unwrap())
    }

    #[test]
    fn pair_secret_matches_the_vector() {
        let code = PairingCode::parse(CODE).unwrap();
        assert_eq!(code.normalized(), CODE_NORMALIZED);
        assert_eq!(p().to_hex(), P_HEX);
        // Any spelling of the same code gives the same P.
        let k = Secret::from_hex(K_HEX).unwrap();
        let again = derive_pair_secret(&k, &PairingCode::parse("k7qp m2xd 9wrt").unwrap());
        assert_eq!(again.to_hex(), P_HEX);
    }

    #[test]
    fn pair_mode_proofs_match_the_vectors() {
        let p = p();
        assert_eq!(pi_proof(&p, MANAGER_NONCE, PI_NONCE, FP, ID), PAIR_PI);
        assert_eq!(mgr_proof(&p, MANAGER_NONCE, PI_NONCE, FP, ID), PAIR_MGR);
        assert!(verify_mgr_proof(&p, MANAGER_NONCE, PI_NONCE, FP, ID, PAIR_MGR));
        // A v1 Manager's pair-mode proof (keyed by K alone) fails.
        assert!(!verify_mgr_proof(&p, MANAGER_NONCE, PI_NONCE, FP, ID, V1_PAIR_MGR));
    }

    #[test]
    fn wrong_code_or_wrong_password_gives_another_secret() {
        let k = Secret::from_hex(K_HEX).unwrap();
        let wrong_code = derive_pair_secret(&k, &PairingCode::parse("K7QP-M2XD-9WRV").unwrap());
        let wrong_pass = derive_pair_secret(
            &derive_pairing_key(USERNAME, "correct horse battery stapler"),
            &PairingCode::parse(CODE).unwrap(),
        );
        for s in [wrong_code, wrong_pass] {
            assert_ne!(s.to_hex(), P_HEX);
            assert!(!verify_mgr_proof(&s, MANAGER_NONCE, PI_NONCE, FP, ID, PAIR_MGR));
            assert_ne!(pi_proof(&s, MANAGER_NONCE, PI_NONCE, FP, ID), PAIR_PI);
        }
    }

    #[test]
    fn pairing_codes_normalize() {
        let n = |s: &str| PairingCode::parse(s).map(|c| c.normalized().to_string());
        for ok in ["K7QP-M2XD-9WRT", "k7qp-m2xd-9wrt", "K7QPM2XD9WRT", " k7qp m2xd 9wrt ", "K7QP--M2XD - 9WRT"] {
            assert_eq!(n(ok).as_deref(), Ok("K7QPM2XD9WRT"), "{ok:?}");
        }
        // O reads as 0; I and L read as 1.
        assert_eq!(n("OOOO-IIII-LLLL").as_deref(), Ok("000011111111"));
        assert_eq!(n("oooo-iiii-llll").as_deref(), Ok("000011111111"));
        for bad in [
            "", "K7QP-M2XD-9WR", "K7QP-M2XD-9WRTX", "K7QP-M2XD-9WRU", "K7QP_M2XD_9WRT", "K7QP.M2XD.9WRT",
            "K7QP-M2XD-9WR\u{e9}", "K7QP-M2XD-9WR\t", "K7QP:M2XD:9WRT", &"A".repeat(200),
        ] {
            assert_eq!(n(bad), Err(BAD_PAIR_CODE), "{bad:?}");
        }
        assert_eq!(BAD_PAIR_CODE, "That isn't a valid pairing code");
        assert_eq!(PAIR_CODE_ALPHABET.len(), 32);
        assert_eq!(format!("{:?}", PairingCode::parse(CODE).unwrap()), "PairingCode(<redacted>)");
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
        let k = p();
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
