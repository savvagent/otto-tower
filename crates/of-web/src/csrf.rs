//! The cross-site request guard for cookie-authenticated writes.
//!
//! `SameSite=Lax` keeps the session cookie off cross-*site* POSTs, but says
//! nothing about same-site siblings: `savvagent.com` (the platform) and
//! `tower.savvagent.com` (this service) are the same site, as is every
//! other `*.savvagent.com` host, so a script on any of them can submit a request
//! here with the victim's cookie attached. `__Host-` stops a sibling *setting*
//! our cookie; it does not stop one *using* it.
//!
//! So a request that is not a safe method and **authenticates with the session
//! cookie** must prove where it came from, the same way the platform's console
//! does (`otto-web`'s `csrf.rs`):
//!
//! 1. an `Origin` header equal to the public origin exactly (scheme, host, port) —
//!    a sibling's origin differs, and so does the literal `null` of a sandboxed
//!    frame; otherwise
//! 2. with no `Origin`, `Sec-Fetch-Site: same-origin`; otherwise
//! 3. with neither, a `Referer` whose origin is the public origin; otherwise
//! 4. `403 cross_site_request`.
//!
//! **A bearer request is exempt**, because the credential is not ambient: a
//! forged cross-site request cannot attach an `Authorization` header. The same
//! goes for a request with no session cookie at all (the tracker and platform
//! webhooks): there is no ambient credential to ride. A request carrying both
//! authenticates by its bearer token (see `session::authenticate`), so the
//! cookie does not matter to it.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::Method;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use http::{header, StatusCode};

use crate::cookies;
use crate::error::ApiError;
use crate::session::bearer_token_in;
use crate::state::Config;

/// `scheme://host[:port]` of the public URL, as a browser serializes an `Origin`.
fn origin_of(url: &str) -> Option<String> {
    let url = url::Url::parse(url).ok()?;
    url.has_host().then(|| url.origin().ascii_serialization())
}

/// The allowed origin, as the middleware's state. With no parsable origin
/// nothing can match, which fails closed: every cookie-bearing write is refused.
pub fn allowed_origin(config: &Config) -> Arc<String> {
    Arc::new(origin_of(&config.public_url).unwrap_or_default())
}

pub async fn check(State(origin): State<Arc<String>>, req: Request, next: Next) -> Response {
    if !allowed(&origin, &req) {
        return ApiError::new(
            StatusCode::FORBIDDEN,
            "cross_site_request",
            "this request did not come from this site, so it was refused",
        )
        .into_response();
    }
    next.run(req).await
}

fn allowed(origin: &str, req: &Request) -> bool {
    if matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS) {
        return true;
    }
    let headers = req.headers();
    if bearer_token_in(headers).is_some()
        || cookies::read(headers, cookies::SESSION_COOKIE).is_none()
    {
        return true;
    }
    if origin.is_empty() {
        return false;
    }
    if let Some(value) = headers.get(header::ORIGIN) {
        return value.to_str().is_ok_and(|v| v == origin);
    }
    if let Some(value) = headers.get("sec-fetch-site") {
        return value == "same-origin";
    }
    headers
        .get(header::REFERER)
        .and_then(|v| v.to_str().ok())
        .and_then(origin_of)
        .is_some_and(|o| o == origin)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;

    const ORIGIN: &str = "https://factory.test";
    const COOKIE: (&str, &str) = ("cookie", "theme=dark; __Host-of_session=abc");

    fn req(method: Method, headers: &[(&str, &str)]) -> Request {
        let mut b = Request::builder().method(method).uri("/x");
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        b.body(Body::empty()).unwrap()
    }

    #[test]
    fn the_origin_is_scheme_host_and_port() {
        assert_eq!(origin_of("https://factory.test/").as_deref(), Some(ORIGIN));
        assert_eq!(
            origin_of("http://localhost:8080").as_deref(),
            Some("http://localhost:8080")
        );
        assert_eq!(origin_of("nonsense"), None);
    }

    #[test]
    fn safe_methods_bearers_and_cookieless_requests_pass() {
        for m in [Method::GET, Method::HEAD, Method::OPTIONS] {
            assert!(allowed(
                ORIGIN,
                &req(m, &[COOKIE, ("origin", "https://evil.test")])
            ));
        }
        assert!(allowed(ORIGIN, &req(Method::POST, &[])));
        assert!(allowed(
            ORIGIN,
            &req(
                Method::POST,
                &[
                    COOKIE,
                    ("authorization", "Bearer otto_at_x"),
                    ("origin", "https://evil.test")
                ]
            )
        ));
    }

    #[test]
    fn a_cookie_bearing_write_must_prove_its_origin() {
        let post = |h: &[(&str, &str)]| allowed(ORIGIN, &req(Method::POST, h));
        assert!(post(&[COOKIE, ("origin", ORIGIN)]));
        assert!(!post(&[COOKIE, ("origin", "https://evil.test")]));
        assert!(!post(&[COOKIE, ("origin", "https://blog.factory.test")]));
        assert!(!post(&[COOKIE, ("origin", "null")]));
        assert!(!post(&[COOKIE, ("origin", "http://factory.test")]));
        // No Origin: fetch metadata, then Referer.
        assert!(post(&[COOKIE, ("sec-fetch-site", "same-origin")]));
        assert!(!post(&[COOKIE, ("sec-fetch-site", "same-site")]));
        assert!(!post(&[COOKIE, ("sec-fetch-site", "cross-site")]));
        assert!(post(&[COOKIE, ("referer", "https://factory.test/o/acme")]));
        assert!(!post(&[
            COOKIE,
            ("referer", "https://evil.test/https://factory.test/")
        ]));
        assert!(!post(&[COOKIE]));
        // A wrong Origin is not rescued by a good Referer.
        assert!(!post(&[
            COOKIE,
            ("origin", "https://evil.test"),
            ("referer", "https://factory.test/")
        ]));
    }

    #[test]
    fn an_unparsable_public_url_fails_closed() {
        assert!(!allowed("", &req(Method::POST, &[COOKIE, ("origin", "")])));
    }
}
