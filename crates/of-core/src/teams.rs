//! Team scoping, enforced against the platform.
//!
//! Teams are identity, so they live in the platform's database and this one
//! cannot hold a foreign key to them. What this database stores is an opaque
//! `team_id` on repos, jobs, and messages, where **a null `team_id` means
//! org-wide**. That makes the way a team id gets onto a row the dangerous part:
//! an id that was never checked (a typo, another tenant's team, a team that has
//! since been deleted) must never be written, and a failure to find out must
//! never be read as "no team".
//!
//! [`VerifiedTeam`] is how that is enforced rather than remembered. `NewRepo`,
//! `RepoPatch`, and `NewJob` take one instead of a bare [`TeamId`], and the only
//! ways to build one are [`VerifiedTeam::verify`] (by id) and
//! [`VerifiedTeam::by_slug`] (by slug), which ask the platform whether the team
//! exists in the caller's org and **fail closed**:
//!
//! - the platform says the team is not in this org → [`Error::TeamNotFound`];
//! - the platform cannot be reached, or answers with anything unexpected →
//!   [`Error::Platform`]. Never "treat it as org-wide", never "assume it's fine".
//!
//! The reverse direction (a team deleted *after* repos were scoped to it) is
//! handled by the platform's `team.deleted` webhook; see [`crate::platform_events`].

use std::collections::HashSet;

use crate::error::{Error, Result};
use otto_resource::{PlatformClient, Role};
use otto_tenant::ids::{OrgId, TeamId, UserId};

/// The longest team slug [`VerifiedTeam::by_slug`] will send to the platform, in
/// bytes.
pub const TEAM_SLUG_MAX: usize = 128;

/// Whether `slug` (already trimmed) could name a team at all. The slug goes
/// into a URL path at the platform, so a separator, a dot segment, URL syntax
/// (`%`, `?`, `#`), or a control character is refused here rather than left to
/// the client's percent-encoding, whatever the platform would make of it.
pub fn team_slug_shape_ok(slug: &str) -> bool {
    !slug.is_empty()
        && slug.len() <= TEAM_SLUG_MAX
        && slug != "."
        && slug != ".."
        && !slug.chars().any(|c| {
            matches!(c, '/' | '\\' | '%' | '?' | '#') || c.is_whitespace() || c.is_control()
        })
}

/// A team id the platform confirmed exists in a particular org.
///
/// Not `Deserialize`, and the fields are private: there is no way to conjure
/// one from untrusted input except by asking the platform.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifiedTeam {
    org: OrgId,
    team: TeamId,
}

impl VerifiedTeam {
    /// Ask the platform whether `team` exists in `org`.
    ///
    /// ```no_run
    /// # async fn demo(platform: &otto_resource::PlatformClient, org: otto_tenant::OrgId,
    /// #     team: otto_tenant::TeamId) -> of_core::Result<()> {
    /// use of_core::teams::VerifiedTeam;
    ///
    /// // An unknown or deleted team is an error, never an org-wide repo.
    /// let team = VerifiedTeam::verify(platform, org, team).await?;
    /// # let _ = team; Ok(()) }
    /// ```
    pub async fn verify(platform: &PlatformClient, org: OrgId, team: TeamId) -> Result<Self> {
        match platform.team(org.as_uuid(), team.as_uuid()).await? {
            // The platform scopes the lookup by org already; checking the answer
            // too means a confused or compromised response cannot attach a
            // foreign team.
            Some(info) if info.org_id == org.as_uuid() && info.id == team.as_uuid() => {
                Ok(Self { org, team })
            }
            Some(info) => {
                // Still a refusal, but not an ordinary one: the platform
                // answered a lookup scoped to this org with another org's or
                // another id's team, which is a platform fault worth paging on.
                tracing::error!(
                    org = %org,
                    requested_team = %team,
                    returned_org = %info.org_id,
                    returned_team = %info.id,
                    "platform answered a team lookup with a team outside the requested org or id; \
                     refused as team_not_found"
                );
                Err(Error::TeamNotFound {
                    team: team.to_string(),
                })
            }
            None => Err(Error::TeamNotFound {
                team: team.to_string(),
            }),
        }
    }

    /// Ask the platform for the team with `slug` in `org`. Fails closed exactly as
    /// [`Self::verify`] does.
    ///
    /// ```no_run
    /// # async fn demo(platform: &otto_resource::PlatformClient, org: otto_tenant::OrgId)
    /// #     -> of_core::Result<()> {
    /// use of_core::teams::VerifiedTeam;
    ///
    /// // A slug the org does not have is an error, never an org-wide repo.
    /// let team = VerifiedTeam::by_slug(platform, org, "platform").await?;
    /// # let _ = team; Ok(()) }
    /// ```
    ///
    /// The slug is trimmed and shape-checked before anything is sent: one that
    /// no team could have (empty, over [`TEAM_SLUG_MAX`] bytes, `.` or `..`, or
    /// holding a `/`, whitespace, or a control character) is refused locally
    /// with the same [`Error::TeamNotFound`] an unknown slug gets, so it never
    /// reaches the platform's URL path.
    pub async fn by_slug(platform: &PlatformClient, org: OrgId, slug: &str) -> Result<Self> {
        let not_found = || Error::TeamNotFound {
            team: format!("{slug:?}"),
        };
        let trimmed = slug.trim();
        if !team_slug_shape_ok(trimmed) {
            return Err(not_found());
        }
        match platform.team_by_slug(org.as_uuid(), trimmed).await? {
            Some(info) if info.org_id == org.as_uuid() => Ok(Self {
                org,
                team: info.id.into(),
            }),
            Some(info) => {
                // See `verify`: a lookup scoped to this org answered with
                // another org's team.
                tracing::error!(
                    org = %org,
                    requested_slug = ?trimmed,
                    returned_org = %info.org_id,
                    "platform answered a team-by-slug lookup with another org's team; \
                     refused as team_not_found"
                );
                Err(not_found())
            }
            None => Err(not_found()),
        }
    }

    pub fn id(&self) -> TeamId {
        self.team
    }

    pub fn org(&self) -> OrgId {
        self.org
    }

    /// Refuse a team that was verified for a different org than the
    /// transaction's. A programming error, not a user one, but it is exactly the
    /// cross-tenant mix-up this type exists to make loud.
    pub(crate) fn in_org(self, org: OrgId) -> Result<TeamId> {
        if self.org == org {
            Ok(self.team)
        } else {
            Err(Error::TeamNotFound {
                team: self.team.to_string(),
            })
        }
    }
}

/// Which team-scoped rows a caller may see or touch.
///
/// A row with a null `team_id` is org-wide and open to every member; a row with
/// a `team_id` is open to that team's members and to org owners and admins.
/// Membership is the platform's, so [`TeamScope::for_member`] asks it, and
/// **fails closed**: a platform that cannot answer is an [`Error::Platform`],
/// never a guess and never [`TeamScope::All`]. A team id the platform does not
/// list (unknown, foreign, or deleted) is simply not matched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TeamScope {
    /// Owners and admins: every row.
    All,
    /// Everyone else: org-wide rows, plus rows of exactly these teams.
    Teams(HashSet<TeamId>),
}

impl TeamScope {
    /// The scope of `user`, a member of `org` with `role`.
    ///
    /// Administrators are [`TeamScope::All`] without a platform call. Anyone
    /// else costs one `member_teams` lookup (cached by the client for
    /// `MEMBER_TTL`). A user the platform no longer knows as a member of `org`
    /// is [`Error::AccessRevoked`], not a member of no teams: the token can
    /// outlive the membership by an introspection cache window, and "no teams"
    /// would still open every org-wide row to someone who has left.
    ///
    /// ```no_run
    /// # async fn demo(platform: &otto_resource::PlatformClient, org: otto_tenant::OrgId,
    /// #     user: otto_tenant::UserId) -> of_core::Result<()> {
    /// use of_core::teams::TeamScope;
    ///
    /// let scope = TeamScope::for_member(platform, org, user, otto_resource::Role::Member).await?;
    /// assert!(scope.allows(None)); // org-wide rows are everyone's
    /// # Ok(()) }
    /// ```
    pub async fn for_member(
        platform: &PlatformClient,
        org: OrgId,
        user: UserId,
        role: Role,
    ) -> Result<Self> {
        if role.can_administer() {
            return Ok(Self::All);
        }
        // `Some([])` is a member on no team; `None` is someone the platform
        // does not know in this org at all.
        let teams = platform
            .member_teams(org.as_uuid(), user.as_uuid())
            .await?
            .ok_or(Error::AccessRevoked)?;
        Ok(Self::Teams(
            teams
                .into_iter()
                // The platform scopes the lookup by org already; checking the
                // answer too means a confused response cannot widen the set.
                .filter(|t| t.org_id == org.as_uuid())
                .map(|t| TeamId::from(t.id))
                .collect(),
        ))
    }

    /// Whether a row with this `team_id` is visible.
    pub fn allows(&self, team: Option<TeamId>) -> bool {
        match (self, team) {
            (Self::All, _) | (_, None) => true,
            (Self::Teams(mine), Some(t)) => mine.contains(&t),
        }
    }

    /// The team ids to restrict a query to besides org-wide rows, or `None`
    /// when nothing is restricted.
    pub fn restriction(&self) -> Option<Vec<TeamId>> {
        match self {
            Self::All => None,
            Self::Teams(mine) => Some(mine.iter().copied().collect()),
        }
    }

    /// Whether this scope sees every row (an administrator's).
    pub fn is_all(&self) -> bool {
        matches!(self, Self::All)
    }

    /// Errors that name a row the caller never asked about (the job a ticket is
    /// already linked to, the repo a remote belongs to, the far end of a
    /// dependency chain) must not name one this scope cannot see. The message is
    /// withheld and the code kept, so a caller branching on [`Error::code`] is
    /// unaffected. Unrestricted callers get the error unchanged; for any
    /// team-limited scope these errors are withheld outright, without checking
    /// whether the named row is actually hidden from it.
    ///
    /// Every surface uses this one definition (see [`Error::Redacted`]), so the
    /// wording, and which errors are withheld, cannot drift between them.
    pub fn redact_foreign_ids(&self, e: Error) -> Error {
        if self.is_all() {
            return e;
        }
        let message = match &e {
            Error::TicketAlreadyLinked { .. } => {
                "that ticket_ref is already linked to another job in this repo; use a different \
                 ticket_ref, or ask an administrator which job holds it"
            }
            Error::RemoteTaken(..) => {
                "that remote is already registered to another repo; ask an administrator which"
            }
            Error::DependencyCycle(..) => {
                "that change would make a job depend on itself, directly or through a chain; \
                 drop the dependency that closes the loop"
            }
            _ => return e,
        };
        Error::Redacted {
            code: e.code(),
            message,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ids::JobId;

    #[test]
    fn team_slug_shape() {
        assert!(team_slug_shape_ok("platform"));
        assert!(team_slug_shape_ok("a.b-c_d"));
        assert!(team_slug_shape_ok(&"x".repeat(TEAM_SLUG_MAX)));
        for bad in [
            "", ".", "..", "a/b", "../x", "a b", "a\tb", "a\u{0}b", "a\u{7f}b", "a\u{a0}b",
        ] {
            assert!(!team_slug_shape_ok(bad), "{bad:?} should be refused");
        }
        assert!(!team_slug_shape_ok(&"x".repeat(TEAM_SLUG_MAX + 1)));
    }

    fn naming_errors() -> Vec<Error> {
        vec![
            Error::TicketAlreadyLinked {
                ticket_ref: "acme/web#7".into(),
                job: JobId::from_seq(41),
            },
            Error::RemoteTaken("github.com/acme/secret".into(), "secret-repo".into()),
            Error::DependencyCycle(JobId::from_seq(41), JobId::from_seq(42)),
        ]
    }

    #[test]
    fn an_unrestricted_scope_is_told_everything() {
        for e in naming_errors() {
            let before = e.to_string();
            let after = TeamScope::All.redact_foreign_ids(e);
            assert!(!matches!(after, Error::Redacted { .. }), "{after:?}");
            assert_eq!(after.to_string(), before);
        }
    }

    #[test]
    fn a_team_limited_scope_keeps_the_code_and_loses_the_names() {
        let scope = TeamScope::Teams(HashSet::from([TeamId::new()]));
        for e in naming_errors() {
            let code = e.code();
            let after = scope.redact_foreign_ids(e);
            assert!(matches!(after, Error::Redacted { .. }), "{after:?}");
            assert_eq!(after.code(), code);
            let text = after.to_string();
            for name in ["secret-repo", "github.com/acme/secret", "job-41", "job-42"] {
                assert!(!text.contains(name), "{name} leaked: {text}");
            }
        }
    }

    #[test]
    fn errors_that_name_nothing_foreign_pass_through() {
        let scope = TeamScope::Teams(HashSet::new());
        let after = scope.redact_foreign_ids(Error::RepoSlugTaken("mine".into()));
        assert!(matches!(after, Error::RepoSlugTaken(_)), "{after:?}");
    }
}
