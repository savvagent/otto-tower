//! Issue references as job ids: `#928` or `owner/repo#928` wherever a job id
//! is accepted.
//!
//! An agent working an issue knows the issue's number long before it knows the
//! `job-N` the queue gave it, so every tool that names a job also takes the
//! job's `ticketRef`. The two namespaces cannot collide, which is what lets one
//! argument carry both without a second field: a job id is always `job-N`
//! ([`crate::ids`]) and a JIRA key is `PROJ-123`, and neither ever contains
//! `#`. So "contains `#`" is the whole test. A string without one is a job id,
//! passed through untouched so that today's errors for a bad id stay exactly
//! as they are, and a string with one must be a well-formed issue reference or
//! it is refused (never guessed at).
//!
//! Stored `ticketRef`s are not uniform: a webhook-created job stores the
//! tracker's own `owner/repo#N`, while `add_job` and `link_ticket` store what
//! the caller typed, and a renamed repository leaves old spellings behind. So a
//! full reference matches case-insensitively, and a short `#N` matches any
//! `owner/repo` (see [`ticket_match_sql`]).

use crate::error::{Error, Result};
use crate::ids::JobId;

/// Longest `OWNER` or `REPO` segment accepted. GitHub's own limits are lower;
/// this only bounds what a caller can make the server compare against.
const MAX_SEGMENT: usize = 100;

/// Most digits in an issue number. Eighteen always fits a `u64`, so parsing
/// cannot overflow and no caller-chosen number needs a second error path.
const MAX_DIGITS: usize = 18;

/// How much of a refused input is quoted back.
const MAX_QUOTED: usize = 200;

/// What a job argument names: a job directly, or the job linked to an issue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobRef {
    Id(JobId),
    Ticket(TicketQuery),
}

/// A validated issue reference. `owner_repo` is lowercased; `None` is the
/// short form `#N`, which matches the issue number in any repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TicketQuery {
    owner_repo: Option<String>,
    number: u64,
}

impl JobRef {
    /// Parse a job argument. Anything without `#` is a job id, unchanged.
    pub fn parse(raw: &str) -> Result<JobRef> {
        if !raw.contains('#') {
            return Ok(JobRef::Id(JobId::from(raw)));
        }
        let refused = || {
            Error::Invalid(format!(
                "{} is not a job reference. Pass a job id like \"job-42\", or the job's issue \
                 as \"#928\" or \"owner/repo#928\".",
                quoted(raw)
            ))
        };
        let (head, digits) = raw.trim().split_once('#').ok_or_else(refused)?;
        if digits.is_empty()
            || digits.len() > MAX_DIGITS
            || !digits.bytes().all(|b| b.is_ascii_digit())
        {
            return Err(refused());
        }
        let number: u64 = digits.parse().map_err(|_| refused())?;
        let owner_repo = if head.is_empty() {
            None
        } else {
            let (owner, repo) = head.split_once('/').ok_or_else(refused)?;
            if !segment_ok(owner) || !segment_ok(repo) {
                return Err(refused());
            }
            Some(head.to_ascii_lowercase())
        };
        Ok(JobRef::Ticket(TicketQuery { owner_repo, number }))
    }
}

impl TicketQuery {
    /// The lowercased reference, compared against `lower(ticket_ref)`:
    /// `"#928"` or `"acme/api#928"`. Leading zeros are gone.
    pub fn exact(&self) -> String {
        match &self.owner_repo {
            Some(or) => format!("{or}#{}", self.number),
            None => format!("#{}", self.number),
        }
    }

    /// The `LIKE` pattern for the short form, matching `#N` under any
    /// `owner/repo`; `None` for the full form, which matches exactly. Built
    /// from the parsed number alone, so it can hold no caller-supplied `%` or
    /// `_`. The `#` immediately before the digits is what keeps `#928` from
    /// matching `…#1928`.
    pub fn suffix_like(&self) -> Option<String> {
        match self.owner_repo {
            Some(_) => None,
            None => Some(format!("%/%#{}", self.number)),
        }
    }
}

impl TicketQuery {
    /// Whether this is the short form `#N`, which names no repository.
    pub fn is_short(&self) -> bool {
        self.owner_repo.is_none()
    }

    /// The lowercased `owner/repo` of a full reference; `None` for `#N`.
    pub fn owner_repo(&self) -> Option<&str> {
        self.owner_repo.as_deref()
    }

    /// This reference read relative to `owner_repo`: a short `#N` becomes
    /// `owner_repo#N`, as GitHub reads `#N` in an issue of that repository; a
    /// full reference is returned unchanged. `None` when `owner_repo` is not
    /// one valid `OWNER/REPO` ([`owner_repo_ok`]), so a caller cannot qualify
    /// a reference with something no full reference could have said.
    pub fn within(&self, owner_repo: &str) -> Option<TicketQuery> {
        if self.owner_repo.is_some() {
            return Some(self.clone());
        }
        owner_repo_ok(owner_repo).then(|| TicketQuery {
            owner_repo: Some(owner_repo.to_ascii_lowercase()),
            number: self.number,
        })
    }

    /// The issue number.
    pub fn number(&self) -> u64 {
        self.number
    }
}

impl std::fmt::Display for TicketQuery {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.exact())
    }
}

/// SQL: job `j`'s `ticket_ref` matches the [`TicketQuery`] whose
/// [`TicketQuery::exact`] is bound as `$exact` and whose
/// [`TicketQuery::suffix_like`] is bound as `$like`.
pub(crate) fn ticket_match_sql(exact: usize, like: usize) -> String {
    format!(
        "(lower(j.ticket_ref) = ${exact} \
          OR (${like}::text IS NOT NULL AND j.ticket_ref LIKE ${like}))"
    )
}

/// Whether `s` is an acceptable `OWNER` or `REPO` segment: 1 to
/// [`MAX_SEGMENT`] characters of `[A-Za-z0-9._-]`, and not `.` or `..`.
///
/// Public because the same segments end up in GitHub API paths, sent with an
/// installation token: `..` there walks the path to an endpoint the reference
/// never named, so the client checks against this one grammar rather than a
/// second copy of it.
pub fn segment_ok(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_SEGMENT
        && s != "."
        && s != ".."
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Whether `s` is exactly `OWNER/REPO`, each a [`segment_ok`] segment: the
/// shape of a GitHub binding's `external_ref` and of the repository half of a
/// full reference.
pub fn owner_repo_ok(s: &str) -> bool {
    s.split_once('/')
        .is_some_and(|(owner, repo)| segment_ok(owner) && segment_ok(repo))
}

/// The refusal for a short `#N` on a tool that changes a job. A write takes a
/// job id or `owner/repo#N`, because `#N` matches that number in every repo
/// and the one job it finds today may not be the one meant: a write must not
/// land on `acme/web#12` because `acme/api#12` has no job yet.
pub fn short_ref_on_write(ticket: &TicketQuery) -> Error {
    let short = ticket.exact();
    Error::Invalid(format!(
        "{short:?} can match an issue in any repo; on a tool that changes a job, name it by id \
         or as owner/repo{short} (get_job({short:?}) or list_jobs(ticket={short:?}) shows which \
         job it is)"
    ))
}

/// Split a stored full reference, `owner/repo#N`, into its parts, keeping the
/// owner and repo as they were spelled. `None` for anything else, the short
/// form `#N` included, since a request needs a repository to go to.
///
/// The rules are [`JobRef::parse`]'s, plus a positive number (GitHub has no
/// issue 0). Stored refs are not always typed through `JobRef::parse`
/// (`link_ticket` keeps what the caller sent), so whatever builds a request
/// from one checks it here first.
pub fn split_full(raw: &str) -> Option<(&str, &str, i64)> {
    let (head, digits) = raw.trim().split_once('#')?;
    if digits.is_empty() || digits.len() > MAX_DIGITS || !digits.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let number: i64 = digits.parse().ok()?;
    if number <= 0 {
        return None;
    }
    let (owner, repo) = head.split_once('/')?;
    if !segment_ok(owner) || !segment_ok(repo) {
        return None;
    }
    Some((owner, repo, number))
}

/// `raw` as a quoted string, cut to [`MAX_QUOTED`] characters, so a refusal
/// cannot echo an unbounded argument back as tool output.
fn quoted(raw: &str) -> String {
    if raw.chars().count() <= MAX_QUOTED {
        format!("{raw:?}")
    } else {
        let cut: String = raw.chars().take(MAX_QUOTED).collect();
        format!("{cut:?}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ticket(raw: &str) -> TicketQuery {
        match JobRef::parse(raw) {
            Ok(JobRef::Ticket(t)) => t,
            other => panic!("{raw:?} parsed as {other:?}"),
        }
    }

    #[test]
    fn a_string_without_a_hash_is_a_job_id_unchanged() {
        assert_eq!(
            JobRef::parse("job-42").unwrap(),
            JobRef::Id(JobId::from("job-42"))
        );
    }

    #[test]
    fn short_references_parse() {
        for raw in ["#928", " #928 ", "#0928"] {
            let t = ticket(raw);
            assert_eq!(t.owner_repo, None, "{raw:?}");
            assert_eq!(t.number, 928, "{raw:?}");
        }
    }

    #[test]
    fn full_references_parse_lowercased() {
        let t = ticket("acme/api#928");
        assert_eq!(t.owner_repo.as_deref(), Some("acme/api"));
        assert_eq!(t.number, 928);

        let t = ticket("Acme.X/a_b-c#1");
        assert_eq!(t.owner_repo.as_deref(), Some("acme.x/a_b-c"));
        assert_eq!(t.number, 1);
    }

    #[test]
    fn malformed_references_are_refused_naming_both_forms() {
        let long_owner = format!("{}/repo#1", "a".repeat(101));
        let nineteen = format!("#{}", "1".repeat(19));
        for raw in [
            "#",
            "#abc",
            "#-1",
            "a#1",
            "a/b/c#1",
            "../x#1",
            "a/..#1",
            "./x#1",
            "/a#1",
            "a/#1",
            nineteen.as_str(),
            long_owner.as_str(),
            "#1 2",
        ] {
            let err = JobRef::parse(raw).expect_err(raw);
            assert_eq!(err.code(), "invalid_argument", "{raw:?}");
            let msg = err.to_string();
            assert!(msg.contains("\"#928\""), "{raw:?}: {msg}");
            assert!(msg.contains("\"owner/repo#928\""), "{raw:?}: {msg}");
            assert!(msg.contains("\"job-42\""), "{raw:?}: {msg}");
            assert!(msg.contains(&format!("{raw:?}")), "{raw:?}: {msg}");
        }
    }

    #[test]
    fn a_refusal_quotes_at_most_two_hundred_characters() {
        let raw = format!("#{}", "x".repeat(500));
        let msg = JobRef::parse(&raw).unwrap_err().to_string();
        assert!(msg.contains(&format!("{:?}…", &raw[..200])), "{msg}");
        assert!(!msg.contains(&"x".repeat(200)), "{msg}");
    }

    #[test]
    fn exact_and_suffix_like() {
        let short = ticket("#928");
        assert_eq!(short.exact(), "#928");
        assert_eq!(short.suffix_like().as_deref(), Some("%/%#928"));

        let full = ticket("Acme/API#0928");
        assert_eq!(full.exact(), "acme/api#928");
        assert_eq!(full.suffix_like(), None);
    }

    #[test]
    fn split_full_keeps_the_spelling_and_refuses_what_cannot_be_a_path() {
        assert_eq!(split_full("Acme/API#0928"), Some(("Acme", "API", 928)));
        assert_eq!(split_full(" a.b/c_d-e#1 "), Some(("a.b", "c_d-e", 1)));
        let nineteen = format!("a/b#{}", "1".repeat(19));
        for raw in [
            "a/b/c#1",
            "../x#1",
            "a/..#1",
            "./x#1",
            "a/.#1",
            "acme/x/../../orgs/y#1",
            "a%2F..%2Fb/c#1",
            "a b/c#1",
            "#1",
            "a/b#0",
            "a/b#-1",
            "a/b#",
            "a/b#1/comments",
            nineteen.as_str(),
        ] {
            assert_eq!(split_full(raw), None, "{raw:?}");
        }
    }
}
