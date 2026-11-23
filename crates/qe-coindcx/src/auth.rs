//! CoinDCX authentication / request signing.
//!
//! Documented scheme (docs.coindcx.com, verified 2026-11-23):
//! * every authenticated call is a POST with a JSON body containing `timestamp`;
//! * signature = hex(HMAC-SHA256(key = API secret, msg = exact JSON body string));
//! * headers `X-AUTH-APIKEY` and `X-AUTH-SIGNATURE`, `Content-Type: application/json`;
//! * the body is serialised compactly (no spaces) and the SAME bytes are sent.
//!
//! Timestamp unit: field tables say "EPOCH timestamp in seconds" but every
//! official code example sends milliseconds (`time.time()*1000`, `Date.now()`),
//! and order docs reject requests delayed by more than 10 s. We centralise the
//! unit here as MILLISECONDS (matching the examples); this remains UNVERIFIED
//! against the live authenticated API because no authenticated call is made.
//!
//! Secrets are held in `Credentials`, whose Debug impl redacts them. Nothing here
//! reads environment variables or files; the desk never constructs credentials
//! in this build.
use hmac::{Hmac, Mac};
use sha2::Sha256;

pub const HEADER_API_KEY: &str = "X-AUTH-APIKEY";
pub const HEADER_SIGNATURE: &str = "X-AUTH-SIGNATURE";

/// The single source of request timestamps (milliseconds since Unix epoch).
pub fn request_timestamp_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub struct Credentials {
    api_key: String,
    api_secret: String,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Credentials { api_key: <redacted>, api_secret: <redacted> }")
    }
}

impl Credentials {
    pub fn new(api_key: String, api_secret: String) -> Option<Self> {
        if api_key.is_empty() || api_secret.is_empty() {
            return None;
        }
        Some(Self {
            api_key,
            api_secret,
        })
    }
    pub fn api_key(&self) -> &str {
        &self.api_key
    }
    /// Sign an exact body string.
    pub fn sign(&self, body: &str) -> String {
        sign(self.api_secret.as_bytes(), body)
    }
}

/// hex(HMAC-SHA256(secret, body)).
pub fn sign(secret: &[u8], body: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(body.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Serialise a body compactly; the returned string is both signed and sent.
pub fn compact_body<T: serde::Serialize>(body: &T) -> serde_json::Result<String> {
    serde_json::to_string(body)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rfc4231_case2() {
        // RFC 4231 test case 2: key "Jefe", data "what do ya want for nothing?"
        assert_eq!(
            sign(b"Jefe", "what do ya want for nothing?"),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
    }
    #[test]
    fn compact_and_redacted() {
        let b = serde_json::json!({"timestamp": 1, "leverage": "5", "pair": "B-BTC_USDT"});
        let s = compact_body(&b).unwrap();
        assert!(!s.contains(' '));
        let c = Credentials::new("k".into(), "s".into()).unwrap();
        assert!(!format!("{c:?}").contains('s') || format!("{c:?}").contains("redacted"));
        assert_eq!(c.sign(&s), sign(b"s", &s));
        assert!(Credentials::new(String::new(), "x".into()).is_none());
    }
    #[test]
    fn timestamp_is_milliseconds() {
        let t = request_timestamp_ms();
        assert!(t > 1_700_000_000_000 && t < 10_000_000_000_000);
    }
}
