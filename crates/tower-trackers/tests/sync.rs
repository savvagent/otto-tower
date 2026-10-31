use savvagent_tenant::ids::OrgId;
use tower_core::ids::{JobId, RepoId};
use tower_core::jobs::{Job, Status, Tracker};
use tower_core::trackers::{Provider, TrackerBinding};

fn binding(provider: Provider, trigger_label: &str) -> TrackerBinding {
    TrackerBinding {
        id: uuid::Uuid::new_v4(),
        org_id: OrgId::new(),
        repo_id: RepoId::new(),
        connection_id: Some(uuid::Uuid::new_v4()),
        provider,
        external_ref: match provider {
            Provider::Github => "acme/api".into(),
            Provider::Jira => "ENG".into(),
        },
        trigger_label: trigger_label.into(),
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
    }
}

fn job(status: Status, tracker: Tracker, ticket_ref: &str, remote_revision: Option<&str>) -> Job {
    Job {
        id: JobId::from("job-7"),
        org_id: OrgId::new(),
        repo_id: RepoId::new(),
        team_id: None,
        title: "Old title".into(),
        description: Some("Old body".into()),
        status,
        ticket_ref: Some(ticket_ref.into()),
        tracker: Some(tracker),
        remote_revision: remote_revision.map(str::to_string),
        agent_type: None,
        metadata: serde_json::json!({}),
        created_at: chrono::Utc::now(),
        started_at: None,
        completed_at: None,
        attempts: 0,
        result: None,
        error: None,
        created_by: None,
        claimed_by: None,
        claimed_by_label: None,
        claim_expires_at: None,
        cancel_requested_at: None,
        cancel_requested_by: None,
        cancel_reason: None,
        ticket_changed_at: None,
        ticket_stale: false,
    }
}

/// A GitHub `issue_comment` event on issue 17, by a human unless `bot`.
fn github_comment_event(
    action: &str,
    issue_state: &str,
    updated_at: Option<&str>,
    bot: bool,
) -> tower_trackers::webhook::WebhookEvent {
    let mut event = github_issue_event(action, &["otto-factory"], updated_at, None);
    event.kind = tower_trackers::webhook::WebhookEventKind::IssueComment;
    event.issue.state = issue_state.into();
    event.issue.comment = Some(tower_trackers::webhook::CommentSnapshot {
        id: "9001".into(),
        body: "Actually, also handle the empty case.".into(),
        author_is_bot: bot,
    });
    event
}

fn github_issue_event(
    action: &str,
    labels: &[&str],
    updated_at: Option<&str>,
    state_reason: Option<&str>,
) -> tower_trackers::webhook::WebhookEvent {
    tower_trackers::webhook::WebhookEvent {
        provider: Provider::Github,
        connection_external_id: "123456".into(),
        binding_external_ref: "acme/api".into(),
        action: action.into(),
        kind: tower_trackers::webhook::WebhookEventKind::Issue,
        issue: tower_trackers::webhook::IssueSnapshot {
            id: "7001".into(),
            reference: "17".into(),
            title: "New title".into(),
            body: Some("New body".into()),
            state: if action == "closed" { "closed" } else { "open" }.into(),
            labels: labels.iter().map(|label| label.to_string()).collect(),
            updated_at: updated_at.map(str::to_string),
            state_reason: state_reason.map(str::to_string),
            comment: None,
        },
    }
}

fn jira_issue_event(
    state: &str,
    labels: &[&str],
    updated_at: Option<&str>,
) -> tower_trackers::webhook::WebhookEvent {
    tower_trackers::webhook::WebhookEvent {
        provider: Provider::Jira,
        connection_external_id: "cloud-1".into(),
        binding_external_ref: "ENG".into(),
        action: "updated".into(),
        kind: tower_trackers::webhook::WebhookEventKind::Issue,
        issue: tower_trackers::webhook::IssueSnapshot {
            id: "10001".into(),
            reference: "ENG-7".into(),
            title: "New title".into(),
            body: Some("New body".into()),
            state: state.into(),
            labels: labels.iter().map(|label| label.to_string()).collect(),
            updated_at: updated_at.map(str::to_string),
            state_reason: None,
            comment: None,
        },
    }
}

#[test]
fn inbound_drops_when_the_trigger_label_does_not_match() {
    let binding = binding(Provider::Github, "otto-factory");
    let event = github_issue_event("opened", &["triaged"], Some("2026-09-03T12:00:00Z"), None);

    let decision = tower_trackers::sync::inbound_decision(&event, &binding, None);
    assert!(matches!(
        decision,
        tower_trackers::sync::InboundDecision::Drop
    ));
}

#[test]
fn inbound_creates_for_a_matching_issue_without_an_existing_job() {
    let binding = binding(Provider::Github, "otto-factory");
    let event = github_issue_event(
        "opened",
        &["otto-factory"],
        Some("2026-09-03T12:00:00-06:00"),
        None,
    );

    let decision = tower_trackers::sync::inbound_decision(&event, &binding, None);
    match decision {
        tower_trackers::sync::InboundDecision::Create {
            title,
            description,
            ticket_ref,
            remote_revision,
        } => {
            assert_eq!(title, "New title");
            assert_eq!(description.as_deref(), Some("New body"));
            assert_eq!(ticket_ref, "acme/api#17");
            assert_eq!(remote_revision.as_deref(), Some("2026-09-03T18:00:00Z"));
        }
        other => panic!("unexpected decision: {other:?}"),
    }
}

#[test]
fn inbound_updates_a_matching_non_terminal_job() {
    let binding = binding(Provider::Jira, "otto-factory");
    let event = jira_issue_event(
        "In Progress",
        &["otto-factory"],
        Some("2026-09-03T12:00:00Z"),
    );
    let existing = job(
        Status::InProgress,
        Tracker::Jira,
        "ENG-7",
        Some("2026-09-03T10:00:00Z"),
    );

    let decision = tower_trackers::sync::inbound_decision(&event, &binding, Some(&existing));
    match decision {
        tower_trackers::sync::InboundDecision::Update {
            title,
            description,
            remote_revision,
        } => {
            assert_eq!(title, "New title");
            assert_eq!(description.as_deref(), Some("New body"));
            assert_eq!(remote_revision.as_deref(), Some("2026-09-03T12:00:00Z"));
        }
        other => panic!("unexpected decision: {other:?}"),
    }
}

#[test]
fn inbound_github_closed_event_completes_or_fails_by_state_reason() {
    let binding = binding(Provider::Github, "otto-factory");
    let complete = github_issue_event(
        "closed",
        &["otto-factory"],
        Some("2026-09-03T12:00:00Z"),
        Some("completed"),
    );
    let fail = github_issue_event(
        "closed",
        &["otto-factory"],
        Some("2026-09-03T12:00:00Z"),
        Some("not_planned"),
    );
    let existing = job(Status::Pending, Tracker::Github, "acme/api#17", None);

    let complete_decision =
        tower_trackers::sync::inbound_decision(&complete, &binding, Some(&existing));
    let fail_decision = tower_trackers::sync::inbound_decision(&fail, &binding, Some(&existing));

    assert!(matches!(
        complete_decision,
        tower_trackers::sync::InboundDecision::Close {
            to: Status::Completed,
            ..
        }
    ));
    assert!(matches!(
        fail_decision,
        tower_trackers::sync::InboundDecision::Close {
            to: Status::Failed,
            ..
        }
    ));
}

/// A close is billed work only when GitHub says the work was delivered. A
/// duplicate, or a reason this code has never heard of, must not read as a
/// completion.
#[test]
fn inbound_github_close_completes_only_for_completed() {
    let binding = binding(Provider::Github, "otto-factory");
    let existing = job(Status::InProgress, Tracker::Github, "acme/api#17", None);

    for reason in ["not_planned", "duplicate", "something_new"] {
        let event = github_issue_event(
            "closed",
            &["otto-factory"],
            Some("2026-09-03T12:00:00Z"),
            Some(reason),
        );
        assert!(
            matches!(
                tower_trackers::sync::inbound_decision(&event, &binding, Some(&existing)),
                tower_trackers::sync::InboundDecision::Close {
                    to: Status::Failed,
                    ..
                }
            ),
            "state_reason {reason:?} must fail the job"
        );
    }
}

#[test]
fn inbound_jira_closed_vocabulary_completes_or_fails() {
    let binding = binding(Provider::Jira, "otto-factory");
    let done = jira_issue_event("Resolved", &["otto-factory"], Some("2026-09-03T12:00:00Z"));
    let failed = jira_issue_event("Rejected", &["otto-factory"], Some("2026-09-03T12:00:00Z"));
    let existing = job(Status::InProgress, Tracker::Jira, "ENG-7", None);

    assert!(matches!(
        tower_trackers::sync::inbound_decision(&done, &binding, Some(&existing)),
        tower_trackers::sync::InboundDecision::Close {
            to: Status::Completed,
            ..
        }
    ));
    assert!(matches!(
        tower_trackers::sync::inbound_decision(&failed, &binding, Some(&existing)),
        tower_trackers::sync::InboundDecision::Close {
            to: Status::Failed,
            ..
        }
    ));
}

#[test]
fn inbound_leaves_unrecognized_jira_closed_like_states_alone() {
    let binding = binding(Provider::Jira, "otto-factory");
    let event = jira_issue_event(
        "Ready For Verification",
        &["otto-factory"],
        Some("2026-09-03T12:00:00Z"),
    );
    let existing = job(Status::InProgress, Tracker::Jira, "ENG-7", None);

    let decision = tower_trackers::sync::inbound_decision(&event, &binding, Some(&existing));
    assert!(matches!(
        decision,
        tower_trackers::sync::InboundDecision::Update { .. }
    ));
}

#[test]
fn inbound_drops_stale_or_echoed_revisions() {
    let binding = binding(Provider::Github, "otto-factory");
    let event = github_issue_event(
        "edited",
        &["otto-factory"],
        Some("2026-09-03T12:00:00Z"),
        None,
    );
    let existing = job(
        Status::Pending,
        Tracker::Github,
        "acme/api#17",
        Some("2026-09-03T12:00:00Z"),
    );

    let decision = tower_trackers::sync::inbound_decision(&event, &binding, Some(&existing));
    assert!(matches!(
        decision,
        tower_trackers::sync::InboundDecision::Drop
    ));
}

#[test]
fn inbound_applies_unparseable_or_missing_revisions_without_clearing_the_stored_one() {
    let binding = binding(Provider::Github, "otto-factory");
    let bad = github_issue_event("edited", &["otto-factory"], Some("not-a-timestamp"), None);
    let missing = github_issue_event("edited", &["otto-factory"], None, None);
    let existing = job(
        Status::Pending,
        Tracker::Github,
        "acme/api#17",
        Some("2026-09-03T12:00:00Z"),
    );

    for event in [&bad, &missing] {
        match tower_trackers::sync::inbound_decision(event, &binding, Some(&existing)) {
            tower_trackers::sync::InboundDecision::Update {
                remote_revision, ..
            } => {
                assert!(remote_revision.is_none());
            }
            other => panic!("unexpected decision: {other:?}"),
        }
    }
}

#[test]
fn inbound_terminal_jobs_are_left_alone() {
    let binding = binding(Provider::Github, "otto-factory");
    let event = github_issue_event(
        "edited",
        &["otto-factory"],
        Some("2026-09-03T12:00:00Z"),
        None,
    );
    let existing = job(Status::Completed, Tracker::Github, "acme/api#17", None);

    let decision = tower_trackers::sync::inbound_decision(&event, &binding, Some(&existing));
    assert!(matches!(
        decision,
        tower_trackers::sync::InboundDecision::NoOp
    ));
}

#[test]
fn outbound_plans_comment_text_and_provider_specific_transition_targets() {
    let claimed = tower_trackers::sync::outbound_decision(
        tower_trackers::sync::JobTransition::Claimed,
        Tracker::Jira,
        None,
    );
    let completed = tower_trackers::sync::outbound_decision(
        tower_trackers::sync::JobTransition::Completed,
        Tracker::Github,
        None,
    );
    let failed = tower_trackers::sync::outbound_decision(
        tower_trackers::sync::JobTransition::Failed,
        Tracker::Jira,
        Some("boom"),
    );

    assert_eq!(claimed.comment, "Claimed.");
    assert!(matches!(
        claimed.jira_transition,
        Some(tower_trackers::sync::JiraTransitionTarget::NamedStatus(ref name)) if name == "In Progress"
    ));
    assert_eq!(completed.comment, "Completed.");
    assert!(matches!(
        completed.github_close,
        Some(tower_trackers::sync::GithubClose { state_reason: ref reason }) if reason == "completed"
    ));
    assert_eq!(failed.comment, "boom");
    assert!(matches!(
        failed.jira_transition,
        Some(tower_trackers::sync::JiraTransitionTarget::StatusCategory(ref category)) if category == "new"
    ));
}

#[test]
fn outbound_uses_supplied_claim_comment_and_picks_matching_jira_transitions() {
    let decision = tower_trackers::sync::outbound_decision(
        tower_trackers::sync::JobTransition::Claimed,
        Tracker::Jira,
        Some("api-agent"),
    );
    let transitions = vec![
        tower_trackers::jira::Transition {
            id: "1".into(),
            name: "To Do".into(),
            to_status: "To Do".into(),
            to_status_category: "new".into(),
        },
        tower_trackers::jira::Transition {
            id: "2".into(),
            name: "Start".into(),
            to_status: "In Progress".into(),
            to_status_category: "indeterminate".into(),
        },
    ];

    let chosen = tower_trackers::sync::select_jira_transition(
        decision.jira_transition.as_ref().unwrap(),
        &transitions,
    )
    .unwrap();

    assert_eq!(decision.comment, "Claimed by api-agent.");
    assert_eq!(chosen.id, "2");
}

#[test]
fn outbound_returns_none_when_no_jira_transition_matches() {
    let decision = tower_trackers::sync::outbound_decision(
        tower_trackers::sync::JobTransition::Completed,
        Tracker::Jira,
        Some("done"),
    );
    let transitions = vec![tower_trackers::jira::Transition {
        id: "1".into(),
        name: "Backlog".into(),
        to_status: "Backlog".into(),
        to_status_category: "new".into(),
    }];

    let chosen = tower_trackers::sync::select_jira_transition(
        decision.jira_transition.as_ref().unwrap(),
        &transitions,
    );

    assert!(chosen.is_none());
}

/// `Cancelled` gets its own comment-only shape rather than reusing `Failed`'s:
/// reusing `Failed` would fire `outbound_decision`'s `(Failed, Jira)` arm and
/// transition a cancelled ticket back to "new" — announcing "still needs
/// doing" about work someone just asked to stop.
#[test]
fn outbound_cancelled_never_transitions_or_closes_a_tracker() {
    let with_detail = tower_trackers::sync::outbound_decision(
        tower_trackers::sync::JobTransition::Cancelled,
        Tracker::Jira,
        Some("stopped as requested"),
    );
    assert_eq!(with_detail.comment, "stopped as requested");
    assert!(with_detail.jira_transition.is_none());

    let no_detail = tower_trackers::sync::outbound_decision(
        tower_trackers::sync::JobTransition::Cancelled,
        Tracker::Jira,
        None,
    );
    assert_eq!(no_detail.comment, "Cancelled.");
    assert!(no_detail.jira_transition.is_none());

    let github = tower_trackers::sync::outbound_decision(
        tower_trackers::sync::JobTransition::Cancelled,
        Tracker::Github,
        None,
    );
    assert!(github.github_close.is_none());
}

fn at(rfc3339: &str) -> chrono::DateTime<chrono::Utc> {
    rfc3339.parse().expect("test timestamp")
}

/// A human comment is a change to the issue the job's description has not
/// absorbed. Every comment action counts: an edit or a deletion changes what
/// the thread says just as a new comment does.
#[test]
fn inbound_github_human_comment_on_a_live_job_is_changed() {
    let binding = binding(Provider::Github, "otto-factory");
    let existing = job(
        Status::InProgress,
        Tracker::Github,
        "acme/api#17",
        Some("2026-09-03T10:00:00Z"),
    );
    for action in ["created", "edited", "deleted"] {
        let event = github_comment_event(action, "open", Some("2026-09-03T12:00:00-06:00"), false);
        assert_eq!(
            tower_trackers::sync::inbound_decision(&event, &binding, Some(&existing)),
            tower_trackers::sync::InboundDecision::Changed {
                at: at("2026-09-03T18:00:00Z"),
            },
            "{action}"
        );
    }
}

#[test]
fn inbound_github_bot_comment_is_a_no_op() {
    let binding = binding(Provider::Github, "otto-factory");
    let existing = job(Status::Pending, Tracker::Github, "acme/api#17", None);
    let event = github_comment_event("created", "open", Some("2026-09-03T12:00:00Z"), true);
    assert_eq!(
        tower_trackers::sync::inbound_decision(&event, &binding, Some(&existing)),
        tower_trackers::sync::InboundDecision::NoOp
    );
}

/// A comment no later than the change already counted on the job (a
/// redelivery, or an older comment arriving after a newer one) has nothing to
/// add, whatever the job's stored revision.
#[test]
fn inbound_github_comment_already_counted_is_dropped() {
    let binding = binding(Provider::Github, "otto-factory");
    let mut existing = job(
        Status::Pending,
        Tracker::Github,
        "acme/api#17",
        Some("2026-09-03T10:00:00Z"),
    );
    existing.ticket_changed_at = Some(at("2026-09-03T12:00:00Z"));
    for updated_at in ["2026-09-03T12:00:00Z", "2026-09-03T11:00:00Z"] {
        let event = github_comment_event("created", "open", Some(updated_at), false);
        assert_eq!(
            tower_trackers::sync::inbound_decision(&event, &binding, Some(&existing)),
            tower_trackers::sync::InboundDecision::Drop,
            "{updated_at}"
        );
    }
}

/// A comment is not part of the issue body the stored revision describes, so a
/// comment no newer than that revision (delivered after a later issue edit or
/// Savvagent Tower's own write-back, or made in the same second) is still a change
/// the job has not counted.
#[test]
fn inbound_github_comment_older_than_the_stored_revision_is_changed() {
    let binding = binding(Provider::Github, "otto-factory");
    for counted in [None, Some(at("2026-09-03T10:00:00Z"))] {
        let mut existing = job(
            Status::InProgress,
            Tracker::Github,
            "acme/api#17",
            Some("2026-09-03T12:00:00Z"),
        );
        existing.ticket_changed_at = counted;
        for updated_at in ["2026-09-03T12:00:00Z", "2026-09-03T11:00:00Z"] {
            let event = github_comment_event("created", "open", Some(updated_at), false);
            assert_eq!(
                tower_trackers::sync::inbound_decision(&event, &binding, Some(&existing)),
                tower_trackers::sync::InboundDecision::Changed { at: at(updated_at) },
                "{counted:?} {updated_at}"
            );
        }
    }
}

#[test]
fn inbound_github_comment_on_a_terminal_job_is_a_no_op() {
    let binding = binding(Provider::Github, "otto-factory");
    let existing = job(Status::Completed, Tracker::Github, "acme/api#17", None);
    let event = github_comment_event("created", "open", Some("2026-09-03T12:00:00Z"), false);
    assert_eq!(
        tower_trackers::sync::inbound_decision(&event, &binding, Some(&existing)),
        tower_trackers::sync::InboundDecision::NoOp
    );
}

/// With no timestamp of the issue's own there is no time to record and no
/// echo check to run, so the comment is not counted rather than guessed at.
#[test]
fn inbound_github_comment_without_a_usable_revision_is_a_no_op() {
    let binding = binding(Provider::Github, "otto-factory");
    let existing = job(Status::Pending, Tracker::Github, "acme/api#17", None);
    for updated_at in [None, Some("not-a-timestamp")] {
        let event = github_comment_event("created", "open", updated_at, false);
        assert_eq!(
            tower_trackers::sync::inbound_decision(&event, &binding, Some(&existing)),
            tower_trackers::sync::InboundDecision::NoOp,
            "{updated_at:?}"
        );
    }
}

/// A comment never creates a job: only the issue's own events do.
#[test]
fn inbound_github_comment_without_a_job_creates_nothing() {
    let binding = binding(Provider::Github, "otto-factory");
    for action in ["created", "edited", "deleted"] {
        let event = github_comment_event(action, "open", Some("2026-09-03T12:00:00Z"), false);
        assert_eq!(
            tower_trackers::sync::inbound_decision(&event, &binding, None),
            tower_trackers::sync::InboundDecision::NoOp,
            "{action}"
        );
    }
}

/// The issue's own `edited` event still rewrites the job from its body.
#[test]
fn inbound_github_issue_edit_still_updates() {
    let binding = binding(Provider::Github, "otto-factory");
    let existing = job(Status::Pending, Tracker::Github, "acme/api#17", None);
    let event = github_issue_event(
        "edited",
        &["otto-factory"],
        Some("2026-09-03T12:00:00Z"),
        None,
    );
    assert!(matches!(
        tower_trackers::sync::inbound_decision(&event, &binding, Some(&existing)),
        tower_trackers::sync::InboundDecision::Update { .. }
    ));
}

/// JIRA keeps its existing behaviour: a comment event carries the issue's
/// fields and updates the job from them.
#[test]
fn inbound_jira_comment_still_updates() {
    let binding = binding(Provider::Jira, "otto-factory");
    let mut event = jira_issue_event(
        "In Progress",
        &["otto-factory"],
        Some("2026-09-03T12:00:00Z"),
    );
    event.action = "comment_created".into();
    event.kind = tower_trackers::webhook::WebhookEventKind::IssueComment;
    event.issue.comment = Some(tower_trackers::webhook::CommentSnapshot {
        id: "20002".into(),
        body: "a comment".into(),
        author_is_bot: false,
    });
    let existing = job(Status::InProgress, Tracker::Jira, "ENG-7", None);
    assert!(matches!(
        tower_trackers::sync::inbound_decision(&event, &binding, Some(&existing)),
        tower_trackers::sync::InboundDecision::Update { .. }
    ));
}

/// The close arm is evaluated before `Changed`: a comment event that also
/// reports a close takes the close. On JIRA a comment event carries the
/// issue's status, so a comment on a Done issue closes its live job.
#[test]
fn inbound_close_is_checked_before_changed() {
    let binding = binding(Provider::Jira, "otto-factory");
    let mut event = jira_issue_event("Done", &["otto-factory"], Some("2026-09-03T12:00:00Z"));
    event.action = "comment_created".into();
    event.kind = tower_trackers::webhook::WebhookEventKind::IssueComment;
    let existing = job(Status::InProgress, Tracker::Jira, "ENG-7", None);
    assert!(matches!(
        tower_trackers::sync::inbound_decision(&event, &binding, Some(&existing)),
        tower_trackers::sync::InboundDecision::Close {
            to: Status::Completed,
            ..
        }
    ));
}

/// A GitHub close is the issue's own `closed` action, never its state, so a
/// human comment on an already-closed issue whose job is still live is a
/// change like any other (the close itself arrives, or arrived, as its own
/// `issues` event).
#[test]
fn inbound_github_comment_on_a_closed_issue_is_changed_not_a_close() {
    let binding = binding(Provider::Github, "otto-factory");
    let existing = job(Status::InProgress, Tracker::Github, "acme/api#17", None);
    let event = github_comment_event("created", "closed", Some("2026-09-03T12:00:00Z"), false);
    assert_eq!(
        tower_trackers::sync::inbound_decision(&event, &binding, Some(&existing)),
        tower_trackers::sync::InboundDecision::Changed {
            at: at("2026-09-03T12:00:00Z"),
        }
    );
}
