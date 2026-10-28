use chrono::{DateTime, SecondsFormat, Utc};
use of_core::jobs::{Job, Status, Tracker};
use of_core::ticket_ref::{JobRef, TicketQuery};
use of_core::trackers::{Provider, TrackerBinding};

use crate::jira::Transition;
use crate::webhook::{WebhookEvent, WebhookEventKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InboundDecision {
    Drop,
    Create {
        title: String,
        description: Option<String>,
        ticket_ref: String,
        remote_revision: Option<String>,
    },
    Update {
        title: String,
        description: Option<String>,
        remote_revision: Option<String>,
    },
    Close {
        to: Status,
        result: Option<String>,
        error: Option<String>,
        remote_revision: Option<String>,
    },
    /// Someone other than a bot commented on (or edited or deleted a comment
    /// on) the issue of a live job: mark the job's description as possibly out
    /// of date (`note_ticket_changed(at)`) without rewriting it. GitHub only.
    /// `at` is the issue's `updated_at`, already parsed, and later than the
    /// `ticket_changed_at` of the job as read (when it has one; a concurrent
    /// delivery may have raised it since). It is deliberately not a revision to
    /// store: see the webhook route for why `Changed` leaves the job's
    /// `remote_revision` alone.
    Changed {
        at: DateTime<Utc>,
    },
    NoOp,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobTransition {
    Claimed,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubClose {
    pub state_reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JiraTransitionTarget {
    NamedStatus(String),
    StatusCategory(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboundDecision {
    pub comment: String,
    pub github_close: Option<GithubClose>,
    pub jira_transition: Option<JiraTransitionTarget>,
}

pub fn inbound_decision(
    event: &WebhookEvent,
    binding: &TrackerBinding,
    existing: Option<&Job>,
) -> InboundDecision {
    if !has_trigger_label(event, &binding.trigger_label) {
        return InboundDecision::Drop;
    }

    if let Some(job) = existing {
        if event.provider == Provider::Github && event.kind == WebhookEventKind::IssueComment {
            return github_comment_decision(event, job);
        }
        if is_stale_or_echoed(
            job.remote_revision.as_deref(),
            event.issue.updated_at.as_deref(),
        ) {
            return InboundDecision::Drop;
        }
        if job.status.is_terminal() {
            return InboundDecision::NoOp;
        }
    }

    let ticket_ref = ticket_ref(event);
    let remote_revision = normalize_remote_revision(event.issue.updated_at.as_deref());
    let close = close_status(event);

    match existing {
        Some(_job) if close.is_some() => {
            let to = close.expect("checked above");
            InboundDecision::Close {
                to,
                result: None,
                error: None,
                remote_revision,
            }
        }
        Some(_job) => {
            if event.provider == Provider::Github
                && !matches!(
                    event.action.as_str(),
                    "opened" | "edited" | "labeled" | "closed"
                )
            {
                return InboundDecision::NoOp;
            }
            InboundDecision::Update {
                title: event.issue.title.clone(),
                description: event.issue.body.clone(),
                remote_revision,
            }
        }
        None if close.is_some() => InboundDecision::NoOp,
        // A comment never creates a job, whatever its action (a comment
        // `edited` would otherwise pass the action check below): the issue's
        // own `opened`/`labeled` events are what put an issue on the queue.
        None if event.provider == Provider::Github
            && event.kind == WebhookEventKind::IssueComment =>
        {
            InboundDecision::NoOp
        }
        None => {
            if event.provider == Provider::Github
                && !matches!(event.action.as_str(), "opened" | "edited" | "labeled")
            {
                return InboundDecision::NoOp;
            }
            InboundDecision::Create {
                title: event.issue.title.clone(),
                description: event.issue.body.clone(),
                ticket_ref,
                remote_revision,
            }
        }
    }
}

/// A GitHub comment on the issue of an existing job. A comment is what a job's
/// description has not absorbed, so it marks the job rather than rewriting it:
/// the comment event's issue snapshot carries the same body the job already
/// has. A GitHub comment event never reports a close either (its action is the
/// comment's: created, edited, deleted).
///
/// The shared echo check does not apply. `remote_revision` is the issue
/// revision whose body the job holds, and a later issue edit or otto-tower's
/// own write-back moves it past a comment the job has still not seen, so
/// comparing against it would drop that comment. The guard is instead
/// `ticket_changed_at`, the newest foreign change already counted: a comment
/// no later than it (a redelivery, or an older comment arriving after a newer
/// one) has nothing to add, and neither has a second comment in the same
/// second as one counted, since the mark already holds that time. Whether a
/// counted comment makes the job stale is
/// still decided against `described_at`, so one older than the job's last
/// re-description does not.
fn github_comment_decision(event: &WebhookEvent, job: &Job) -> InboundDecision {
    if job.status.is_terminal() {
        return InboundDecision::NoOp;
    }
    // The bot filter is what keeps otto-tower's own write-back comment (a
    // claim or completion note) from flagging its own job, since no echo check
    // runs ahead of a comment. It also ignores other apps' bots, which is the
    // intent: a CI status comment is not a change to what the work is. It
    // assumes write-backs are posted as the GitHub App (`GithubAppClient`
    // authenticates as an installation), which they are, and that GitHub
    // sends the comment's `user.type`, which it does for every comment.
    let by_bot = event
        .issue
        .comment
        .as_ref()
        .is_some_and(|comment| comment.author_is_bot);
    if by_bot {
        return InboundDecision::NoOp;
    }
    // Without the issue's own timestamp there is no time to record, so the
    // comment is not counted rather than stamped with a guessed time.
    let Some(at) = event
        .issue
        .updated_at
        .as_deref()
        .and_then(parse_remote_revision)
    else {
        return InboundDecision::NoOp;
    };
    if job.ticket_changed_at.is_some_and(|counted| at <= counted) {
        return InboundDecision::Drop;
    }
    InboundDecision::Changed { at }
}

pub fn outbound_decision(
    transition: JobTransition,
    tracker: Tracker,
    detail: Option<&str>,
) -> OutboundDecision {
    let comment = match transition {
        JobTransition::Claimed => detail
            .map(|agent| format!("Claimed by {agent}."))
            .unwrap_or_else(|| "Claimed.".into()),
        JobTransition::Completed => detail
            .map(str::to_string)
            .unwrap_or_else(|| "Completed.".into()),
        JobTransition::Failed => detail
            .map(str::to_string)
            .unwrap_or_else(|| "Failed.".into()),
        JobTransition::Cancelled => detail
            .map(str::to_string)
            .unwrap_or_else(|| "Cancelled.".into()),
    };

    let github_close = matches!(
        (transition, tracker),
        (JobTransition::Completed, Tracker::Github)
    )
    .then(|| GithubClose {
        state_reason: "completed".into(),
    });

    let jira_transition = match (transition, tracker) {
        (JobTransition::Claimed, Tracker::Jira) => {
            Some(JiraTransitionTarget::NamedStatus("In Progress".into()))
        }
        (JobTransition::Completed, Tracker::Jira) => {
            Some(JiraTransitionTarget::StatusCategory("done".into()))
        }
        (JobTransition::Failed, Tracker::Jira) => {
            Some(JiraTransitionTarget::StatusCategory("new".into()))
        }
        _ => None,
    };

    OutboundDecision {
        comment,
        github_close,
        jira_transition,
    }
}

pub fn select_jira_transition<'a>(
    target: &JiraTransitionTarget,
    transitions: &'a [Transition],
) -> Option<&'a Transition> {
    transitions.iter().find(|transition| match target {
        JiraTransitionTarget::NamedStatus(name) => transition.to_status.eq_ignore_ascii_case(name),
        JiraTransitionTarget::StatusCategory(category) => {
            normalized_jira_category(&transition.to_status_category)
                .eq_ignore_ascii_case(&normalized_jira_category(category))
        }
    })
}

pub fn normalize_remote_revision(raw: Option<&str>) -> Option<String> {
    raw.and_then(parse_remote_revision)
        .map(|timestamp| timestamp.to_rfc3339_opts(SecondsFormat::AutoSi, true))
}

/// A tracker revision (an RFC 3339 `updated_at`) as a time, or `None` when it
/// does not parse: an unreadable revision is treated as no revision.
pub fn parse_remote_revision(raw: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|timestamp| timestamp.with_timezone(&Utc))
}

fn is_stale_or_echoed(stored: Option<&str>, incoming: Option<&str>) -> bool {
    match (
        stored.and_then(parse_remote_revision),
        incoming.and_then(parse_remote_revision),
    ) {
        (Some(stored), Some(incoming)) => incoming <= stored,
        _ => false,
    }
}

fn has_trigger_label(event: &WebhookEvent, trigger_label: &str) -> bool {
    event
        .issue
        .labels
        .iter()
        .any(|label| label.eq_ignore_ascii_case(trigger_label))
}

/// The `ticket_ref` a webhook event resolves to, in the same
/// `"{repo}#{number}"` / `"PROJ-123"` convention `add_job` documents. Exported
/// so callers doing their own job lookup before `inbound_decision` runs (the
/// webhook route needs the existing job to decide create-vs-update) derive
/// the identical string instead of risking silent drift between two
/// hand-written formats.
pub fn ticket_ref(event: &WebhookEvent) -> String {
    match event.provider {
        Provider::Github => format!("{}#{}", event.binding_external_ref, event.issue.reference),
        Provider::Jira => event.issue.reference.clone(),
    }
}

fn close_status(event: &WebhookEvent) -> Option<Status> {
    match event.provider {
        Provider::Github if event.action == "closed" => {
            // A completion is billed, so only GitHub's own "completed" counts.
            // `not_planned`, `duplicate` (the work was not delivered under this
            // issue) and any reason added later fail the job instead. A null
            // reason is how GitHub reports closes that predate `state_reason`.
            match event.issue.state_reason.as_deref() {
                Some("completed") | None => Some(Status::Completed),
                Some(_) => Some(Status::Failed),
            }
        }
        Provider::Jira => match normalized_jira_status(&event.issue.state).as_str() {
            "done" | "closed" | "resolved" => Some(Status::Completed),
            "won't do" | "wont do" | "cancelled" | "rejected" | "declined" => Some(Status::Failed),
            _ => None,
        },
        _ => None,
    }
}

fn normalized_jira_status(status: &str) -> String {
    status.trim().to_ascii_lowercase()
}

fn normalized_jira_category(category: &str) -> String {
    match category.trim().to_ascii_lowercase().as_str() {
        "new" | "to do" | "todo" => "new".into(),
        "done" => "done".into(),
        other => other.into(),
    }
}

/// Most references [`blocked_by`] returns for one issue body.
pub const MAX_BLOCKED_BY: usize = 20;

/// `line` with any leading blockquote markers (`>`, `> >`, ...) and the
/// whitespace around them removed.
fn without_quote_markers(line: &str) -> &str {
    let mut rest = line.trim_start();
    while let Some(inner) = rest.strip_prefix('>') {
        rest = inner.trim_start();
    }
    rest
}

/// The issue references named on "Blocked by" lines of an issue body, in order
/// of first appearance, deduplicated, at most [`MAX_BLOCKED_BY`].
///
/// A line counts when it contains the phrase `blocked by` (any case, not as
/// the tail of a longer word such as "unblocked by") outside a fenced code
/// block. Everything after the phrase to the end of the line is searched for
/// `#N` and `OWNER/REPO#N`, whatever separates them, so list markers, quotes
/// and emphasis need no handling of their own. Each candidate is validated by
/// [`JobRef::parse`], the grammar every job argument uses, so nothing here can
/// accept a reference a tool would refuse; one it refuses is skipped. Each is
/// returned in its normalized form ([`of_core::ticket_ref::TicketQuery::exact`]:
/// lowercased, no leading zeros), which is also what duplicates collapse on.
///
/// The cap bounds the lookups a hostile issue body can make one `sync_ticket`
/// call perform.
pub fn blocked_by(body: &str) -> Vec<String> {
    const PHRASE: &str = "blocked by";
    let mut found: Vec<String> = Vec::new();
    // The fence that opened the current code block, if inside one: its
    // character and length. As CommonMark has it, a block is closed only by a
    // line of the same character, at least as long, with nothing after it but
    // whitespace. So a `~~~` line inside a backtick block, a three-backtick
    // example inside a four-backtick block, and a fence line with an info
    // string all leave the block open, and a "Blocked by" line shown inside it
    // stays an example rather than becoming a dependency.
    let mut fence: Option<(char, usize)> = None;
    for line in body.lines() {
        // A fence inside a blockquote (`> ~~~`) is still a fence, so its
        // quote markers come off before looking for one.
        let trimmed = without_quote_markers(line);
        let run = |c: char| trimmed.chars().take_while(|&x| x == c).count();
        let marker = ['`', '~'].into_iter().find(|&c| run(c) >= 3);
        match (fence, marker) {
            (None, Some(c)) => {
                fence = Some((c, run(c)));
                continue;
            }
            (Some((open, len)), Some(c))
                if c == open && run(c) >= len && trimmed[run(c)..].trim().is_empty() =>
            {
                fence = None;
                continue;
            }
            (Some(_), _) => continue,
            (None, None) => {}
        }
        // ASCII lowercasing keeps byte offsets, so `start` indexes `line` too.
        let lower = line.to_ascii_lowercase();
        let Some(start) = lower.match_indices(PHRASE).map(|(i, _)| i).find(|&i| {
            !lower[..i]
                .chars()
                .next_back()
                .is_some_and(|c| c.is_alphanumeric())
        }) else {
            continue;
        };
        let rest = &line[start + PHRASE.len()..];
        let tokens = rest.split(|c: char| {
            !(c.is_ascii_alphanumeric() || matches!(c, '#' | '/' | '.' | '_' | '-'))
        });
        for token in tokens {
            // A sentence's full stop ("Blocked by #14.") is not part of the
            // reference, and a well-formed reference never ends in a dot.
            let token = token.trim_end_matches('.');
            if !token.contains('#') {
                continue;
            }
            let Ok(JobRef::Ticket(query)) = JobRef::parse(token) else {
                continue;
            };
            let exact = query.exact();
            if !found.contains(&exact) {
                found.push(exact);
                if found.len() == MAX_BLOCKED_BY {
                    return found;
                }
            }
        }
    }
    found
}

/// The issues `owner/repo`'s issue body says it is blocked by, as queries to
/// look jobs up with: [`blocked_by`]'s references, with a short `#N` read as
/// an issue in `owner/repo` itself (as GitHub reads it), so `#14` and
/// `owner/repo#14` on one body are one blocker, not two.
pub fn blocker_tickets(body: &str, owner: &str, repo: &str) -> Vec<TicketQuery> {
    let mut tickets: Vec<TicketQuery> = Vec::new();
    for raw in blocked_by(body) {
        let qualified = if raw.starts_with('#') {
            format!("{owner}/{repo}{raw}")
        } else {
            raw
        };
        match JobRef::parse(&qualified) {
            Ok(JobRef::Ticket(t)) if !tickets.contains(&t) => tickets.push(t),
            Ok(_) => {}
            // Reachable only if `owner/repo` is itself outside the reference
            // grammar, in which case no job could match it anyway.
            Err(error) => {
                tracing::warn!(owner, repo, %error, "skipping a Blocked by reference");
            }
        }
    }
    tickets
}

/// The time of a change to an issue that otto-tower did not make, if its
/// `updated_at` says there was one: newer than `stored`, the revision
/// otto-tower recorded after its own last write, or any readable revision when
/// nothing was recorded. An unreadable `updated_at` is no evidence of a
/// change. Whether that change makes the job stale is decided at read time,
/// against when its description was written, so this only reports it.
pub fn foreign_change(updated_at: Option<&str>, stored: Option<&str>) -> Option<DateTime<Utc>> {
    let changed = updated_at.and_then(parse_remote_revision)?;
    match stored.and_then(parse_remote_revision) {
        Some(stored) if changed <= stored => None,
        _ => Some(changed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exact(tickets: &[TicketQuery]) -> Vec<String> {
        tickets.iter().map(TicketQuery::exact).collect()
    }

    #[test]
    fn short_blockers_are_qualified_with_the_issues_own_repo() {
        let body = "Blocked by #14, Acme/API#14 and other/lib#3\nBlocked by #0014 and #15";
        assert_eq!(
            exact(&blocker_tickets(body, "Acme", "API")),
            vec!["acme/api#14", "other/lib#3", "acme/api#15"]
        );
    }

    #[test]
    fn no_blocked_by_line_is_no_blockers() {
        assert!(blocker_tickets("Depends on #14", "acme", "api").is_empty());
        assert!(blocker_tickets("", "acme", "api").is_empty());
    }

    #[test]
    fn a_revision_is_foreign_only_when_newer_than_the_stored_one() {
        let at = "2026-09-03T18:13:00Z";
        let later = "2026-09-03T18:14:00Z";
        let parsed = parse_remote_revision(at);
        assert_eq!(foreign_change(Some(at), None), parsed);
        assert_eq!(foreign_change(Some(at), Some("not a time")), parsed);
        assert_eq!(
            foreign_change(Some(later), Some(at)),
            parse_remote_revision(later)
        );
        assert_eq!(
            foreign_change(Some(at), Some(at)),
            None,
            "otto-tower's own write"
        );
        assert_eq!(foreign_change(Some(at), Some(later)), None);
        // The same instant in another offset is the same revision.
        assert_eq!(
            foreign_change(Some("2026-09-03T12:13:00-06:00"), Some(at)),
            None
        );
        assert_eq!(foreign_change(None, Some(at)), None);
        assert_eq!(foreign_change(Some("garbage"), None), None);
    }

    #[test]
    fn a_single_reference() {
        assert_eq!(blocked_by("Blocked by #925"), vec!["#925"]);
    }

    #[test]
    fn several_references_with_any_separators() {
        assert_eq!(
            blocked_by("**Blocked by:** #1, #2 and acme/x#3"),
            vec!["#1", "#2", "acme/x#3"]
        );
    }

    #[test]
    fn the_phrase_is_case_insensitive() {
        assert_eq!(blocked_by("BLOCKED BY #4"), vec!["#4"]);
        assert_eq!(blocked_by("- [ ] blocked By #5"), vec!["#5"]);
    }

    #[test]
    fn a_reference_before_the_phrase_is_ignored() {
        assert_eq!(blocked_by("#7 is blocked by #8"), vec!["#8"]);
    }

    #[test]
    fn lines_inside_fences_are_ignored() {
        let body = "```\nBlocked by #1\n```\n~~~rust\nBlocked by #2\n~~~\nBlocked by #3";
        assert_eq!(blocked_by(body), vec!["#3"]);
        // A `~~~` line does not close a backtick fence.
        let body = "```\n~~~\nBlocked by #1\n```\nBlocked by #2";
        assert_eq!(blocked_by(body), vec!["#2"]);
    }

    #[test]
    fn a_fence_inside_a_blockquote_is_still_a_fence() {
        let body = "> ~~~\n> Blocked by #14\n> ~~~\n> Blocked by #15";
        assert_eq!(blocked_by(body), vec!["#15"]);
        let body = "> > ```\n> > Blocked by #1\n> > ```\nBlocked by #2";
        assert_eq!(blocked_by(body), vec!["#2"]);
    }

    #[test]
    fn a_fence_closes_only_with_one_at_least_as_long_and_bare() {
        // A three-backtick example inside a four-backtick block stays inside.
        let body = "````\n```\nBlocked by #1\n```\n````\nBlocked by #2";
        assert_eq!(blocked_by(body), vec!["#2"]);
        // A longer closing fence closes; one with an info string does not.
        let body = "```\nBlocked by #1\n```rust\nBlocked by #2\n`````\nBlocked by #3";
        assert_eq!(blocked_by(body), vec!["#3"]);
        // Trailing whitespace after a closing fence is fine.
        let body = "~~~~\nBlocked by #1\n~~~~  \nBlocked by #4";
        assert_eq!(blocked_by(body), vec!["#4"]);
    }

    #[test]
    fn duplicates_collapse() {
        assert_eq!(
            blocked_by("Blocked by #1, #1\nBlocked by #01 and Acme/X#2, acme/x#2"),
            vec!["#1", "acme/x#2"]
        );
    }

    #[test]
    fn at_most_twenty_are_kept() {
        let refs: Vec<String> = (1..=30).map(|n| format!("#{n}")).collect();
        let got = blocked_by(&format!("Blocked by {}", refs.join(" ")));
        assert_eq!(got.len(), MAX_BLOCKED_BY);
        assert_eq!(got, refs[..20].to_vec());
    }

    #[test]
    fn malformed_tokens_are_skipped() {
        assert_eq!(blocked_by("Blocked by #abc, a/b/c#1 and #9"), vec!["#9"]);
    }

    #[test]
    fn several_lines_combine() {
        assert_eq!(
            blocked_by("Intro #100\nBlocked by #1\nsome text\n> Blocked by #2."),
            vec!["#1", "#2"]
        );
    }

    #[test]
    fn no_phrase_no_references() {
        assert!(blocked_by("Fixes #1, see #2").is_empty());
        assert!(blocked_by("").is_empty());
    }

    #[test]
    fn the_phrase_inside_a_longer_word_does_not_count() {
        assert!(blocked_by("Unblocked by #1").is_empty());
    }
}
