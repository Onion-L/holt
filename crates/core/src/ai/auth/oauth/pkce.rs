//! Port of `pi-core/ai/src/auth/oauth/pkce.ts`: PKCE utilities.

use sha2::{Digest, Sha256};

/// Port of `generatePKCE`: a random 32-byte base64url verifier plus its
/// SHA-256 base64url challenge. The TypeScript version is async for Web
/// Crypto; the Rust port is synchronous.
pub fn generate_pkce() -> PkcePair {
    let mut verifier_bytes = [0u8; 32];
    getrandom::fill(&mut verifier_bytes).expect("system RNG is always available");
    let verifier = base64url_encode(&verifier_bytes);

    let challenge = base64url_encode(&Sha256::digest(verifier.as_bytes()));
    PkcePair {
        verifier,
        challenge,
    }
}

/// The `generatePKCE` result.
pub struct PkcePair {
    pub verifier: String,
    pub challenge: String,
}

/// Base64url encoding without padding, matching the TypeScript
/// `btoa(...).replace(/\+/g, "-").replace(/\//g, "_").replace(/=/g, "")`.
fn base64url_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let mut buffer = [0u8; 3];
        buffer[..chunk.len()].copy_from_slice(chunk);
        let word = ((buffer[0] as u32) << 16) | ((buffer[1] as u32) << 8) | buffer[2] as u32;
        out.push(ALPHABET[(word >> 18) as usize & 0x3f] as char);
        out.push(ALPHABET[(word >> 12) as usize & 0x3f] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(word >> 6) as usize & 0x3f] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[word as usize & 0x3f] as char);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_the_rfc_7636_appendix_b_vector() {
        // RFC 7636 Appendix B: this verifier's S256 challenge is well-known.
        let verifier = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
        assert_eq!(
            base64url_encode(&Sha256::digest(verifier.as_bytes())),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn generates_urlsafe_verifier_and_matching_challenge() {
        let pkce = generate_pkce();
        // 32 bytes -> 43 base64url characters, no padding or +/ characters.
        assert_eq!(pkce.verifier.len(), 43);
        assert!(!pkce.verifier.contains(['+', '/', '=']));

        assert_eq!(
            pkce.challenge,
            base64url_encode(&Sha256::digest(pkce.verifier.as_bytes()))
        );
        assert_ne!(pkce.verifier, pkce.challenge);
    }
}
