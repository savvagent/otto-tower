//! The scopes Savvagent Tower's resource server understands.
//!
//! Scope strings are only meaningful within one resource server. The otto
//! platform (the authorization server) validates every authorize, code
//! redemption, refresh, and PAT against the scope list registered for this
//! resource, and this service checks them again in every tool. The registration
//! is an operator step (`otto-platform-server resource register --scopes …`,
//! see docs/deploy/fly.md); this module is the single place Savvagent Tower says
//! what it understands, so the registration, the advertised metadata
//! (`/.well-known/oauth-protected-resource`), and the tools' own scope checks
//! cannot drift apart. The `scopes_supported` document is generated from
//! [`KNOWN`].
//!
//! An unknown scope is rejected rather than ignored — silently dropping a
//! requested scope gives a client a token that does less than it believes,
//! which fails later and confusingly.

/// What this resource server is called on a consent screen and in its metadata.
pub const RESOURCE_NAME: &str = "Savvagent Tower";

pub const JOBS_READ: &str = "jobs:read";
pub const JOBS_WRITE: &str = "jobs:write";
pub const REPOS_READ: &str = "repos:read";
pub const REPOS_WRITE: &str = "repos:write";
pub const MESSAGES: &str = "messages";
pub const TRACKERS: &str = "trackers";
pub const ORG_ADMIN: &str = "org:admin";

/// Every scope this server understands, and the list to register with the platform.
pub const KNOWN: &[&str] = &[
    JOBS_READ,
    JOBS_WRITE,
    REPOS_READ,
    REPOS_WRITE,
    MESSAGES,
    TRACKERS,
    ORG_ADMIN,
];

/// What to register as the default scopes: granted when a client asks for
/// nothing in particular. Read-only: a client
/// that wants to change anything has to say so, and the user has to see it on
/// the consent screen.
pub const DEFAULT: &[&str] = &[JOBS_READ, REPOS_READ];

#[cfg(test)]
mod tests {
    use super::*;

    /// These are the strings existing tokens and registered clients already
    /// carry. Changing one is a breaking change to every issued grant.
    #[test]
    fn the_scope_list_is_the_one_tokens_already_carry() {
        assert_eq!(
            KNOWN,
            [
                "jobs:read",
                "jobs:write",
                "repos:read",
                "repos:write",
                "messages",
                "trackers",
                "org:admin"
            ]
        );
        assert_eq!(DEFAULT, ["jobs:read", "repos:read"]);
    }

    #[test]
    fn the_defaults_are_read_only_and_known() {
        for s in DEFAULT {
            assert!(KNOWN.contains(s));
            assert!(!s.ends_with(":write") && *s != ORG_ADMIN);
        }
    }
}
