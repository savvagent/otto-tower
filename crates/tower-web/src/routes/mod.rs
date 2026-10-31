//! The console's handlers, grouped by what they act on.
//!
//! Every handler here is named once in [`crate::catalog`], which is what mounts
//! it and what documents it. A handler that is not in the catalog is not
//! reachable — deliberately, so that adding a route and describing it are the
//! same act.
//!
//! Identity — accounts, orgs, members, teams, SSO, tokens, usage — is not here:
//! it lives in the Savvagent platform.

pub mod audit;
pub mod auth;
pub mod jobs;
pub mod platform;
pub mod repos;
pub mod trackers;
pub mod webhooks;

/// Distinguish "field absent" from "field present and null".
///
/// serde collapses both into `None` for an `Option<T>`; wrapping the whole
/// deserialization in `Some` recovers the difference — absent stays `None`
/// because of `#[serde(default)]`, while an explicit `null` arrives as
/// `Some(None)`.
///
/// A repo's `teamId` needs the distinction and cannot fake it: it has to be
/// un-settable, or a team-scoped repo can never be made org-wide (which is also
/// how an admin releases a repo whose team the platform has deleted).
pub(crate) fn double_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    serde::Deserialize::deserialize(deserializer).map(Some)
}
