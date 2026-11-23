//! CoinDCX authentication / request signing and credential loading.
//!
//! # Narrow API (for qe-desk and the private client)
//! * [`request_timestamp_ms`]: the ONLY source of request timestamps.
//! * [`Credentials`]: key + secret, Debug-redacted, never serialised.
//! * [`load_credentials`]: env (`COINDCX_API_KEY` / `COINDCX_API_SECRET`) or a
//!   0600 local file; returns a [`CredentialStatus`] that says only
//!   configured / not configured (+ a non-secret reason).
//! * [`Credentials::sign_body`]: compact JSON body + its signature, so the bytes
//!   signed are exactly the bytes sent.
//!
//! # Documented scheme (docs.coindcx.com "Authentication", re-read 2026-11-23)
//! * every authenticated REST call carries a JSON body with `timestamp`;
//! * `signature = hex(HMAC-SHA256(key = API secret, msg = JSON body))`;
//!   the python example signs `json.dumps(body, separators=(',', ':'))`;
//! * headers `X-AUTH-APIKEY`, `X-AUTH-SIGNATURE`, `Content-Type: application/json`;
//! * the private socket joins channel `coindcx` with
//!   `authSignature = hex(HMAC-SHA256(secret, '{"channel":"coindcx"}'))`.
//!
//! # Timestamp unit (centralised here)
//! Field tables say "EPOCH timestamp in seconds", but every official example
//! sends milliseconds (`int(round(time.time() * 1000))`, `Math.floor(Date.now())`)
//! and create-order says requests delayed by more than 10 s are rejected. We
//! send MILLISECONDS. This is UNVERIFIED against the live authenticated API
//! until the account owner runs `live_readiness --read-account`, whose wallet
//! fetch either succeeds (unit accepted) or fails with the server's message.
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::path::{Path, PathBuf};

pub const HEADER_API_KEY: &str = "X-AUTH-APIKEY";
pub const HEADER_SIGNATURE: &str = "X-AUTH-SIGNATURE";
pub const ENV_API_KEY: &str = "COINDCX_API_KEY";
pub const ENV_API_SECRET: &str = "COINDCX_API_SECRET";
/// Optional path of a protected local secrets file (KEY=VALUE lines).
pub const ENV_SECRETS_FILE: &str = "QE_SECRETS_FILE";
/// Default local secrets file (gitignored by the repository `.gitignore`).
pub const DEFAULT_SECRETS_FILE: &str = ".env";

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

/// A body ready to send: `body` is the exact byte string that was signed.
pub struct SignedBody {
    pub body: String,
    pub signature: String,
}

impl std::fmt::Debug for SignedBody {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "SignedBody {{ body: {}, signature: <redacted> }}",
            self.body
        )
    }
}

impl Credentials {
    pub fn new(api_key: String, api_secret: String) -> Option<Self> {
        let (k, s) = (api_key.trim().to_string(), api_secret.trim().to_string());
        if k.is_empty() || s.is_empty() {
            return None;
        }
        Some(Self {
            api_key: k,
            api_secret: s,
        })
    }
    pub fn api_key(&self) -> &str {
        &self.api_key
    }
    /// Sign an exact body string.
    pub fn sign(&self, body: &str) -> String {
        sign(self.api_secret.as_bytes(), body)
    }
    /// Serialise compactly and sign the same bytes.
    pub fn sign_body<T: serde::Serialize>(&self, body: &T) -> serde_json::Result<SignedBody> {
        let body = compact_body(body)?;
        let signature = self.sign(&body);
        Ok(SignedBody { body, signature })
    }
}

/// hex(HMAC-SHA256(secret, body)).
pub fn sign(secret: &[u8], body: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret).expect("HMAC accepts any key length");
    mac.update(body.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// Serialise a body compactly; the returned string is both signed and sent.
/// (serde_json emits no whitespace, matching python `separators=(',', ':')`.)
pub fn compact_body<T: serde::Serialize>(body: &T) -> serde_json::Result<String> {
    serde_json::to_string(body)
}

/// Non-secret description of the credential state, safe to log and serve.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct CredentialStatus {
    pub configured: bool,
    /// "env" or "file" when configured.
    pub source: Option<&'static str>,
    /// Why credentials were not loaded (never contains secret material).
    pub problem: Option<String>,
}

/// Load credentials for the backend process: environment first, then the file
/// named by `QE_SECRETS_FILE` (default `.env` in the working directory).
pub fn load_credentials() -> (Option<Credentials>, CredentialStatus) {
    let file = std::env::var(ENV_SECRETS_FILE)
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(DEFAULT_SECRETS_FILE));
    load_credentials_from(|k| std::env::var(k).ok(), Some(&file))
}

/// Testable core of [`load_credentials`].
pub fn load_credentials_from(
    env: impl Fn(&str) -> Option<String>,
    file: Option<&Path>,
) -> (Option<Credentials>, CredentialStatus) {
    let not = |problem: &str| CredentialStatus {
        configured: false,
        source: None,
        problem: Some(problem.to_string()),
    };
    let ok = |source| CredentialStatus {
        configured: true,
        source: Some(source),
        problem: None,
    };
    let (ek, es) = (env(ENV_API_KEY), env(ENV_API_SECRET));
    let set = |v: &Option<String>| v.as_deref().is_some_and(|v| !v.trim().is_empty());
    if set(&ek) || set(&es) {
        return match Credentials::new(ek.unwrap_or_default(), es.unwrap_or_default()) {
            Some(c) => (Some(c), ok("env")),
            None => (
                None,
                not("only one of COINDCX_API_KEY / COINDCX_API_SECRET is set"),
            ),
        };
    }
    let Some(path) = file else {
        return (None, not("no credentials in environment"));
    };
    let Ok(meta) = std::fs::metadata(path) else {
        return (None, not("no credentials in environment or secrets file"));
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o077 != 0 {
            return (
                None,
                not("secrets file permissions too open: run chmod 600 on it"),
            );
        }
    }
    #[cfg(not(unix))]
    let _ = meta;
    let Ok(text) = std::fs::read_to_string(path) else {
        return (None, not("secrets file unreadable"));
    };
    let (mut k, mut s) = (String::new(), String::new());
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        let value = value
            .trim()
            .trim_matches('"')
            .trim_matches('\'')
            .to_string();
        match name.trim().trim_start_matches("export ").trim() {
            ENV_API_KEY => k = value,
            ENV_API_SECRET => s = value,
            _ => {}
        }
    }
    match Credentials::new(k, s) {
        Some(c) => (Some(c), ok("file")),
        None => (None, not("secrets file has no complete key/secret pair")),
    }
}

/// Offline signing self-test against fixed vectors (no network, no secrets).
pub fn signing_self_test() -> Result<(), String> {
    // RFC 4231 test case 2.
    if sign(b"Jefe", "what do ya want for nothing?")
        != "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
    {
        return Err("RFC 4231 HMAC-SHA256 vector mismatch".into());
    }
    // Documented cancel-order body, serialised as python json.dumps(separators=(',',':'))
    // and signed with the docs' placeholder secret "YYYY" (reference computed with
    // python3 hmac/hashlib).
    let body = compact_body(&DocCancel {
        timestamp: 1705565256365,
        id: "c87ca633-6218-44ea-900b-e86981358cbd",
    })
    .map_err(|e| e.to_string())?;
    if body != r#"{"timestamp":1705565256365,"id":"c87ca633-6218-44ea-900b-e86981358cbd"}"# {
        return Err(format!("cancel body serialised as {body}"));
    }
    if sign(b"YYYY", &body) != "40526f5c54ffdacffd8ee8ed43b12cbd25e015d1a33ee2501faf71f60a4266bf" {
        return Err("documented REST body signature mismatch".into());
    }
    // Private socket join payload.
    let sock =
        compact_body(&serde_json::json!({"channel": "coindcx"})).map_err(|e| e.to_string())?;
    if sock != r#"{"channel":"coindcx"}"#
        || sign(b"YYYY", &sock)
            != "c8f94ab0ff0372da5b20dfa616c8a1abd06341fa2c0ea19559decc4c8485b989"
    {
        return Err("socket auth signature mismatch".into());
    }
    Ok(())
}

#[derive(serde::Serialize)]
struct DocCancel<'a> {
    timestamp: i64,
    id: &'a str,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn self_test_vectors() {
        signing_self_test().unwrap();
    }
    #[test]
    fn sign_body_signs_sent_bytes() {
        let c = Credentials::new("k".into(), "s".into()).unwrap();
        let sb = c
            .sign_body(&serde_json::json!({"timestamp": 1, "id": "x"}))
            .unwrap();
        assert!(!sb.body.contains(' '));
        assert_eq!(sb.signature, sign(b"s", &sb.body));
    }
    #[test]
    fn redacted_debug() {
        let c = Credentials::new("KEY123".into(), "SECRET456".into()).unwrap();
        let d = format!("{c:?}");
        assert!(!d.contains("KEY123") && !d.contains("SECRET456"));
        let sb = c.sign_body(&serde_json::json!({"timestamp": 1})).unwrap();
        assert!(!format!("{sb:?}").contains(&sb.signature));
        assert!(Credentials::new(String::new(), "x".into()).is_none());
        assert!(Credentials::new("  ".into(), "x".into()).is_none());
    }
    #[test]
    fn timestamp_is_milliseconds() {
        let t = request_timestamp_ms();
        assert!(t > 1_700_000_000_000 && t < 10_000_000_000_000);
    }
    #[test]
    fn loads_from_env_first() {
        let env = |k: &str| match k {
            ENV_API_KEY => Some("k".to_string()),
            ENV_API_SECRET => Some("s".to_string()),
            _ => None,
        };
        let (c, st) = load_credentials_from(env, None);
        assert!(c.is_some());
        assert_eq!(st.source, Some("env"));
        let half = |k: &str| (k == ENV_API_KEY).then(|| "k".to_string());
        let (c, st) = load_credentials_from(half, None);
        assert!(c.is_none() && !st.configured);
        let (c, st) = load_credentials_from(|_| None, None);
        assert!(c.is_none() && !st.configured);
    }
    #[cfg(unix)]
    #[test]
    fn file_must_be_0600() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("qe-auth-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("secrets.env");
        std::fs::write(
            &p,
            "# comment\nCOINDCX_API_KEY=\"abc\"\nexport COINDCX_API_SECRET=def\n",
        )
        .unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o644)).unwrap();
        let (c, st) = load_credentials_from(|_| None, Some(&p));
        assert!(c.is_none());
        assert!(st.problem.unwrap().contains("chmod 600"));
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600)).unwrap();
        let (c, st) = load_credentials_from(|_| None, Some(&p));
        let c = c.unwrap();
        assert_eq!(c.api_key(), "abc");
        assert_eq!(c.sign("x"), sign(b"def", "x"));
        assert_eq!(st.source, Some("file"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
