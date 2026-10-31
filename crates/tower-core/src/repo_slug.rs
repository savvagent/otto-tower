//! The caller-chosen `slug` of a registered repo.
//!
//! `register_repo` takes a free-form slug, and other members' agents then read
//! it inside server sentences: the "registered repos" list of
//! `RepoUnresolved` and the owner named by `RemoteTaken`. Unbounded, a slug is
//! a place to plant a paragraph of instruction-shaped text in a peer's tool
//! output (the same hole `agent_label` closes for labels).
//!
//! - [`validate`] bounds what is accepted at write time. It refuses rather than
//!   rewrites or trims: a slug is an identifier an agent passes back verbatim,
//!   so a silently changed one would no longer round-trip.
//! - [`is_displayable`], [`describe`] and [`render_known`] are the read side. A
//!   slug stored before this rule existed fails today's [`validate`] and is
//!   withheld from prose, without a migration rewriting tenant rows. It stays
//!   reachable as structured data (`list_repos`), because hiding the handle
//!   would strand the repo.
//!
//! The rule is about shape only. It never compares a slug against a list of
//! known hosts or clients.

use crate::error::{Error, Result};

/// The longest accepted slug, in characters. A maximal GitHub `owner/repo`
/// (up to 39 + 1 + 100 = 140 characters) is deliberately refused: a slug is a
/// short handle an agent passes back, not a copy of the remote string.
pub const MAX_LEN: usize = 128;

/// The charset, start and length part of the rule as a regex, for the OpenAPI
/// schema. It is weaker than [`validate`]: the path-segment rule (no empty,
/// `.` or `..` segment) is enforced only there, as a regex cannot say it
/// readably.
pub const PATTERN: &str = r"^[A-Za-z0-9][A-Za-z0-9._/-]*$";

/// The most slugs `render_known` names; bounds the sentence to a few KB
/// however many repos an org registers.
pub const MAX_LISTED: usize = 50;

/// Checks a caller-supplied slug: 1 to [`MAX_LEN`] characters, first an ASCII
/// letter or digit, the rest ASCII letters, digits, `-`, `_`, `.` or `/`. No `/`-separated segment may be empty, `.` or
/// `..`.
///
/// No trimming. The refusal describes the problem and never repeats the slug,
/// since echoing it would hand the refused text straight back as tool output.
///
/// ```
/// use tower_core::repo_slug::validate;
/// assert_eq!(validate("acme/api-v2").unwrap(), "acme/api-v2");
/// assert_eq!(validate("a b").unwrap_err().code(), "invalid_repo_slug");
/// ```
pub fn validate(slug: &str) -> Result<&str> {
    let len = slug.chars().count();
    if len == 0 {
        return Err(refuse("is empty".into()));
    }
    if len > MAX_LEN {
        return Err(refuse(format!("is {len} characters long")));
    }
    let mut chars = slug.chars();
    if !chars.next().is_some_and(|c| c.is_ascii_alphanumeric()) {
        return Err(refuse("must start with an ASCII letter or digit".into()));
    }
    if let Some((i, c)) = slug
        .chars()
        .enumerate()
        .find(|(_, c)| !(c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/')))
    {
        return Err(refuse(format!(
            "contains U+{:04X} at character {}, which is not allowed",
            c as u32,
            i + 1
        )));
    }
    // A slug is pasted into URL path segments (`/repos/{repo}`); `/` is allowed
    // for `owner/repo`, but an empty, `.` or `..` segment would let it climb or
    // collapse a path in a client that does not encode.
    if slug.split('/').any(|seg| matches!(seg, "" | "." | "..")) {
        return Err(refuse("has an empty, '.' or '..' path segment".into()));
    }
    Ok(slug)
}

fn refuse(problem: String) -> Error {
    Error::InvalidRepoSlug { problem }
}

/// Whether a stored slug may appear in server prose: exactly when it passes
/// today's [`validate`].
pub fn is_displayable(slug: &str) -> bool {
    validate(slug).is_ok()
}

/// A repo named for an error sentence: the slug in quotes if it is displayable,
/// otherwise a pointer to `list_repos`. The quotes are written here, which is
/// safe because a conforming slug holds no `"` or `\`.
///
/// ```
/// use tower_core::repo_slug::describe;
/// assert_eq!(describe("api"), "\"api\"");
/// assert_eq!(describe("a\nb"), "another repo (call list_repos)");
/// ```
pub fn describe(slug: &str) -> String {
    if is_displayable(slug) {
        format!("\"{slug}\"")
    } else {
        "another repo (call list_repos)".to_string()
    }
}

/// The `known` text of `RepoUnresolved` for the slugs the caller may see
/// (already ordered). At most [`MAX_LISTED`] displayable slugs are named; the
/// rest are counted, never quoted.
pub fn render_known<S: AsRef<str>>(visible: &[S]) -> String {
    if visible.is_empty() {
        return "none registered yet".to_string();
    }
    let shown: Vec<&str> = visible
        .iter()
        .map(AsRef::as_ref)
        .filter(|s| is_displayable(s))
        .take(MAX_LISTED)
        .collect();
    let hidden = visible.len() - shown.len();
    if shown.is_empty() {
        return format!("none shown ({hidden} registered; call list_repos for the full list)");
    }
    let list = shown
        .iter()
        .map(|s| format!("\"{s}\""))
        .collect::<Vec<_>>()
        .join(", ");
    if hidden == 0 {
        list
    } else {
        format!("{list} (and {hidden} more not shown; call list_repos for the full list)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refusal(slug: &str) -> String {
        match validate(slug) {
            Err(e) => {
                assert_eq!(e.code(), "invalid_repo_slug");
                e.to_string()
            }
            Ok(v) => panic!("expected {slug:?} to be refused, got {v:?}"),
        }
    }

    #[test]
    fn accepts_conforming_slugs() {
        for s in ["api", "acme/api-v2", "a.b_c", "a..b", &"a".repeat(128)] {
            assert_eq!(validate(s).unwrap(), s);
        }
    }

    #[test]
    fn refuses_empty_dot_and_dotdot_path_segments() {
        for s in ["x/../y", "a//b", "a/", "./a", "a/./b", "a/..", "a/b/"] {
            let msg = refusal(s);
            assert!(!msg.contains(s), "{msg:?} repeats {s:?}");
        }
        assert!(refusal("a//b").contains("path segment"));
        for s in ["a/b.c", "a..b", "a/.b", "a/b..", "a/..b"] {
            assert_eq!(validate(s).unwrap(), s);
        }
    }

    /// Hand-rolled match of `PATTERN` (tower-core has no regex dependency).
    fn matches_pattern(s: &str) -> bool {
        assert_eq!(PATTERN, r"^[A-Za-z0-9][A-Za-z0-9._/-]*$");
        let mut it = s.chars();
        it.next().is_some_and(|c| c.is_ascii_alphanumeric())
            && it.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '/' | '-'))
    }

    #[test]
    fn pattern_agrees_with_validate_on_charset_start_and_length() {
        // Inputs free of path-segment problems, so only what PATTERN can say differs.
        let long = "a".repeat(129);
        for s in [
            "api",
            "acme/api-v2",
            "a.b_c",
            "a..b",
            "a/b.c",
            "-zq",
            ".zq",
            "/zq",
            "a b",
            "a\nb",
            "a\"b",
            "aé",
            "api ",
            " api",
            "",
        ] {
            assert_eq!(matches_pattern(s), validate(s).is_ok(), "{s:?}");
        }
        // Length is carried by maxLength, not the pattern.
        assert!(matches_pattern(&long) && validate(&long).is_err());
        // Path-segment rule is beyond the pattern, by design.
        assert!(matches_pattern("a//b") && validate("a//b").is_err());
    }

    #[test]
    fn refuses_the_rest_without_echoing_them() {
        let long = "a".repeat(129);
        for s in [
            "", &long, "-zq", ".zq", "/zq", "a b", "a\nb", "a\"b", "aé", "api ", " api",
        ] {
            let msg = refusal(s);
            if !s.is_empty() {
                assert!(!msg.contains(s), "{msg:?} repeats {s:?}");
            }
        }
    }

    #[test]
    fn displayable_agrees_with_validate() {
        for s in ["api", "", "a b", "-x", "acme/api"] {
            assert_eq!(is_displayable(s), validate(s).is_ok());
        }
    }

    #[test]
    fn describe_quotes_or_points_at_list_repos() {
        assert_eq!(describe("api"), "\"api\"");
        assert_eq!(describe("evil\nslug"), "another repo (call list_repos)");
    }

    #[test]
    fn render_known_cases() {
        let none: [&str; 0] = [];
        assert_eq!(render_known(&none), "none registered yet");
        assert_eq!(render_known(&["api", "web"]), "\"api\", \"web\"");
        assert_eq!(
            render_known(&["api", "bad\nslug", "web", "x y"]),
            "\"api\", \"web\" (and 2 more not shown; call list_repos for the full list)"
        );
        assert_eq!(
            render_known(&["a b", "c\nd", "e f"]),
            "none shown (3 registered; call list_repos for the full list)"
        );
    }

    #[test]
    fn render_known_is_cut_by_max_listed() {
        let many: Vec<String> = (0..60).map(|i| format!("r{i:02}")).collect();
        let out = render_known(&many);
        assert_eq!(out.matches('"').count(), MAX_LISTED * 2);
        assert!(out.ends_with("(and 10 more not shown; call list_repos for the full list)"));
    }

    #[test]
    fn length_counts_characters_and_a_multibyte_tail_is_a_charset_refusal() {
        let msg = refusal(&format!("a{}", "é".repeat(127)));
        assert!(msg.contains("U+00E9"), "{msg}");
        assert!(!msg.contains("characters long"), "{msg}");
        assert!(refusal(&"a".repeat(129)).contains("is 129 characters long"));
        assert!(validate(&"a".repeat(128)).is_ok());
    }

    #[test]
    fn render_known_counts_cap_cut_and_shape_withheld_together() {
        let mut many: Vec<String> = (0..60).map(|i| format!("r{i:02}")).collect();
        many[3] = "bad slug".into();
        many[7] = "bad\nslug".into();
        let out = render_known(&many);
        // 58 displayable, 50 shown; the 10 hidden are 8 cut by the cap and 2 withheld.
        assert_eq!(out.matches('"').count(), MAX_LISTED * 2);
        assert!(out.ends_with("(and 10 more not shown; call list_repos for the full list)"));
        assert!(!out.contains("bad"));
    }

    #[test]
    fn render_known_at_the_edge_of_the_cap() {
        let fifty: Vec<String> = (0..50).map(|i| format!("r{i:02}")).collect();
        assert!(!render_known(&fifty).contains("more not shown"));
        let fifty_one: Vec<String> = (0..51).map(|i| format!("r{i:02}")).collect();
        let out = render_known(&fifty_one);
        assert_eq!(out.matches('"').count(), MAX_LISTED * 2);
        assert!(out.ends_with("(and 1 more not shown; call list_repos for the full list)"));
    }
}
