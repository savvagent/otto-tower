//! The console's two cookies, and what rides in them.
//!
//! - **`__Host-of_session`** — the login session: 32 random bytes, base64url,
//!   and nothing else. It is a lookup key and carries no claims; the row it keys
//!   (`tower_core::console_sessions`) holds the platform token pair. Only its SHA-256
//!   is stored.
//! - **`__Host-of_oauth`** — the in-flight login: the `state`, the PKCE
//!   verifier, and where to go afterwards, sealed with the deployment's
//!   encryption key. It lives ten minutes and is cleared by the callback. It is
//!   what makes the callback a response to *this browser's* request (login CSRF)
//!   and what keeps the PKCE verifier off the URL.
//!
//! Both are `HttpOnly; Secure; SameSite=Lax; Path=/` with no `Domain`, which is
//! what the `__Host-` prefix makes a browser enforce: a sibling subdomain
//! (`savvagent.com` is one) cannot plant them. `Secure` is set
//! unconditionally, as the platform does for its own cookie: browsers treat
//! `http://localhost` as a secure context and store `Secure` cookies there, so
//! local development needs no exemption and a deployment cannot forget one.
//! `Lax` rather than `Strict` because the callback is a cross-site top-level
//! navigation (back from the platform) that has to carry the oauth cookie.

use axum::http::{header, HeaderMap, HeaderValue};
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine;
use rand::RngCore;
use savvagent_tenant::crypto::Cipher;
use serde::{Deserialize, Serialize};

pub const SESSION_COOKIE: &str = "__Host-of_session";
pub const OAUTH_COOKIE: &str = "__Host-of_oauth";

/// How long a login may take, from `/auth/login` to `/auth/callback`.
pub const OAUTH_MAX_AGE_SECS: i64 = 600;

/// Longest `next` path accepted.
const MAX_NEXT_LEN: usize = 512;

/// A random 32-byte value, base64url (43 characters).
pub fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// The value of cookie `name`, matched on the exact name: a cookie called
/// `evil-__Host-of_session` (which a sibling host can set) is not ours.
pub fn read<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|raw| raw.split(';'))
        .filter_map(|pair| pair.split_once('='))
        .find(|(n, _)| n.trim() == name)
        .map(|(_, v)| v.trim())
}

/// The session cookie, if present and shaped like one we mint. Anything else is
/// never looked up.
pub fn session_cookie(headers: &HeaderMap) -> Option<&str> {
    read(headers, SESSION_COOKIE).filter(|v| {
        v.len() == 43
            && v.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    })
}

pub fn set(name: &str, value: &str, max_age_secs: i64) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{name}={value}; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age={max_age_secs}"
    ))
    .expect("cookie values are base64url by construction")
}

/// A `Set-Cookie` that makes the browser drop `name`. Same attributes as when it
/// was set: a clear that differs in `Path` or `__Host-` requirements is ignored
/// and the stale cookie keeps being sent.
pub fn clear(name: &str) -> HeaderValue {
    HeaderValue::from_str(&format!(
        "{name}=; Path=/; HttpOnly; Secure; SameSite=Lax; Max-Age=0"
    ))
    .expect("static cookie")
}

/// What `/auth/login` remembers for `/auth/callback`.
#[derive(Serialize, Deserialize)]
pub struct OauthFlow {
    pub state: String,
    pub verifier: String,
    /// A same-origin path to land on afterwards, already checked by [`safe_next`].
    pub next: Option<String>,
    /// The org slug the person asked to sign in to.
    pub org: Option<String>,
    /// Unix seconds. The cookie's `Max-Age` is advisory; this is enforced.
    pub exp: i64,
}

impl OauthFlow {
    pub fn seal(&self, cipher: &Cipher) -> Option<String> {
        let json = serde_json::to_vec(self).ok()?;
        let sealed = cipher.seal(&json).ok()?;
        let mut combined = sealed.nonce;
        combined.extend_from_slice(&sealed.ciphertext);
        Some(URL_SAFE_NO_PAD.encode(combined))
    }

    /// `None` for anything that is not an unexpired flow we sealed.
    pub fn open(cipher: &Cipher, value: &str, now: i64) -> Option<Self> {
        let combined = URL_SAFE_NO_PAD
            .decode(value)
            .or_else(|_| STANDARD.decode(value))
            .ok()?;
        if combined.len() < 12 {
            return None;
        }
        let (nonce, ciphertext) = combined.split_at(12);
        let json = cipher.open(ciphertext, nonce).ok()?;
        let flow: Self = serde_json::from_slice(&json).ok()?;
        (flow.exp > now).then_some(flow)
    }
}

// Not derived: the verifier is a secret until the code is redeemed.
impl std::fmt::Debug for OauthFlow {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OauthFlow")
            .field("next", &self.next)
            .field("org", &self.org)
            .finish_non_exhaustive()
    }
}

/// `next` if it is a same-origin path we are willing to redirect to, else `None`.
///
/// A redirect target that arrives in a URL is the classic open redirect, so this
/// accepts only what is certainly a path on this origin: it starts with a single
/// `/`, so no scheme and no authority (`//evil.test`, `https://…`); it has no `\`
/// (browsers read `/\evil.test` as `//evil.test`); and it has no control or
/// whitespace characters (browsers strip tabs and newlines from URLs, so
/// `/\t/evil.test` is `//evil.test` to them). Paths into `/auth` are refused too,
/// so a login cannot be made to bounce back into the login routes.
pub fn safe_next(next: &str) -> Option<String> {
    let ok = next.starts_with('/')
        && !next.starts_with("//")
        && next.len() <= MAX_NEXT_LEN
        && !next
            .chars()
            .any(|c| c == '\\' || c.is_control() || c.is_whitespace())
        && next != "/auth"
        && !next.starts_with("/auth/")
        && !next.starts_with("/auth?");
    ok.then(|| next.to_string())
}

/// An org slug as the platform issues them, for use in `org_hint`.
pub fn is_org_slug(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 64
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(cookie: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::COOKIE, HeaderValue::from_str(cookie).unwrap());
        h
    }

    #[test]
    fn a_cookie_is_found_by_exact_name_among_others() {
        let h = headers("theme=dark; __Host-of_session=abc; x=1");
        assert_eq!(read(&h, SESSION_COOKIE), Some("abc"));
        let h = headers("evil-__Host-of_session=abc");
        assert_eq!(read(&h, SESSION_COOKIE), None);
    }

    #[test]
    fn only_a_well_formed_session_cookie_is_ever_looked_up() {
        let good = random_token();
        assert_eq!(good.len(), 43);
        let h = headers(&format!("{SESSION_COOKIE}={good}"));
        assert_eq!(session_cookie(&h), Some(good.as_str()));
        for bad in ["short", "a'b", &"x".repeat(44)] {
            let h = headers(&format!("{SESSION_COOKIE}={bad}"));
            assert_eq!(session_cookie(&h), None, "{bad}");
        }
    }

    #[test]
    fn safe_next_accepts_paths_and_refuses_everything_else() {
        for ok in ["/", "/o/acme", "/o/acme/queue?status=pending#x"] {
            assert_eq!(safe_next(ok).as_deref(), Some(ok));
        }
        for bad in [
            "",
            "o/acme",
            "//evil.test",
            "/\\evil.test",
            "https://evil.test",
            "javascript:alert(1)",
            "/\t/evil.test",
            "/o/acme\r\nSet-Cookie: x=1",
            "/ spaced",
            "/auth/login",
            "/auth",
            &format!("/{}", "a".repeat(600)),
        ] {
            assert_eq!(safe_next(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn a_flow_survives_the_cookie_and_expires() {
        let cipher = Cipher::from_base64_key(&STANDARD.encode([5u8; 32])).expect("test key");
        let flow = OauthFlow {
            state: "s".into(),
            verifier: "v".into(),
            next: Some("/o/acme".into()),
            org: Some("acme".into()),
            exp: 1_000,
        };
        let sealed = flow.seal(&cipher).unwrap();
        let back = OauthFlow::open(&cipher, &sealed, 999).unwrap();
        assert_eq!(back.state, "s");
        assert_eq!(back.next.as_deref(), Some("/o/acme"));
        assert!(
            OauthFlow::open(&cipher, &sealed, 1_000).is_none(),
            "expired"
        );
        assert!(OauthFlow::open(&cipher, "garbage", 0).is_none());
        let other = Cipher::from_base64_key(&STANDARD.encode([6u8; 32])).unwrap();
        assert!(OauthFlow::open(&other, &sealed, 0).is_none(), "wrong key");
    }
}
