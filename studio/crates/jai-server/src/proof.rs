//! The `/health` proof (DESIGN.md section 7): a client sends a random nonce,
//! and the server answers with an HMAC of its port and that nonce under the
//! token. The client checks it before it sends the token anywhere, so a
//! process that took a port named in a stale `server.json` (or another
//! user's server) never sees the token. The token itself never travels.

use std::ops::RangeInclusive;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use rand::rngs::OsRng;
use rand::TryRngCore;
use sha2::{Digest, Sha256};

/// Accepted nonce lengths, in base64url characters.
const NONCE_CHARS: RangeInclusive<usize> = 16..=128;
/// Random bytes in a nonce `nonce` makes.
const NONCE_BYTES: usize = 32;
/// SHA-256's block size, which HMAC pads the key to.
const BLOCK: usize = 64;

/// 16 to 128 characters of the base64url alphabet.
pub(crate) fn valid_nonce(nonce: &str) -> bool {
    NONCE_CHARS.contains(&nonce.len())
        && nonce
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// `base64url(HMAC-SHA256(token, "jai-health:<port>:<nonce>"))`, without
/// padding. The port is the server's own bound port, so an answer relayed
/// from a server on another port does not match.
pub(crate) fn proof(token: &str, port: u16, nonce: &str) -> String {
    let message = format!("jai-health:{port}:{nonce}");
    URL_SAFE_NO_PAD.encode(hmac_sha256(token.as_bytes(), message.as_bytes()))
}

/// A fresh nonce: 32 bytes from the OS RNG, base64url.
pub(crate) fn nonce() -> anyhow::Result<String> {
    let mut bytes = [0u8; NONCE_BYTES];
    OsRng
        .try_fill_bytes(&mut bytes)
        .map_err(|e| anyhow::anyhow!("the OS random number generator failed: {e}"))?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

/// HMAC-SHA256 (RFC 2104).
fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        block[..32].copy_from_slice(&Sha256::digest(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let mut inner = Sha256::new();
    inner.update(block.map(|b| b ^ 0x36));
    inner.update(message);
    let inner = inner.finalize();
    let mut outer = Sha256::new();
    outer.update(block.map(|b| b ^ 0x5c));
    outer.update(inner);
    let mut mac = [0u8; 32];
    mac.copy_from_slice(&outer.finalize());
    mac
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// RFC 4231 test cases 1, 2, 6 and 7.
    #[test]
    fn hmac_test_vectors() {
        assert_eq!(
            hex(&hmac_sha256(&[0x0b; 20], b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        assert_eq!(
            hex(&hmac_sha256(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert_eq!(
            hex(&hmac_sha256(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
        assert_eq!(
            hex(&hmac_sha256(
                &[0xaa; 131],
                b"This is a test using a larger than block-size key and a larger than block-size data. The key needs to be hashed before being used by the HMAC algorithm."
            )),
            "9b09ffa71b942fcb27635fbcd5b0e944bfdc63644f0713938a7f51535c3a35e2"
        );
    }

    #[test]
    fn proofs_depend_on_token_port_and_nonce() {
        let nonce = "abcdefghijklmnop";
        let p = proof("jai_token", 5240, nonce);
        assert_eq!(p.len(), 43);
        assert_eq!(p, proof("jai_token", 5240, nonce));
        assert_ne!(p, proof("jai_other", 5240, nonce));
        assert_ne!(p, proof("jai_token", 5241, nonce));
        assert_ne!(p, proof("jai_token", 5240, "abcdefghijklmnoq"));
    }

    #[test]
    fn nonces() {
        let a = nonce().unwrap();
        assert!(valid_nonce(&a), "{a}");
        assert_eq!(a.len(), 43);
        assert_ne!(a, nonce().unwrap());
        assert!(valid_nonce(&"A-_9".repeat(4)));
        assert!(valid_nonce(&"a".repeat(128)));
        for bad in [
            "short",
            &"a".repeat(129),
            "abcdefghijklmno=",
            "abcdefghijklmn%2",
            "abcdefghijklmnop&x=1",
            "abcdefgh ijklmnop",
        ] {
            assert!(!valid_nonce(bad), "{bad}");
        }
    }
}
