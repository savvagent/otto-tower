//! Typed identifiers.
//!
//! These are newtypes rather than bare `Uuid`/`String` so that the compiler
//! rejects passing a `UserId` where an `OrgId` belongs. In a multi-tenant system
//! that mix-up is the single highest-consequence typo available, and it is
//! otherwise invisible: both are `Uuid`, both come from the same request, and
//! the query still runs.

use serde::{Deserialize, Serialize};
use std::fmt;
use uuid::Uuid;

macro_rules! uuid_id {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, sqlx::Type)]
        #[serde(transparent)]
        #[sqlx(transparent)]
        pub struct $name(pub Uuid);

        impl $name {
            pub fn new() -> Self {
                Self(Uuid::new_v4())
            }
            pub fn as_uuid(&self) -> Uuid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }

        impl From<Uuid> for $name {
            fn from(u: Uuid) -> Self {
                Self(u)
            }
        }

        impl std::str::FromStr for $name {
            type Err = uuid::Error;
            fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
                Ok(Self(Uuid::parse_str(s)?))
            }
        }

        // Written out rather than derived. A derive on a newtype produces a
        // schema named after the wrapper with the inner type nested inside;
        // over the wire these *are* UUID strings, and that is what an MCP
        // client's schema should say.
        impl schemars::JsonSchema for $name {
            fn schema_name() -> std::borrow::Cow<'static, str> {
                stringify!($name).into()
            }

            fn schema_id() -> std::borrow::Cow<'static, str> {
                concat!(module_path!(), "::", stringify!($name)).into()
            }

            fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
                schemars::json_schema!({
                    "type": "string",
                    "format": "uuid",
                    "description": $doc,
                })
            }

            fn inline_schema() -> bool {
                true
            }
        }
    };
}

uuid_id!(RepoId, "A registered repository within one org.");

/// A job identifier: `job-N`, unique **within an org**, drawn from that org's
/// own counter.
///
/// Not a UUID on purpose. Agents quote job ids to each other in messages and
/// humans read them in the console, so they have to be short and speakable. The
/// per-org counter means two orgs both have a `job-1` — which is exactly the
/// point: neither can enumerate the other's work by counting.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, sqlx::Type)]
#[serde(transparent)]
#[sqlx(transparent)]
pub struct JobId(pub String);

impl JobId {
    pub fn from_seq(seq: i64) -> Self {
        Self(format!("job-{seq}"))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for JobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for JobId {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for JobId {
    fn from(s: &str) -> Self {
        Self(s.to_string())
    }
}

/// `job-N`, not a UUID — so the schema says string with an example rather than
/// a format an agent would try to satisfy by generating one.
impl schemars::JsonSchema for JobId {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "JobId".into()
    }

    fn schema_id() -> std::borrow::Cow<'static, str> {
        concat!(module_path!(), "::JobId").into()
    }

    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({
            "type": "string",
            "description": "A job identifier, unique within one organization.",
            "examples": ["job-42"],
        })
    }

    fn inline_schema() -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_ids_are_per_org_sequential() {
        assert_eq!(JobId::from_seq(1).as_str(), "job-1");
        assert_eq!(JobId::from_seq(42).as_str(), "job-42");
    }
}
