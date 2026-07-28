// RTSP authentication for Sehcontrol ScreenCam (docs/SCREENCAM_PLAN.md, Fase 7).
//
// Credentials are issued by the membership panel, never by this machine: the
// server sends them in `GET /api/client-policy` → `screen_cam.rtsp_user` /
// `screen_cam.rtsp_password`, Dart persists them into `LocalConfig`
// (`UserModel._persistScreenCamPolicy`) and this module reads them back from
// there. Same one-way "the panel decides, the client obeys" flow the rest of
// Fase 4 already uses — there is deliberately no way to set or change them
// from this machine, so a local user can't widen their own access.
//
// Both RFC 2617 schemes are offered on every challenge and either is accepted:
//   - Digest (MD5) — what NVRs/DVRs pick when offered, password never crosses
//     the wire.
//   - Basic — fallback for the handful of cheap NVRs and older VLC builds that
//     only implement this. It sends the password base64'd (i.e. effectively in
//     the clear), which is why it is only ever the *second* choice offered;
//     dropping it entirely would make this incompatible with hardware the
//     operator may already own.
//
// Deliberately NOT covered here (see the Fase 7 notes in the plan):
// the ONVIF SOAP surface in onvif.rs stays unauthenticated for now. It exposes
// device metadata and the RTSP *URL*, but not the video — a LAN attacker who
// queries it still can't watch anything without the credentials below. Adding
// WS-Security UsernameToken there is the natural next step, kept separate so a
// bug in it can't break NVR auto-discovery, which is the whole point of Fase 6.

use std::time::{Duration, Instant};

use hbb_common::{config::LocalConfig, log};

/// Written by Dart from the panel's policy. An empty user means "no
/// credentials configured" — see [`Credentials::is_set`].
pub const USER_OPTION_KEY: &str = "screencam-rtsp-user";
pub const PASS_OPTION_KEY: &str = "screencam-rtsp-pass";

/// Shown to the operator in the NVR's credential prompt, and part of the
/// Digest HA1 hash — changing it invalidates every client's cached digest.
const REALM: &str = "Sehcontrol ScreenCam";

#[derive(Clone, Default, PartialEq, Eq)]
pub struct Credentials {
    pub user: String,
    pub pass: String,
}

impl Credentials {
    /// Auth is keyed off the *username* alone: a configured user with an empty
    /// password is still a real (if terrible) credential the operator chose,
    /// whereas no username at all is what "the panel hasn't sent any yet"
    /// looks like.
    pub fn is_set(&self) -> bool {
        !self.user.is_empty()
    }
}

/// Same cross-process staleness problem (and same fix) as `PolicyCache` in
/// mod.rs: Dart writes these keys from the *UI* process, everything here runs
/// in `--server`, so the in-memory `LocalConfig::get_option` would never
/// observe a credential rotation. Re-read from disk, cached briefly so the
/// per-request path isn't parsing a TOML file every time.
const CACHE_TTL: Duration = Duration::from_secs(2);

struct CredCache {
    fetched_at: Option<Instant>,
    creds: Credentials,
}

lazy_static::lazy_static! {
    static ref CRED_CACHE: std::sync::Mutex<CredCache> = std::sync::Mutex::new(CredCache {
        fetched_at: None,
        creds: Credentials::default(),
    });
}

pub fn credentials() -> Credentials {
    let mut cache = CRED_CACHE.lock().unwrap();
    let stale = match cache.fetched_at {
        None => true,
        Some(t) => t.elapsed() >= CACHE_TTL,
    };
    if !stale {
        return cache.creds.clone();
    }
    let fresh = Credentials {
        user: LocalConfig::get_option_from_file(USER_OPTION_KEY),
        pass: LocalConfig::get_option_from_file(PASS_OPTION_KEY),
    };
    // Log only on an actual change, otherwise this fires every 2s forever.
    if cache.fetched_at.is_some() && fresh != cache.creds {
        if fresh.is_set() {
            log::info!(
                "[screencam] RTSP credentials updated from panel (user '{}')",
                fresh.user
            );
        } else {
            log::warn!(
                "[screencam] RTSP credentials cleared by panel — the stream is now \
                 reachable by anyone on the network"
            );
        }
    }
    cache.creds = fresh.clone();
    cache.fetched_at = Some(Instant::now());
    fresh
}

/// One challenge per RTSP connection. A client that fails or skips auth gets a
/// 401 carrying this nonce and retries on the same connection, which is what
/// every RTSP client does natively; a client that reconnects simply gets a new
/// nonce and repeats the exchange.
pub struct Challenge {
    nonce: String,
}

impl Challenge {
    pub fn new() -> Self {
        Self {
            nonce: format!(
                "{:016x}{:016x}",
                hbb_common::rand::random::<u64>(),
                hbb_common::rand::random::<u64>()
            ),
        }
    }

    /// Both schemes, strongest first — RTSP clients pick the first one they
    /// understand, so Digest must be listed before Basic.
    pub fn www_authenticate_headers(&self) -> [(&'static str, String); 2] {
        [
            (
                "WWW-Authenticate",
                format!("Digest realm=\"{REALM}\", nonce=\"{}\"", self.nonce),
            ),
            ("WWW-Authenticate", format!("Basic realm=\"{REALM}\"")),
        ]
    }

    /// `authorization` is the raw `Authorization:` header value, if the client
    /// sent one. Returns false for anything malformed, unknown or mismatched.
    pub fn verify(&self, creds: &Credentials, method: &str, authorization: Option<&str>) -> bool {
        let Some(header) = authorization else {
            return false;
        };
        let header = header.trim();
        if let Some(rest) = strip_prefix_ci(header, "Digest ") {
            self.verify_digest(creds, method, rest)
        } else if let Some(rest) = strip_prefix_ci(header, "Basic ") {
            verify_basic(creds, rest.trim())
        } else {
            false
        }
    }

    fn verify_digest(&self, creds: &Credentials, method: &str, params: &str) -> bool {
        let params = parse_digest_params(params);
        let get = |k: &str| params.iter().find(|(pk, _)| pk == k).map(|(_, v)| v.as_str());

        if get("username") != Some(creds.user.as_str()) {
            return false;
        }
        // Reject a response computed against some *other* nonce (a replay from
        // an earlier connection), which is the whole reason the nonce exists.
        if get("nonce") != Some(self.nonce.as_str()) {
            return false;
        }
        let Some(response) = get("response") else {
            return false;
        };
        // RFC 2617: HA2 uses the URI exactly as the client echoed it back in
        // the header, not the one from the request line — they legitimately
        // differ (clients often append/strip the track suffix).
        let uri = get("uri").unwrap_or("");

        let ha1 = md5_hex(&format!("{}:{}:{}", creds.user, REALM, creds.pass));
        let ha2 = md5_hex(&format!("{}:{}", method, uri));
        let expected = md5_hex(&format!("{}:{}:{}", ha1, self.nonce, ha2));
        constant_time_eq(expected.as_bytes(), response.as_bytes())
    }
}

fn verify_basic(creds: &Credentials, encoded: &str) -> bool {
    let Ok(raw) = crate::common::decode64(encoded) else {
        return false;
    };
    let Ok(decoded) = String::from_utf8(raw) else {
        return false;
    };
    // Split on the *first* colon only: a password may legitimately contain one.
    let Some((user, pass)) = decoded.split_once(':') else {
        return false;
    };
    constant_time_eq(user.as_bytes(), creds.user.as_bytes())
        & constant_time_eq(pass.as_bytes(), creds.pass.as_bytes())
}

/// `key=value` / `key="value"` pairs separated by commas, as they appear in an
/// `Authorization: Digest ...` header. Hand-rolled for the same reason the
/// rest of this module hand-rolls its parsing: the values here are simple and
/// pulling an HTTP header crate in for one line isn't worth it. Quoted values
/// containing commas are handled; escaped quotes inside them are not (no RTSP
/// client emits those for these fields).
fn parse_digest_params(input: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let bytes = input.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i] == b' ' || bytes[i] == b',') {
            i += 1;
        }
        let key_start = i;
        while i < bytes.len() && bytes[i] != b'=' && bytes[i] != b',' {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'=' {
            break;
        }
        let key = input[key_start..i].trim().to_ascii_lowercase();
        i += 1; // skip '='
        let value = if i < bytes.len() && bytes[i] == b'"' {
            i += 1;
            let start = i;
            while i < bytes.len() && bytes[i] != b'"' {
                i += 1;
            }
            let v = input[start..i.min(bytes.len())].to_owned();
            if i < bytes.len() {
                i += 1; // closing quote
            }
            v
        } else {
            let start = i;
            while i < bytes.len() && bytes[i] != b',' {
                i += 1;
            }
            input[start..i].trim().to_owned()
        };
        out.push((key, value));
    }
    out
}

fn md5_hex(data: &str) -> String {
    use md5::{Digest, Md5};
    let mut hasher = Md5::new();
    hasher.update(data.as_bytes());
    hex::encode(hasher.finalize())
}

/// Compares without an early return, so a wrong password can't be recovered
/// byte-by-byte by timing repeated attempts. Length is allowed to leak (it
/// already does, via the encoded header's size).
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn strip_prefix_ci<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    if s.len() >= prefix.len() && s[..prefix.len()].eq_ignore_ascii_case(prefix) {
        Some(&s[prefix.len()..])
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn md5_matches_rfc1321_vectors() {
        assert_eq!(md5_hex(""), "d41d8cd98f00b204e9800998ecf8427e");
        assert_eq!(md5_hex("abc"), "900150983cd24fb0d6963f7d28e17f72");
    }

    #[test]
    fn digest_roundtrip_accepts_correct_and_rejects_wrong_password() {
        let creds = Credentials {
            user: "cam".to_owned(),
            pass: "s3cret".to_owned(),
        };
        let challenge = Challenge::new();
        let nonce = challenge.nonce.clone();
        let uri = "rtsp://host:8554/live/main";

        let ha1 = md5_hex(&format!("cam:{}:s3cret", REALM));
        let ha2 = md5_hex(&format!("DESCRIBE:{uri}"));
        let response = md5_hex(&format!("{ha1}:{nonce}:{ha2}"));
        let header = format!(
            "Digest username=\"cam\", realm=\"{REALM}\", nonce=\"{nonce}\", uri=\"{uri}\", response=\"{response}\""
        );
        assert!(challenge.verify(&creds, "DESCRIBE", Some(&header)));

        // Same request, wrong password on the server side must not verify.
        let other = Credentials {
            user: "cam".to_owned(),
            pass: "wrong".to_owned(),
        };
        assert!(!challenge.verify(&other, "DESCRIBE", Some(&header)));
        // ...and the digest is bound to the method, so it can't be replayed
        // onto a different one.
        assert!(!challenge.verify(&creds, "PLAY", Some(&header)));
    }

    #[test]
    fn digest_rejects_foreign_nonce() {
        let creds = Credentials {
            user: "cam".to_owned(),
            pass: "s3cret".to_owned(),
        };
        let challenge = Challenge::new();
        let uri = "rtsp://host:8554/live/main";
        let stale_nonce = "deadbeefdeadbeefdeadbeefdeadbeef";
        let ha1 = md5_hex(&format!("cam:{}:s3cret", REALM));
        let ha2 = md5_hex(&format!("DESCRIBE:{uri}"));
        let response = md5_hex(&format!("{ha1}:{stale_nonce}:{ha2}"));
        let header = format!(
            "Digest username=\"cam\", nonce=\"{stale_nonce}\", uri=\"{uri}\", response=\"{response}\""
        );
        assert!(!challenge.verify(&creds, "DESCRIBE", Some(&header)));
    }

    #[test]
    fn basic_roundtrip() {
        let creds = Credentials {
            user: "cam".to_owned(),
            pass: "p:ss".to_owned(), // colon in password must survive the split
        };
        let challenge = Challenge::new();
        let encoded = crate::common::encode64("cam:p:ss");
        assert!(challenge.verify(&creds, "DESCRIBE", Some(&format!("Basic {encoded}"))));

        let wrong = crate::common::encode64("cam:nope");
        assert!(!challenge.verify(&creds, "DESCRIBE", Some(&format!("Basic {wrong}"))));
    }

    #[test]
    fn missing_or_unknown_scheme_is_rejected() {
        let creds = Credentials {
            user: "cam".to_owned(),
            pass: "x".to_owned(),
        };
        let challenge = Challenge::new();
        assert!(!challenge.verify(&creds, "DESCRIBE", None));
        assert!(!challenge.verify(&creds, "DESCRIBE", Some("Bearer sometoken")));
        assert!(!challenge.verify(&creds, "DESCRIBE", Some("Digest ")));
    }

    #[test]
    fn parses_unquoted_and_quoted_params() {
        let parsed = parse_digest_params("username=\"cam\", nc=00000001, uri=\"a,b\"");
        let get = |k: &str| {
            parsed
                .iter()
                .find(|(pk, _)| pk == k)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(get("username"), Some("cam"));
        assert_eq!(get("nc"), Some("00000001"));
        assert_eq!(get("uri"), Some("a,b"));
    }
}
