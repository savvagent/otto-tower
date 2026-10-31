//! Everything the process cannot infer, read once from the environment.
//!
//! Two rules decide whether a setting gets a default, and they are the reason
//! this file is longer than a `serde` derive would be:
//!
//! 1. **A setting whose wrong value fails silently has no default.** A wrong
//!    `OF_PUBLIC_URL` does not crash: it hands out links to an origin that does not
//!    exist and advertises an audience nothing accepts, and the first
//!    report arrives hours later from somebody who cannot sign in. Refusing to
//!    start is the cheap version of that failure.
//! 2. **A setting whose wrong value is merely inconvenient gets one.** The bind
//!    address, the log format, and the static directory are all obvious within
//!    seconds of looking.
//!
//! Nothing here falls back quietly. A variable that is set but unparseable is
//! an error naming the variable and what it accepts, never a default — a
//! `OF_RUN_MIGRATIONS=yes-please` that silently reads as "off" is how a
//! deployment ships a schema nobody migrated. A variable this server no longer
//! reads is not ignored quietly either: [`RETIRED`] names each one, and
//! [`Config::warn_retired`] logs it at startup, so an operator who sets it
//! learns it does nothing instead of believing it does something.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, NaiveDate, Utc};

/// The whole deployment, resolved.
///
/// `Debug` is written by hand and prints no secret: this struct holds the
/// platform credentials, the encryption key, and the tracker providers' client
/// secrets, and a `{:?}` in a log line must never be how they leak.
#[derive(Clone)]
pub struct Config {
    pub database_url: String,
    pub bind: SocketAddr,

    /// Public origin a browser sees. The links this service hands out and the
    /// discovery pointer in a `401` are built from it.
    pub public_url: String,
    /// Canonical MCP resource URI, and the audience every token carries. Must
    /// be exactly the `resource_uri` this service was registered with at the
    /// platform (`savvagent-platform-server resource register`).
    pub resource_uri: String,

    /// The Savvagent platform's base URL, e.g. `https://savvagent.com`: the OAuth
    /// authorization server, the identity directory, and the billing system of
    /// record. Advertised to MCP clients as this resource's authorization server
    /// and called for token introspection, member and team lookups, and usage.
    pub platform_url: String,
    /// The introspection credential the platform issued when this service was
    /// registered (`sav_rs_…`). Authenticates every call this service makes to
    /// the platform, as HTTP Basic with `resource_uri` as the user.
    pub introspection_secret: String,
    /// The key the platform signs its lifecycle webhooks to this service with
    /// (`sav_whsec_…`), issued when the webhook URL
    /// (`{public_url}/platform/webhooks`) was registered. A different secret
    /// from the introspection credential, on purpose.
    pub platform_webhook_secret: String,

    /// 32 bytes, base64. Encrypts secrets at rest (currently tracker webhook
    /// secrets and JIRA OAuth credentials — see `tower_core::trackers` and
    /// `tower_trackers::jira`).
    pub encryption_key: String,

    /// GitHub App id for tracker integration. Optional because deployments
    /// that do not enable GitHub integration have no App configured.
    pub github_app_id: Option<i64>,
    /// PEM-encoded GitHub App private key. Optional for the same reason as
    /// `github_app_id`; PEM validity is checked later, when a task constructs
    /// a `GithubAppClient` from it, rather than here where the key is just
    /// configuration data. Literal `\n` escapes (how a multi-line PEM is
    /// commonly stored in a single-line `.env` value or a CI secret) are
    /// normalized to real newlines before storage.
    pub github_app_private_key: Option<String>,
    /// Shared secret for GitHub webhook signature verification. Optional
    /// because GitHub integration itself is optional per deployment.
    pub github_app_webhook_secret: Option<String>,
    /// The GitHub App's URL slug, used to build the installation link the
    /// console sends an admin to (`github.com/apps/{slug}/installations/new`).
    /// Optional for the same reason as the rest; the console offers no Connect
    /// GitHub button without it, rather than linking somewhere that 404s.
    pub github_app_slug: Option<String>,
    /// The GitHub App's OAuth client id and secret — the *user*-to-server
    /// credentials, distinct from `github_app_private_key`, which is the
    /// server-to-server one. The console needs these to prove that the admin
    /// binding an installation id actually administers that installation; see
    /// `docs/specs/2026-09-04-tracker-console-design.md` §2 for why an
    /// unverified installation id is a cross-tenant escalation.
    pub github_app_client_id: Option<String>,
    pub github_app_client_secret: Option<String>,
    /// Atlassian OAuth client id for JIRA tracker sync. Optional because JIRA
    /// integration itself is optional per deployment.
    pub jira_client_id: Option<String>,
    /// Atlassian OAuth client secret for JIRA tracker sync.
    pub jira_client_secret: Option<String>,

    /// The console's OAuth client id at the platform, for console sign-in. A
    /// public first-party client an operator registers
    /// (`savvagent-platform-server client register --first-party`) with the redirect
    /// URI `{public_url}/auth/callback`. Optional: while unset, `/auth/login`
    /// answers `503 console_login_disabled` and everything else runs, so the
    /// server can be deployed before the client exists.
    pub console_client_id: Option<String>,

    /// Where an owner goes to lift a budget or payment block, named in the
    /// refusal. Defaults to `platform_url`, the console's home, which lists
    /// the user's orgs (billing lives under each org there; there is no
    /// org-independent billing page to link to).
    pub upgrade_url: String,
    /// Retired variables ([`RETIRED`]) that are set in the environment, to be
    /// warned about once logging is up. See [`Config::warn_retired`].
    pub retired_set: Vec<&'static str>,

    /// Extra authorities accepted in the MCP endpoint's `Host` header, beyond
    /// the one derived from `public_url`.
    pub extra_allowed_hosts: Vec<String>,
    /// Hostnames this deployment still answers on but no longer advertises
    /// (`OF_LEGACY_HOSTS`): after a rename, the old public host until its
    /// sunset. Accepted like `extra_allowed_hosts`, and every response through
    /// one carries a deprecation notice (`tower_mcp::legacy`). Bare, lowercase
    /// hostnames; never `public_url`'s own.
    pub legacy_hosts: Vec<String>,
    /// When the legacy hosts stop working (`OF_LEGACY_SUNSET`), announced in a
    /// `Sunset` header and in the MCP instructions. Requires `legacy_hosts`.
    pub legacy_sunset: Option<DateTime<Utc>>,
    /// Hostnames that only redirect (`OF_REDIRECT_HOSTS`): a request whose
    /// `Host` is one of these gets a `308` to the same path and query on
    /// `public_url`, whatever the method and path, before anything else
    /// looks at it. Bare, lowercase hostnames; never `public_url`'s own, and
    /// never also in `extra_allowed_hosts` or `legacy_hosts` (a host cannot
    /// both be served and redirected).
    pub redirect_hosts: Vec<String>,
    /// Browser origins accepted on requests to `/mcp` that carry `Origin`.
    /// Empty disables the check, which is right for a surface reached by CLI
    /// agents rather than by pages.
    pub allowed_origins: Vec<String>,

    /// The built console bundle. Served with an `index.html` fallback.
    pub static_dir: PathBuf,

    pub run_migrations: bool,
    pub log_format: LogFormat,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Config")
            .field("bind", &self.bind)
            .field("public_url", &self.public_url)
            .field("resource_uri", &self.resource_uri)
            .field("platform_url", &self.platform_url)
            .field("console_client_id", &self.console_client_id)
            .field("upgrade_url", &self.upgrade_url)
            .field("retired_set", &self.retired_set)
            .field("legacy_hosts", &self.legacy_hosts)
            .field("legacy_sunset", &self.legacy_sunset)
            .field("redirect_hosts", &self.redirect_hosts)
            .field("static_dir", &self.static_dir)
            .field("run_migrations", &self.run_migrations)
            .field("log_format", &self.log_format)
            .finish_non_exhaustive()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogFormat {
    /// One JSON object per line, for a log aggregator.
    Json,
    /// Human-readable, for a terminal.
    Text,
}

impl Config {
    /// Read the environment. Every failure names the variable it is about.
    pub fn from_env() -> Result<Self> {
        let public_url = required("OF_PUBLIC_URL")?.trim_end_matches('/').to_string();

        // Parsed rather than merely stored: a `OF_PUBLIC_URL` with no scheme
        // produces links a mail client will not linkify and an `allowed_hosts`
        // entry of the empty string, both of which are much harder to diagnose
        // later than a message here.
        let parsed = url::Url::parse(&public_url)
            .with_context(|| format!("OF_PUBLIC_URL is not a URL: {public_url:?}"))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(anyhow!(
                "OF_PUBLIC_URL must be http or https, got {:?}",
                parsed.scheme()
            ));
        }
        if parsed.host_str().is_none() {
            return Err(anyhow!("OF_PUBLIC_URL has no host: {public_url:?}"));
        }

        let (legacy_hosts, legacy_sunset) = legacy(
            optional("OF_LEGACY_HOSTS").as_deref(),
            optional("OF_LEGACY_SUNSET").as_deref(),
            parsed.host_str().unwrap_or_default(),
        )?;

        let extra_allowed_hosts = list("OF_ALLOWED_HOSTS");
        let redirect_hosts = redirect_hosts(
            optional("OF_REDIRECT_HOSTS").as_deref(),
            parsed.host_str().unwrap_or_default(),
            &extra_allowed_hosts,
            &legacy_hosts,
        )?;

        let platform_url = required("OF_PLATFORM_URL")?
            .trim_end_matches('/')
            .to_string();
        let platform = url::Url::parse(&platform_url)
            .with_context(|| format!("OF_PLATFORM_URL is not a URL: {platform_url:?}"))?;
        if !matches!(platform.scheme(), "http" | "https") || platform.host_str().is_none() {
            return Err(anyhow!(
                "OF_PLATFORM_URL must be an http(s) URL with a host, got {platform_url:?}"
            ));
        }

        Ok(Self {
            database_url: required("DATABASE_URL")?,
            bind: parse_var("OF_BIND", "0.0.0.0:8080", |v| {
                v.parse::<SocketAddr>()
                    .map_err(|e| anyhow!("{e}; expected host:port, e.g. 0.0.0.0:8080"))
            })?,

            // Defaults to the MCP endpoint on the public origin, which is where
            // this binary actually serves it. Overridable because a deployment
            // behind a rewriting proxy may advertise a different canonical URI,
            // and the audience must match what the AS mints tokens for.
            resource_uri: optional("OF_RESOURCE_URI")
                .unwrap_or_else(|| format!("{public_url}/mcp")),

            platform_url: platform_url.clone(),
            introspection_secret: required("OF_INTROSPECTION_SECRET")?,
            platform_webhook_secret: required("OF_PLATFORM_WEBHOOK_SECRET")?,

            encryption_key: required("OF_ENCRYPTION_KEY")?,
            github_app_id: optional("OF_GITHUB_APP_ID")
                .map(|value| {
                    value.trim().parse::<i64>().with_context(|| {
                        format!(
                            "OF_GITHUB_APP_ID={value:?} is not valid; expected an integer App id"
                        )
                    })
                })
                .transpose()?,
            github_app_private_key: optional("OF_GITHUB_APP_PRIVATE_KEY")
                .map(normalize_pem_newlines),
            github_app_webhook_secret: optional("OF_GITHUB_APP_WEBHOOK_SECRET"),
            github_app_slug: optional("OF_GITHUB_APP_SLUG"),
            github_app_client_id: optional("OF_GITHUB_APP_CLIENT_ID"),
            github_app_client_secret: optional("OF_GITHUB_APP_CLIENT_SECRET"),
            jira_client_id: optional("OF_JIRA_CLIENT_ID"),
            jira_client_secret: optional("OF_JIRA_CLIENT_SECRET"),
            console_client_id: optional("OF_CONSOLE_CLIENT_ID"),

            upgrade_url: upgrade_url(optional("OF_UPGRADE_URL"), &platform_url),
            retired_set: retired_set(|name| std::env::var_os(name).is_some()),

            extra_allowed_hosts,
            legacy_hosts,
            legacy_sunset,
            redirect_hosts,
            allowed_origins: list("OF_ALLOWED_ORIGINS"),

            static_dir: optional("OF_STATIC_DIR")
                .unwrap_or_else(|| "web/build".into())
                .into(),

            run_migrations: parse_var("OF_RUN_MIGRATIONS", "1", parse_bool)?,
            log_format: parse_var("OF_LOG_FORMAT", "text", |v| match v {
                "json" => Ok(LogFormat::Json),
                "text" => Ok(LogFormat::Text),
                other => Err(anyhow!("expected json or text, got {other:?}")),
            })?,

            // Last, because the defaults above are built from it.
            public_url,
        })
    }

    /// Warn when `OF_LEGACY_SUNSET` has already passed. The legacy hosts keep
    /// answering (removing them is the operator's step 11), and their notice
    /// says they were retired; an operator restarting past the date has most
    /// likely forgotten that step, and the log is where they will see it.
    pub fn warn_past_sunset(&self, now: DateTime<Utc>) {
        if let Some(sunset) = self.past_sunset(now) {
            tracing::warn!(
                sunset = %sunset.to_rfc3339(),
                hosts = ?self.legacy_hosts,
                "OF_LEGACY_SUNSET has passed but OF_LEGACY_HOSTS still serves the old \
                 host(s); finish the rename (docs/deploy/fly.md, step 11) or move the date"
            );
        }
    }

    fn past_sunset(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        self.legacy_sunset.filter(|sunset| *sunset <= now)
    }

    /// Log a warning for each retired variable that is set. Called once
    /// tracing is initialised, which is after `from_env` has run.
    pub fn warn_retired(&self) {
        for name in &self.retired_set {
            tracing::warn!(variable = name, "{}", retired_warning(name));
        }
    }

    /// Authorities the MCP endpoint accepts in `Host`.
    ///
    /// The public URL's host is always in the list, because that is the name
    /// this deployment tells clients to use. `rmcp` treats an entry with no
    /// port as matching any port, so the bare host covers `example.com:8080`
    /// as well as `example.com`. The legacy hosts are accepted too: a client
    /// still configured with the old URL must keep working until the sunset.
    pub fn allowed_hosts(&self) -> Vec<String> {
        let mut hosts: Vec<String> = url::Url::parse(&self.public_url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .into_iter()
            .collect();
        hosts.extend(self.extra_allowed_hosts.iter().cloned());
        hosts.extend(self.legacy_hosts.iter().cloned());
        hosts
    }
}

/// Variables this server used to read and no longer does, with why. Each one
/// that is set is warned about at startup rather than silently ignored.
pub const RETIRED: &[(&str, &str)] = &[(
    "OF_ENFORCE_QUOTAS",
    "completed jobs are always refused when the Savvagent platform reports the \
     organization blocked (budget_exceeded or payment_required), and nothing \
     else is ever refused for billing; remove it from the environment",
)];

/// The retired variables `is_set` reports as present. Presence is what
/// matters, not the value: `OF_ENFORCE_QUOTAS=0` does nothing either.
fn retired_set(is_set: impl Fn(&str) -> bool) -> Vec<&'static str> {
    RETIRED
        .iter()
        .map(|(name, _)| *name)
        .filter(|name| is_set(name))
        .collect()
}

fn retired_warning(name: &str) -> String {
    let why = RETIRED
        .iter()
        .find(|(n, _)| *n == name)
        .map_or("", |(_, why)| why);
    format!("{name} is set but has no effect: it is retired. {why}")
}

/// `OF_UPGRADE_URL` when set, else the platform console's home.
fn upgrade_url(explicit: Option<String>, platform_url: &str) -> String {
    explicit.unwrap_or_else(|| tower_billing::default_fix_url(platform_url))
}

fn optional(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.trim().is_empty())
}

fn required(name: &str) -> Result<String> {
    optional(name).ok_or_else(|| anyhow!("{name} is required and not set"))
}

/// Parse an optional variable, defaulting only when it is absent.
///
/// A variable that is *present* and unparseable is an error. Falling back to
/// the default there would mean a typo in a value someone deliberately set is
/// indistinguishable from not setting it.
fn parse_var<T>(name: &str, default: &str, parse: impl Fn(&str) -> Result<T>) -> Result<T> {
    let raw = optional(name);
    let value = raw.as_deref().unwrap_or(default);
    parse(value.trim()).with_context(|| format!("{name}={value:?} is not valid"))
}

fn parse_bool(v: &str) -> Result<bool> {
    match v.to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        other => Err(anyhow!(
            "expected a boolean (1/0, true/false, yes/no, on/off), got {other:?}"
        )),
    }
}

/// A comma-separated list. Absent and empty both mean "none".
fn list(name: &str) -> Vec<String> {
    optional(name)
        .map(|v| {
            v.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// `OF_LEGACY_HOSTS` and `OF_LEGACY_SUNSET`, together.
///
/// A sunset with no legacy host announces the end of nothing; it is refused
/// rather than ignored, because whoever set it believes something is being
/// announced.
fn legacy(
    hosts: Option<&str>,
    sunset: Option<&str>,
    public_host: &str,
) -> Result<(Vec<String>, Option<DateTime<Utc>>)> {
    let hosts = legacy_hosts(hosts, public_host)?;
    let sunset = legacy_sunset(sunset)?;
    if sunset.is_some() && hosts.is_empty() {
        return Err(anyhow!(
            "OF_LEGACY_SUNSET is set but OF_LEGACY_HOSTS is not; a sunset date \
             announces when the legacy hosts stop working, so name them in \
             OF_LEGACY_HOSTS or unset OF_LEGACY_SUNSET"
        ));
    }
    Ok((hosts, sunset))
}

/// `OF_LEGACY_HOSTS`: comma-separated bare hostnames.
///
/// Strict, because the failure of a loose parse is silent: an entry written as
/// a URL (`https://old.example.com`) or with a port would never equal a request's
/// host, so the old address would answer without its deprecation notice and
/// nobody would be told to move before the sunset. The public URL's own host
/// is refused too — the canonical host cannot also be the deprecated one.
fn legacy_hosts(raw: Option<&str>, public_host: &str) -> Result<Vec<String>> {
    bare_hostnames("OF_LEGACY_HOSTS", "deprecated", raw, public_host)
}

/// `OF_REDIRECT_HOSTS`: the same strict list as [`legacy_hosts`], plus the
/// refusal of any host that is also served.
///
/// A host in both lists would be ambiguous, and the redirect would silently win
/// (it runs first), so whoever added it to the allow-list to keep a client
/// working would be wrong without being told. Compared case-insensitively and
/// without surrounding whitespace, since `OF_ALLOWED_HOSTS` is not normalized.
fn redirect_hosts(
    raw: Option<&str>,
    public_host: &str,
    allowed: &[String],
    legacy: &[String],
) -> Result<Vec<String>> {
    let hosts = bare_hostnames("OF_REDIRECT_HOSTS", "redirected", raw, public_host)?;
    for host in &hosts {
        let clash = |name: &str, list: &[String]| {
            list.iter()
                .any(|h| h.trim().eq_ignore_ascii_case(host))
                .then(|| {
                    anyhow!(
                        "OF_REDIRECT_HOSTS names {host:?}, which is also in {name}; a host \
                         is either served or redirected, so remove it from one of them"
                    )
                })
        };
        if let Some(err) =
            clash("OF_ALLOWED_HOSTS", allowed).or_else(|| clash("OF_LEGACY_HOSTS", legacy))
        {
            return Err(err);
        }
    }
    Ok(hosts)
}

fn bare_hostnames(
    var: &str,
    role: &str,
    raw: Option<&str>,
    public_host: &str,
) -> Result<Vec<String>> {
    let Some(raw) = raw else {
        return Ok(vec![]);
    };
    let invalid = |why: String| {
        anyhow!(
            "{var}={raw:?} is not valid: {why}. Expected comma-separated \
             bare hostnames, e.g. old.example.com"
        )
    };
    let mut hosts = Vec::new();
    for entry in raw.split(',').map(str::trim) {
        // An empty entry (`a,,b`, a trailing comma) is refused rather than
        // skipped, so the Worker, which reads the same value, can apply exactly
        // the same rule.
        if entry.is_empty() {
            return Err(invalid("it has an empty entry".into()));
        }
        let host = entry.to_ascii_lowercase();
        // Dot-separated labels of letters, digits and hyphens (an
        // internationalized name in its `xn--` form), and a parse that leaves
        // it unchanged as a URL's host. A trailing dot is refused: it is a
        // legal DNS spelling, but no client sends it in `Host`, so an entry
        // carrying one would never match.
        let bare = host.split('.').all(|label| {
            !label.is_empty()
                && label
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        }) && url::Url::parse(&format!("https://{host}/"))
            .is_ok_and(|u| u.host_str() == Some(host.as_str()));
        if !bare {
            return Err(invalid(format!(
                "{entry:?} is not a bare hostname (no scheme, port, path, or trailing dot)"
            )));
        }
        if host == public_host.to_ascii_lowercase() {
            return Err(invalid(format!(
                "{entry:?} is OF_PUBLIC_URL's own host, which is canonical and cannot \
                 also be {role}"
            )));
        }
        if !hosts.contains(&host) {
            hosts.push(host);
        }
    }
    Ok(hosts)
}

/// `OF_LEGACY_SUNSET`: an RFC 3339 date-time, or a bare RFC 3339 full-date
/// (taken as midnight UTC, the start of that day).
fn legacy_sunset(raw: Option<&str>) -> Result<Option<DateTime<Utc>>> {
    let Some(raw) = raw.map(str::trim) else {
        return Ok(None);
    };
    if let Ok(at) = DateTime::parse_from_rfc3339(raw) {
        return Ok(Some(at.with_timezone(&Utc)));
    }
    if let Ok(day) = NaiveDate::parse_from_str(raw, "%Y-%m-%d") {
        if let Some(midnight) = day.and_hms_opt(0, 0, 0) {
            return Ok(Some(midnight.and_utc()));
        }
    }
    Err(anyhow!(
        "OF_LEGACY_SUNSET={raw:?} is not valid; expected an RFC 3339 date-time such as \
         2027-01-31T00:00:00Z, or a date such as 2027-01-31"
    ))
}

/// Normalize a PEM value that arrived as a literal `\n`-escaped single line —
/// the common shape for a multi-line secret stored in a `.env` file or a CI
/// secret store that doesn't preserve real newlines.
fn normalize_pem_newlines(value: String) -> String {
    value.replace("\\n", "\n")
}

#[cfg(test)]
impl Config {
    /// A complete, valid `Config` for tests.
    ///
    /// One fixture rather than a literal per test: a new field then fails to
    /// compile here once, instead of being quietly defaulted in every test that
    /// happens to build a `Config` by hand.
    pub(crate) fn for_test() -> Self {
        Self {
            database_url: "postgres://x".into(),
            bind: "0.0.0.0:8080".parse().expect("test bind"),
            public_url: "https://factory.example.com".into(),
            resource_uri: "https://factory.example.com/mcp".into(),
            platform_url: "https://otto.example.com".into(),
            introspection_secret: "otto_rs_test".into(),
            platform_webhook_secret: "otto_whsec_test".into(),
            encryption_key: "k".into(),
            github_app_id: None,
            github_app_private_key: None,
            github_app_webhook_secret: None,
            github_app_slug: None,
            github_app_client_id: None,
            github_app_client_secret: None,
            jira_client_id: None,
            jira_client_secret: None,
            console_client_id: None,
            upgrade_url: "https://otto.example.com".into(),
            retired_set: vec![],
            extra_allowed_hosts: vec![],
            legacy_hosts: vec![],
            legacy_sunset: None,
            redirect_hosts: vec![],
            allowed_origins: vec![],
            static_dir: "web/build".into(),
            run_migrations: true,
            log_format: LogFormat::Text,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn booleans_accept_the_spellings_people_actually_write() {
        for yes in ["1", "true", "TRUE", "yes", "On"] {
            assert!(parse_bool(yes).unwrap(), "{yes}");
        }
        for no in ["0", "false", "NO", "off"] {
            assert!(!parse_bool(no).unwrap(), "{no}");
        }
    }

    /// The whole point of `parse_bool` returning a `Result`. `OF_RUN_MIGRATIONS`
    /// reading as "off" because somebody wrote `enabled` is a control that is
    /// deployed switched off, and nothing says so.
    #[test]
    fn a_misspelled_boolean_is_an_error_and_not_a_default() {
        for bad in ["enabled", "y", "2", "please"] {
            assert!(parse_bool(bad).is_err(), "{bad} should not parse");
        }
    }

    /// The v0.10.0 console has no `/settings/billing`; billing lives under each
    /// org. Without an override, the refusal points at the console's home,
    /// which lists the user's orgs.
    #[test]
    fn the_fix_url_defaults_to_the_platform_console_home() {
        assert_eq!(
            upgrade_url(None, "https://otto.example.com"),
            "https://otto.example.com"
        );
        assert_eq!(
            upgrade_url(None, "https://otto.example.com/"),
            "https://otto.example.com"
        );
        assert_eq!(
            upgrade_url(
                Some("https://billing.example.com/fix".into()),
                "https://otto.example.com"
            ),
            "https://billing.example.com/fix"
        );
    }

    /// A retired variable is noticed whatever its value, and only when set.
    #[test]
    fn a_retired_variable_is_noticed_when_set() {
        assert_eq!(
            retired_set(|name| name == "OF_ENFORCE_QUOTAS"),
            vec!["OF_ENFORCE_QUOTAS"]
        );
        assert!(retired_set(|_| false).is_empty());
    }

    /// Setting `OF_ENFORCE_QUOTAS` must produce a warning that names it and
    /// says it does nothing, not silence: an operator who set it to turn
    /// enforcement off would otherwise believe they had.
    #[test]
    fn a_retired_variable_that_is_set_is_warned_about() {
        #[derive(Clone, Default)]
        struct Captured(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
        impl std::io::Write for Captured {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let captured = Captured::default();
        let writer = captured.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .with_ansi(false)
            .finish();
        let mut config = Config::for_test();
        config.retired_set = vec!["OF_ENFORCE_QUOTAS"];
        tracing::subscriber::with_default(subscriber, || config.warn_retired());

        let logged = String::from_utf8(captured.0.lock().unwrap().clone()).unwrap();
        assert!(logged.contains("WARN"), "{logged}");
        assert!(logged.contains("OF_ENFORCE_QUOTAS"), "{logged}");
        assert!(logged.contains("no effect"), "{logged}");

        // Nothing set, nothing logged.
        let quiet = Captured::default();
        let writer = quiet.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_writer(move || writer.clone())
            .finish();
        tracing::subscriber::with_default(subscriber, || Config::for_test().warn_retired());
        assert!(quiet.0.lock().unwrap().is_empty());
    }

    #[test]
    fn a_default_applies_only_when_the_variable_is_absent() {
        // Absent: the default is used.
        assert_eq!(
            parse_var("OF_TEST_ABSENT_VAR_XYZ", "7", |v| Ok(v.parse::<u8>()?)).unwrap(),
            7
        );
    }

    #[test]
    fn a_list_treats_absent_empty_and_whitespace_alike() {
        assert!(list("OF_TEST_ABSENT_LIST_XYZ").is_empty());
    }

    #[test]
    fn a_pem_with_literal_newline_escapes_is_normalized() {
        assert_eq!(
            normalize_pem_newlines(
                "-----BEGIN RSA PRIVATE KEY-----\\nabc\\n-----END RSA PRIVATE KEY-----".into()
            ),
            "-----BEGIN RSA PRIVATE KEY-----\nabc\n-----END RSA PRIVATE KEY-----"
        );
    }

    #[test]
    fn a_pem_with_real_newlines_already_is_left_unchanged() {
        let pem = "-----BEGIN RSA PRIVATE KEY-----\nabc\n-----END RSA PRIVATE KEY-----";
        assert_eq!(normalize_pem_newlines(pem.into()), pem);
    }

    #[test]
    fn legacy_hosts_are_bare_lowercase_hostnames() {
        assert!(legacy_hosts(None, "tower.example.com").unwrap().is_empty());
        assert_eq!(
            legacy_hosts(
                Some(" Factory.Example.com , old.example.com,factory.example.com"),
                "tower.example.com"
            )
            .unwrap(),
            vec!["factory.example.com", "old.example.com"]
        );
    }

    /// Set but unparseable is a startup error, never "no legacy hosts": a host
    /// that silently never matches answers without the notice until the sunset.
    #[test]
    fn a_legacy_host_that_could_never_match_is_an_error() {
        for bad in [
            "https://factory.example.com",
            "factory.example.com:443",
            "factory.example.com/mcp",
            "user@factory.example.com",
            "factory example.com",
            "factory.example.com.",
            ".factory.example.com",
            "factory..example.com",
            ",",
            " , ",
            "factory.example.com,",
            "factory.example.com,,old.example.com",
        ] {
            let err = legacy_hosts(Some(bad), "tower.example.com")
                .expect_err(bad)
                .to_string();
            assert!(err.contains("OF_LEGACY_HOSTS"), "{bad}: {err}");
        }
    }

    #[test]
    fn the_canonical_host_cannot_also_be_legacy() {
        let err = legacy_hosts(Some("TOWER.example.com"), "tower.example.com")
            .unwrap_err()
            .to_string();
        assert!(err.contains("canonical"), "{err}");
    }

    #[test]
    fn redirect_hosts_are_bare_lowercase_hostnames_and_default_to_none() {
        let none = |raw| redirect_hosts(raw, "tower.example.com", &[], &[]).unwrap();
        assert!(none(None).is_empty());
        assert_eq!(
            none(Some(" Old.Example.com ,older.example.com,old.example.com")),
            vec!["old.example.com", "older.example.com"]
        );
        for bad in [
            "https://old.example.com",
            "old.example.com:443",
            "old.example.com/mcp",
            "old.example.com.",
            "old.example.com,",
            ",",
        ] {
            let err = redirect_hosts(Some(bad), "tower.example.com", &[], &[])
                .expect_err(bad)
                .to_string();
            assert!(err.contains("OF_REDIRECT_HOSTS"), "{bad}: {err}");
        }
    }

    #[test]
    fn the_public_host_cannot_be_redirected() {
        let err = redirect_hosts(Some("TOWER.example.com"), "tower.example.com", &[], &[])
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("canonical") && err.contains("OF_REDIRECT_HOSTS"),
            "{err}"
        );
    }

    #[test]
    fn a_host_cannot_be_both_redirected_and_served() {
        let allowed = vec!["x.example.com".to_string(), " Old.example.com".to_string()];
        let err = redirect_hosts(Some("old.example.com"), "tower.example.com", &allowed, &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("OF_ALLOWED_HOSTS"), "{err}");

        let legacy = vec!["old.example.com".to_string()];
        let err = redirect_hosts(Some("old.example.com"), "tower.example.com", &[], &legacy)
            .unwrap_err()
            .to_string();
        assert!(err.contains("OF_LEGACY_HOSTS"), "{err}");

        // Disjoint lists are fine.
        let ok = redirect_hosts(
            Some("old.example.com"),
            "tower.example.com",
            &allowed[..1],
            &[],
        );
        assert_eq!(ok.unwrap(), vec!["old.example.com"]);
    }

    #[test]
    fn a_sunset_is_an_rfc_3339_date_or_date_time() {
        use chrono::TimeZone;
        let midnight = Utc.with_ymd_and_hms(2027, 1, 31, 0, 0, 0).unwrap();
        assert_eq!(legacy_sunset(None).unwrap(), None);
        assert_eq!(
            legacy_sunset(Some("2027-01-31T00:00:00Z")).unwrap(),
            Some(midnight)
        );
        assert_eq!(
            legacy_sunset(Some("2027-01-31T01:00:00+01:00")).unwrap(),
            Some(midnight)
        );
        assert_eq!(legacy_sunset(Some("2027-01-31")).unwrap(), Some(midnight));
    }

    #[test]
    fn an_unparseable_sunset_is_an_error_and_not_a_default() {
        for bad in [
            "soon",
            "31/01/2027",
            "2027-02-30",
            "Sun, 31 Jan 2027 00:00:00 GMT",
        ] {
            let err = legacy_sunset(Some(bad)).expect_err(bad).to_string();
            assert!(err.contains("OF_LEGACY_SUNSET"), "{bad}: {err}");
        }
    }

    #[test]
    fn a_sunset_without_a_legacy_host_is_an_error() {
        let err = legacy(None, Some("2027-01-31"), "tower.example.com")
            .unwrap_err()
            .to_string();
        assert!(err.contains("OF_LEGACY_HOSTS"), "{err}");

        // Both unset is the normal state outside a rename, and changes nothing.
        let (hosts, sunset) = legacy(None, None, "tower.example.com").unwrap();
        assert!(hosts.is_empty() && sunset.is_none());

        let (hosts, sunset) = legacy(
            Some("old.example.com"),
            Some("2027-01-31"),
            "tower.example.com",
        )
        .unwrap();
        assert_eq!(hosts, vec!["old.example.com"]);
        assert!(sunset.is_some());
    }

    #[test]
    fn a_sunset_already_past_is_noticed() {
        use chrono::TimeZone;
        let mut config = Config::for_test();
        config.legacy_hosts = vec!["old.example.com".into()];
        config.legacy_sunset = Some(Utc.with_ymd_and_hms(2027, 1, 31, 0, 0, 0).unwrap());

        let before = Utc.with_ymd_and_hms(2027, 1, 30, 0, 0, 0).unwrap();
        let after = Utc.with_ymd_and_hms(2027, 2, 1, 0, 0, 0).unwrap();
        assert_eq!(config.past_sunset(before), None);
        assert_eq!(config.past_sunset(after), config.legacy_sunset);

        config.legacy_sunset = None;
        assert_eq!(config.past_sunset(after), None);
    }

    #[test]
    fn the_legacy_hosts_are_accepted_by_the_mcp_endpoint() {
        let mut config = Config::for_test();
        config.legacy_hosts = vec!["old.example.com".into()];
        assert!(config
            .allowed_hosts()
            .contains(&"old.example.com".to_string()));
    }

    #[test]
    fn allowed_hosts_always_contain_the_public_origins_host() {
        let mut config = Config::for_test();
        config.extra_allowed_hosts = vec!["otto-factory-mcp.fly.dev".into()];

        assert_eq!(
            config.allowed_hosts(),
            vec![
                "factory.example.com".to_string(),
                "otto-factory-mcp.fly.dev".to_string()
            ]
        );
    }
}
