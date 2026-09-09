use anyhow::{Result, bail};
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

type HmacSha256 = Hmac<Sha256>;

/// Builds the exact string that gets HMAC-signed:
///   "<unix_timestamp>\n<project>\n<hex sha256 of body>"
/// Constructing this in one place is the whole point — get this wrong on
/// either the signing or verifying side and every deploy fails with an
/// opaque "signature mismatch," so it's tested directly (see tests below)
/// rather than only exercised indirectly through the HTTP layer.
pub fn signing_string(timestamp: i64, project: &str, body: &[u8]) -> String {
    let body_hash = hex::encode(Sha256::digest(body));
    format!("{timestamp}\n{project}\n{body_hash}")
}

/// Computes the hex-encoded HMAC-SHA256 tag for a signing string, given the
/// project's secret. Used by deploy-ci to produce the X-Deploy-Signature
/// header value (as "sha256=<hex>").
pub fn compute_signature(
    secret: &[u8],
    timestamp: i64,
    project: &str,
    body: &[u8],
) -> Result<String> {
    let msg = signing_string(timestamp, project, body);
    let mut mac = HmacSha256::new_from_slice(secret)?;
    mac.update(msg.as_bytes());
    Ok(hex::encode(mac.finalize().into_bytes()))
}

/// Verifies a provided signature (hex, WITHOUT the "sha256=" prefix — strip
/// that in the caller) against the secret, timestamp, project, and body.
/// Uses `Mac::verify_slice`, which does a constant-time comparison
/// internally — do not replace this with a manual `==` on hex strings or
/// byte arrays, which would reintroduce a timing side channel.
pub fn verify_signature(
    secret: &[u8],
    timestamp: i64,
    project: &str,
    body: &[u8],
    provided_signature_hex: &str,
) -> Result<()> {
    let msg = signing_string(timestamp, project, body);
    let mut mac = HmacSha256::new_from_slice(secret)?;
    mac.update(msg.as_bytes());

    let provided_bytes = match hex::decode(provided_signature_hex) {
        Ok(b) => b,
        Err(_) => bail!("signature is not valid hex"),
    };

    mac.verify_slice(&provided_bytes)
        .map_err(|_| anyhow::anyhow!("signature verification failed"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let secret = b"test-secret-do-not-use-in-prod";
        let body = b"pretend bundle bytes";
        let ts = 1_757_000_000i64;
        let sig = compute_signature(secret, ts, "myproj", body).unwrap();
        assert!(verify_signature(secret, ts, "myproj", body, &sig).is_ok());
    }

    #[test]
    fn rejects_wrong_secret() {
        let body = b"pretend bundle bytes";
        let ts = 1_757_000_000i64;
        let sig = compute_signature(b"secret-a", ts, "myproj", body).unwrap();
        assert!(verify_signature(b"secret-b", ts, "myproj", body, &sig).is_err());
    }

    #[test]
    fn rejects_tampered_body() {
        let secret = b"test-secret-do-not-use-in-prod";
        let ts = 1_757_000_000i64;
        let sig = compute_signature(secret, ts, "myproj", b"original").unwrap();
        assert!(verify_signature(secret, ts, "myproj", b"tampered!", &sig).is_err());
    }

    #[test]
    fn rejects_wrong_project() {
        let secret = b"test-secret-do-not-use-in-prod";
        let body = b"pretend bundle bytes";
        let ts = 1_757_000_000i64;
        let sig = compute_signature(secret, ts, "myproj", body).unwrap();
        assert!(verify_signature(secret, ts, "otherproj", body, &sig).is_err());
    }
}
