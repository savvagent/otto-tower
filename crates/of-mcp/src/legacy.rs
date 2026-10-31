//! Hosts this deployment still answers on but no longer wants to be called by.
//!
//! When the product moved from `otto-factory.savvagent.com` to
//! `tower.savvagent.com`, the old URL had to keep working until a published
//! sunset date, and every response through it had to say so. This module is the
//! whole of that: it decides whether a request arrived through a deprecated
//! host, stamps the response with the notice, and hands the rest of the MCP
//! surface a [`Legacy`] marker so discovery and `initialize` can say the same
//! thing in their own vocabulary.
//!
//! ## The host only selects; it never authorizes
//!
//! The origin host (`X-Forwarded-Host`, else `Host`) is attacker-controlled, so
//! it is used for exactly one thing: choosing among values an operator already
//! configured. It picks which configured legacy host's URLs a discovery document
//! names, and whether the notice is attached. It never reaches token
//! validation: `require_bearer` asks the platform about a token as
//! `OF_RESOURCE_URI` and nothing else, whichever host the request came through,
//! and a test in `of-server` pins that. A caller who forges `X-Forwarded-Host`
//! to name a legacy host gains a deprecation notice and a discovery document
//! naming a URL the operator chose — nothing it could not have read by calling
//! that URL directly.
//!
//! ## Why `X-Forwarded-Host`
//!
//! The production console is a Cloudflare Worker that proxies to the Fly app,
//! and a Worker cannot choose the `Host` of its subrequest: the origin sees its
//! own `fly.dev` name whichever public host the browser or agent used. The
//! Worker therefore *sets* `X-Forwarded-Host` from the request URL, overwriting
//! anything the client sent (`web/worker/index.ts`), and that is the only place
//! the public host survives the hop. A deployment with no proxy has no such
//! header, and `Host` is then the public host itself.
//!
//! ## Why it lives in this crate
//!
//! The headers go on every response, console and webhooks included, so
//! `of-server` applies [`mark`] as its outermost layer. But the only consumers
//! of the [`Legacy`] marker are here — the protected-resource document, the
//! `401` challenge and `initialize` — so the type and the one function that
//! makes it sit beside them.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use chrono::{DateTime, SecondsFormat, Utc};
use http::{HeaderMap, HeaderValue, Uri};

/// The deprecated hosts and what to tell their callers. Built once from
/// configuration; empty (no legacy host) is the normal state outside a rename.
///
/// Everything a legacy-host response needs is worked out here, once per host,
/// so marking a request is a lookup and an `Arc` clone rather than a round of
/// string formatting.
#[derive(Debug, Clone, Default)]
pub struct Deprecation {
    hosts: Vec<Legacy>,
}

impl Deprecation {
    /// `hosts` must already be validated bare hostnames; `of-server`'s config
    /// refuses to start otherwise. They are lowercased here so that matching
    /// cannot depend on how an operator typed them.
    ///
    /// Each legacy host is served the way the canonical one is: the public
    /// URL's scheme and port, and the resource URI's path. A deployment whose
    /// MCP endpoint is not at `/mcp` therefore advertises the same path on the
    /// old host as on the new.
    pub fn new(
        public_url: &url::Url,
        resource_uri: &url::Url,
        hosts: impl IntoIterator<Item = impl Into<String>>,
        sunset: Option<DateTime<Utc>>,
    ) -> Self {
        let successor_origin = public_url.origin().ascii_serialization();
        let port = public_url
            .port()
            .map(|p| format!(":{p}"))
            .unwrap_or_default();
        let resource_path = resource_uri.path().trim_end_matches('/');
        let hosts = hosts
            .into_iter()
            .map(|h| {
                let host = h.into().to_ascii_lowercase();
                let origin = format!("{}://{host}{port}", public_url.scheme());
                Legacy(Arc::new(LegacyHost {
                    resource_uri: format!("{origin}{resource_path}"),
                    metadata_url: format!("{origin}/.well-known/oauth-protected-resource"),
                    host,
                    successor_origin: successor_origin.clone(),
                    successor_mcp: resource_uri.to_string(),
                    sunset,
                }))
            })
            .collect();
        Self { hosts }
    }

    /// Whether this request's origin host is one of the deprecated ones, and if
    /// so which. The returned marker carries the *configured* entry, never the
    /// header's spelling, so nothing a caller typed ends up in a response.
    pub fn classify(&self, headers: &HeaderMap, uri: &Uri) -> Option<Legacy> {
        if self.hosts.is_empty() {
            return None;
        }
        let origin = origin_host(headers, uri)?;
        self.hosts.iter().find(|l| l.0.host == origin).cloned()
    }
}

/// A request that arrived through a deprecated host. Placed in the request's
/// extensions by [`mark`]; the MCP transport carries it into `initialize` as
/// part of `http::request::Parts`. Cheap to clone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Legacy(Arc<LegacyHost>);

#[derive(Debug, PartialEq, Eq)]
struct LegacyHost {
    host: String,
    resource_uri: String,
    metadata_url: String,
    successor_origin: String,
    successor_mcp: String,
    sunset: Option<DateTime<Utc>>,
}

impl Legacy {
    /// The configured legacy hostname the request came through.
    pub fn host(&self) -> &str {
        &self.0.host
    }

    /// The resource URI a client that started on this host is talking to.
    ///
    /// RFC 9728 §3.3 makes a client check that the metadata's `resource` is the
    /// URL it was given; a fresh OAuth flow started on the old URL would fail
    /// that check if the document named the canonical one. The platform keeps
    /// this URI as an alias of the canonical resource until the sunset, so a
    /// token requested for it is minted for, and introspects as, this service.
    pub fn resource_uri(&self) -> String {
        self.0.resource_uri.clone()
    }

    /// The discovery pointer a `401` on this host carries.
    pub fn metadata_url(&self) -> String {
        self.0.metadata_url.clone()
    }

    /// The same resource on the canonical host: `OF_PUBLIC_URL`'s origin
    /// followed by `target`. This is both the `successor-version` link and
    /// where [`Self::redirect`] sends a legacy-host console request, so the two
    /// can never disagree.
    ///
    /// A `target` that does not start with `/` becomes `/`: appended to an
    /// origin, `@evil.com` would turn the origin into userinfo and the URL into
    /// one for another host. A request can carry such a target (HTTP/2's
    /// `:path`, or `OPTIONS *`), so this never trusts its caller to have
    /// checked.
    pub fn successor_url(&self, target: &str) -> String {
        let target = if target.starts_with('/') { target } else { "/" };
        format!("{}{target}", self.0.successor_origin)
    }

    /// `308` to the same path and query on the canonical host, with `Vary` (the
    /// answer depends on the host) and `no-store`. [`mark`] stamps it like
    /// every other legacy-host response, with the query kept in the `Link` so
    /// the link and `Location` name one URL.
    pub fn redirect(&self, uri: &Uri) -> Response {
        let location = HeaderValue::from_str(&self.successor_url(request_target(uri)))
            .or_else(|_| HeaderValue::from_str(&self.successor_url("/")));
        let mut response = http::StatusCode::PERMANENT_REDIRECT.into_response();
        let headers = response.headers_mut();
        if let Ok(location) = location {
            headers.insert(http::header::LOCATION, location);
        }
        headers.insert(http::header::VARY, HeaderValue::from_static(VARY_BY_HOST));
        headers.insert(
            http::header::CACHE_CONTROL,
            HeaderValue::from_static("no-store"),
        );
        response.extensions_mut().insert(Moved);
        response
    }

    /// The paragraph `initialize` instructions open with on this host. Agents
    /// read instructions, not response headers, so this is how the notice
    /// reaches the reader who can act on it.
    pub fn notice(&self) -> String {
        self.notice_at(Utc::now())
    }

    /// [`Self::notice`] as of `now`. Past the sunset the old address is still
    /// answering (the operator has not removed it yet), and saying it "keeps
    /// working until" a date already gone would be false.
    pub fn notice_at(&self, now: DateTime<Utc>) -> String {
        let when = match self.0.sunset {
            Some(sunset) if sunset <= now => format!(
                "It was retired on {} and may stop answering at any moment.",
                sunset.to_rfc3339_opts(SecondsFormat::Secs, true)
            ),
            Some(sunset) => format!(
                "It keeps working until {}, and stops working after that.",
                sunset.to_rfc3339_opts(SecondsFormat::Secs, true)
            ),
            None => "It keeps working for now, but it will be retired; the shutdown date \
                     has not been published yet."
                .to_string(),
        };
        format!(
            "DEPRECATED ADDRESS. You reached Savvagent Tower through {old}, its old address. \
             {when} Change this MCP server's URL in your client configuration to {new} \
             and sign in again if your client asks; your jobs, repos and leases are the \
             same on both addresses. Tell the person you are working for, because \
             changing the configuration may be theirs to do.",
            old = self.0.resource_uri,
            new = self.0.successor_mcp,
        )
    }

    /// `Deprecation: true`, a `successor-version` `Link` (RFC 8288) and, with a
    /// sunset date, `Sunset` (RFC 8594). `true` rather than RFC 9745's
    /// `@<timestamp>`: the host was deprecated by an operator's cutover, not on
    /// a date this binary knows. The successor link keeps `target` — the
    /// request's path, and its query too when the response is a redirect — so a
    /// client following it lands on the same resource on the canonical host.
    /// It is appended, so a `Link` a handler already set is kept beside it.
    fn stamp(&self, target: &str, headers: &mut HeaderMap) {
        headers.insert("deprecation", HeaderValue::from_static("true"));

        let origin = &self.0.successor_origin;
        let link = HeaderValue::from_str(&format!(
            "<{}>; rel=\"successor-version\"",
            escape_for_link(&self.successor_url(target))
        ))
        .or_else(|_| HeaderValue::from_str(&format!("<{origin}/>; rel=\"successor-version\"")));
        if let Ok(link) = link {
            headers.append(http::header::LINK, link);
        }

        if let Some(sunset) = self.0.sunset {
            if let Ok(value) = HeaderValue::from_str(&http_date(sunset)) {
                headers.insert("sunset", value);
            }
        }
    }
}

/// The outermost layer of `of-server`'s router: mark a legacy-host request and
/// stamp its response, whichever surface answers it.
pub async fn mark(
    State(deprecation): State<Arc<Deprecation>>,
    mut req: Request,
    next: Next,
) -> Response {
    let Some(legacy) = deprecation.classify(req.headers(), req.uri()) else {
        return next.run(req).await;
    };
    // The query goes into the `Link` only on a redirect, where it is already in
    // `Location`. On a response served in place it would copy whatever the
    // query carried — `/auth/callback?code=…&state=…` — into a header that
    // proxies and logs keep with far less care than they keep the URL.
    let path = request_path(req.uri()).to_string();
    let path_and_query = request_target(req.uri()).to_string();
    req.extensions_mut().insert(legacy.clone());
    let mut response = next.run(req).await;
    let moved = response.extensions().get::<Moved>().is_some();
    let target = if moved { &path_and_query } else { &path };
    legacy.stamp(target, response.headers_mut());
    response
}

/// Left on a response by [`Legacy::redirect`], so [`mark`] knows the query is
/// already public in `Location` and may be repeated in the `Link`.
#[derive(Debug, Clone, Copy)]
struct Moved;

/// The request's path and query, or `/` when it is not origin-form (an
/// HTTP/2 `:path` that does not start with `/`, or `OPTIONS *`).
pub fn request_target(uri: &Uri) -> &str {
    uri.path_and_query()
        .map(|pq| pq.as_str())
        .filter(|pq| pq.starts_with('/'))
        .unwrap_or("/")
}

/// [`request_target`] without the query.
fn request_path(uri: &Uri) -> &str {
    Some(uri.path())
        .filter(|p| p.starts_with('/'))
        .unwrap_or("/")
}

/// The value of `Vary` for a response that depends on which host was used:
/// discovery and the `401` challenge name a different URL on a legacy host.
pub const VARY_BY_HOST: &str = "X-Forwarded-Host, Host";

/// The public host a request was addressed to: `X-Forwarded-Host`, else
/// `Host`, else the request target's authority. Lowercase, without a port.
///
/// `X-Forwarded-Host` may be a list when several proxies append to it; the
/// first entry is the one the client used. One that is not valid text falls
/// through to `Host` rather than failing: the result only ever selects among
/// configured hosts, so the worst outcome is a request treated as canonical.
pub fn origin_host(headers: &HeaderMap, uri: &Uri) -> Option<String> {
    let from_header = |name| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.split(',').next())
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    let authority = from_header("x-forwarded-host")
        .or_else(|| from_header(http::header::HOST.as_str()))
        .or_else(|| uri.authority().map(|a| a.as_str()))?;
    Some(strip_port(authority).to_ascii_lowercase())
}

fn strip_port(authority: &str) -> &str {
    if let Some(rest) = authority.strip_prefix('[') {
        // An IPv6 literal keeps its brackets so it can never equal a hostname.
        return rest
            .find(']')
            .map_or(authority, |end| &authority[..end + 2]);
    }
    match authority.rsplit_once(':') {
        Some((host, port)) if port.bytes().all(|b| b.is_ascii_digit()) => host,
        _ => authority,
    }
}

/// `<` and `>` would end the link target early; nothing else a request path or
/// query can legally hold is a problem inside one.
fn escape_for_link(path: &str) -> String {
    path.replace('<', "%3C").replace('>', "%3E")
}

/// An IMF-fixdate (RFC 9110 §5.6.7), the format `Sunset` requires.
pub fn http_date(at: DateTime<Utc>) -> String {
    at.format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn url(s: &str) -> url::Url {
        url::Url::parse(s).unwrap()
    }

    fn deprecation(sunset: Option<DateTime<Utc>>) -> Deprecation {
        Deprecation::new(
            &url("https://tower.example.com"),
            &url("https://tower.example.com/mcp"),
            ["Factory.Example.com"],
            sunset,
        )
    }

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    fn uri() -> Uri {
        "/mcp".parse().unwrap()
    }

    #[test]
    fn the_forwarded_host_wins_over_host() {
        let h = headers(&[
            ("host", "otto-factory-mcp.fly.dev"),
            ("x-forwarded-host", "factory.example.com"),
        ]);
        assert_eq!(
            origin_host(&h, &uri()).as_deref(),
            Some("factory.example.com")
        );
        assert!(deprecation(None).classify(&h, &uri()).is_some());

        // And the other way round: a canonical forwarded host is canonical even
        // when Host happens to name a legacy one.
        let h = headers(&[
            ("host", "factory.example.com"),
            ("x-forwarded-host", "tower.example.com"),
        ]);
        assert!(deprecation(None).classify(&h, &uri()).is_none());
    }

    #[test]
    fn host_is_used_when_nothing_was_forwarded() {
        let h = headers(&[("host", "factory.example.com:8443")]);
        assert!(deprecation(None).classify(&h, &uri()).is_some());
        let h = headers(&[("host", "tower.example.com")]);
        assert!(deprecation(None).classify(&h, &uri()).is_none());
    }

    #[test]
    fn matching_ignores_case_and_port_but_not_suffixes() {
        for legacy in [
            "FACTORY.example.com",
            "factory.example.com:443",
            "factory.example.com, proxy.internal",
        ] {
            let h = headers(&[("x-forwarded-host", legacy)]);
            assert!(deprecation(None).classify(&h, &uri()).is_some(), "{legacy}");
        }
        for other in [
            "evil-factory.example.com",
            "factory.example.com.evil",
            "[::1]:443",
            "",
        ] {
            let h = headers(&[("host", "tower.example.com"), ("x-forwarded-host", other)]);
            assert!(deprecation(None).classify(&h, &uri()).is_none(), "{other}");
        }
    }

    /// The marker names the configured host, never the header's spelling, so
    /// nothing a caller sent is echoed into a discovery document.
    #[test]
    fn the_marker_carries_the_configured_spelling() {
        let h = headers(&[("x-forwarded-host", "FACTORY.EXAMPLE.COM:8443")]);
        let legacy = deprecation(None).classify(&h, &uri()).expect("legacy");
        assert_eq!(legacy.resource_uri(), "https://factory.example.com/mcp");
        assert_eq!(
            legacy.metadata_url(),
            "https://factory.example.com/.well-known/oauth-protected-resource"
        );
    }

    #[test]
    fn no_configured_host_means_nothing_is_legacy() {
        let none = Deprecation::new(
            &url("https://tower.example.com"),
            &url("https://tower.example.com/mcp"),
            Vec::<String>::new(),
            None,
        );
        let h = headers(&[("x-forwarded-host", "factory.example.com")]);
        assert!(none.classify(&h, &uri()).is_none());
    }

    #[test]
    fn the_stamp_names_the_successor_and_the_sunset_only_when_set() {
        let sunset = Utc.with_ymd_and_hms(2027, 1, 31, 0, 0, 0).unwrap();
        let h = headers(&[("x-forwarded-host", "factory.example.com")]);

        let mut out = HeaderMap::new();
        out.insert(http::header::LINK, "</next>; rel=\"next\"".parse().unwrap());
        let legacy = deprecation(Some(sunset)).classify(&h, &uri()).unwrap();
        legacy.stamp("/api/orgs/<x>", &mut out);
        assert_eq!(out["deprecation"], "true");
        let links: Vec<_> = out.get_all(http::header::LINK).iter().collect();
        assert_eq!(
            links,
            [
                "</next>; rel=\"next\"",
                r#"<https://tower.example.com/api/orgs/%3Cx%3E>; rel="successor-version""#
            ],
            "a Link a handler set is kept beside the successor"
        );
        assert_eq!(out["sunset"], "Sun, 31 Jan 2027 00:00:00 GMT");

        let mut out = HeaderMap::new();
        deprecation(None)
            .classify(&h, &uri())
            .unwrap()
            .stamp("/mcp", &mut out);
        assert_eq!(out["deprecation"], "true");
        assert!(out.get("sunset").is_none());
    }

    /// A target that is not origin-form never reaches a URL: appended to the
    /// origin, `@evil.com` would make it a URL for another host.
    #[test]
    fn a_target_that_is_not_a_path_becomes_the_root() {
        let h = headers(&[("x-forwarded-host", "factory.example.com")]);
        let legacy = deprecation(None).classify(&h, &uri()).unwrap();
        for bad in ["@evil.com", "evil.com", "*", ""] {
            assert_eq!(
                legacy.successor_url(bad),
                "https://tower.example.com/",
                "{bad:?}"
            );
        }
        // Paths that merely look like other hosts stay on this origin.
        for odd in ["//evil.com", "/\\evil.com", "/@evil.com"] {
            assert_eq!(
                legacy.successor_url(odd),
                format!("https://tower.example.com{odd}"),
                "{odd:?}"
            );
        }

        // Whatever the request carried, the redirect's Location is the origin's.
        let mut targets = vec![Uri::from_static("*")];
        for raw in ["@evil.com", "//evil.com", "/@evil.com"] {
            let built = http::uri::PathAndQuery::try_from(raw).ok().and_then(|pq| {
                let mut parts = http::uri::Parts::default();
                parts.path_and_query = Some(pq);
                Uri::from_parts(parts).ok()
            });
            targets.extend(built);
        }
        for target in targets {
            let response = legacy.redirect(&target);
            let location = response.headers()[http::header::LOCATION].to_str().unwrap();
            assert!(
                location.starts_with("https://tower.example.com/"),
                "{target:?} -> {location}"
            );
            assert!(
                !location.starts_with("https://tower.example.com@"),
                "{target:?} -> {location}"
            );
        }
        assert_eq!(request_target(&Uri::from_static("*")), "/");
    }

    /// The successor keeps the query too: `/auth/login?org=acme` on the old
    /// host is `/auth/login?org=acme` on the new one, not a bare sign-in.
    #[test]
    fn the_successor_link_keeps_the_query() {
        let h = headers(&[("x-forwarded-host", "factory.example.com")]);
        let legacy = deprecation(None).classify(&h, &uri()).unwrap();
        assert_eq!(
            legacy.successor_url("/auth/login?org=acme&next=%2Fo%2Facme"),
            "https://tower.example.com/auth/login?org=acme&next=%2Fo%2Facme"
        );
        let mut out = HeaderMap::new();
        legacy.stamp("/o/acme/queue?status=pending", &mut out);
        assert_eq!(
            out[http::header::LINK],
            r#"<https://tower.example.com/o/acme/queue?status=pending>; rel="successor-version""#
        );
    }

    /// The successor is `OF_PUBLIC_URL`'s origin, whatever trailing slash or
    /// default port it was written with.
    #[test]
    fn the_successor_is_the_public_origin() {
        let d = Deprecation::new(
            &url("https://tower.example.com:443/"),
            &url("https://tower.example.com/mcp"),
            ["factory.example.com"],
            None,
        );
        let h = headers(&[("x-forwarded-host", "factory.example.com")]);
        let legacy = d.classify(&h, &uri()).unwrap();
        assert_eq!(legacy.successor_url("/x"), "https://tower.example.com/x");
    }

    #[test]
    fn the_notice_names_both_addresses_and_the_date() {
        let sunset = Utc.with_ymd_and_hms(2027, 1, 31, 0, 0, 0).unwrap();
        let h = headers(&[("x-forwarded-host", "factory.example.com")]);
        let before = Utc.with_ymd_and_hms(2026, 10, 21, 0, 0, 0).unwrap();
        let legacy = deprecation(Some(sunset)).classify(&h, &uri()).unwrap();
        let notice = legacy.notice_at(before);
        assert!(notice.starts_with("DEPRECATED"), "{notice}");
        assert!(
            notice.contains("https://factory.example.com/mcp"),
            "{notice}"
        );
        assert!(notice.contains("https://tower.example.com/mcp"), "{notice}");
        assert!(notice.contains("2027-01-31T00:00:00Z"), "{notice}");

        assert!(notice.contains("keeps working until"), "{notice}");

        // Past the sunset the old address may still answer, but must not claim
        // to keep working.
        let after = Utc.with_ymd_and_hms(2027, 2, 1, 0, 0, 0).unwrap();
        let retired = legacy.notice_at(after);
        assert!(
            retired.contains("was retired on 2027-01-31T00:00:00Z"),
            "{retired}"
        );
        assert!(!retired.contains("keeps working until"), "{retired}");

        let undated = deprecation(None).classify(&h, &uri()).unwrap().notice();
        assert!(undated.contains("not been published"), "{undated}");

        // Constraint 3: the notice is read by every agent alike.
        let lowered = notice.to_lowercase();
        for client in ["claude", "copilot", "cursor", "codex", "gemini"] {
            assert!(!lowered.contains(client), "{client}");
        }
    }

    /// The legacy resource takes its path from `OF_RESOURCE_URI` and its
    /// scheme and port from `OF_PUBLIC_URL`, like the canonical one.
    #[test]
    fn the_legacy_resource_mirrors_the_canonical_paths_and_port() {
        let d = Deprecation::new(
            &url("http://tower.localhost:8080"),
            &url("http://tower.localhost:8080/agents/mcp"),
            ["factory.localhost"],
            None,
        );
        let h = headers(&[("host", "factory.localhost:8080")]);
        let legacy = d.classify(&h, &uri()).expect("legacy");
        assert_eq!(legacy.host(), "factory.localhost");
        assert_eq!(
            legacy.resource_uri(),
            "http://factory.localhost:8080/agents/mcp"
        );
        assert_eq!(
            legacy.metadata_url(),
            "http://factory.localhost:8080/.well-known/oauth-protected-resource"
        );
    }
}
