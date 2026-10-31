//! Organization tools.
//!
//! `whoami` is the tool an agent should call first in a session, and it is the
//! answer to a question no other tool answers: *which* of a person's
//! organizations does this token open? A user may belong to several; a token
//! opens exactly one, fixed when it was issued and not selectable per call. An
//! agent that assumes otherwise queues work in the wrong company's queue.
//!
//! Both tools here are free, and deliberately so: a caller must never have to
//! spend an operation to find out how many it has left.

use rmcp::handler::server::tool::Extension;
use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::ErrorData;
use rmcp::{tool, tool_router};
use serde::Deserialize;

use super::out;
use crate::server::{Factory, McpResult};

#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct NoArgs {}

#[tool_router(router = org_router, vis = "pub(crate)")]
impl Factory {
    #[tool(
        name = "whoami",
        description = "Who this token says you are: your user, the one organization it opens, \
                       your role in it, the scopes it carries, and this month's usage: completed \
                       jobs billed on the organization's Savvagent Tower meter, its budget and any \
                       block (coordination calls are free). Call this first in a session — it \
                       is the only way to know which organization your work will land in, and \
                       which tools you are allowed to use."
    )]
    pub async fn whoami(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(_): Parameters<NoArgs>,
    ) -> Result<Json<out::WhoAmI>, ErrorData> {
        let caller = self.caller(&parts)?;

        // No scope check. A token that cannot ask what it is cannot report a
        // useful error either, and there is nothing here the holder of the
        // token does not already have.
        // Who and what the token says it is comes from the platform: the user
        // and org are its records, not ours. A `None` means the platform no
        // longer sees an active membership (removed since the token was last
        // introspected), and the identity fields are simply left empty.
        let member = self
            .platform()
            .member(caller.org_id.as_uuid(), caller.user_id.as_uuid())
            .await
            .mcp()?;

        // Every platform read comes before the meter, so a lookup that fails
        // leaves no record of a call that was never served.
        let usage = self.meter().report(caller.org_id).await.mcp()?;

        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "whoami").await?;
        tx.commit().await.mcp()?;

        let user = member.as_ref().map(|m| &m.user);
        let org = member.as_ref().map(|m| &m.org);
        Ok(Json(out::WhoAmI {
            service: of_core::scopes::RESOURCE_NAME,
            user: out::UserOut {
                id: caller.user_id,
                email: user.and_then(|u| u.email.clone()),
                name: user.and_then(|u| u.name.clone()),
            },
            org: out::OrgOut {
                id: caller.org_id,
                slug: org.map(|o| o.slug.clone()),
                name: org.map(|o| o.name.clone()),
                plan: org.map(|o| o.plan.clone()),
            },
            role: member.as_ref().map(|m| m.role.into()),
            token: out::TokenOut {
                kind: match caller.kind {
                    otto_resource::TokenKind::Oauth => "oauth",
                    otto_resource::TokenKind::Pat => "pat",
                },
                client_id: caller.client_id,
                scopes: caller.scopes,
                expires_at: caller.expires_at,
            },
            usage,
        }))
    }

    #[tool(
        name = "usage",
        description = "This organization's usage this month: `meters` lists the completed jobs \
                       billed on its Savvagent Tower meter, `budget` shows its spend limit (null \
                       if none is set) and `blockedReason` says why completing jobs is being \
                       refused, if it is. Coordination calls are free; only each completed job \
                       is billed, including one completed by closing its linked ticket. \
                       This tool is free and works even while blocked. If `budget` \
                       is nearly spent or `blockedReason` is set, tell the person you are \
                       working for — someone with billing access may need to act."
    )]
    pub async fn usage(
        &self,
        Extension(parts): Extension<http::request::Parts>,
        Parameters(_): Parameters<NoArgs>,
    ) -> Result<Json<out::UsageOut>, ErrorData> {
        let caller = self.caller(&parts)?;

        // Read first: a failed lookup must leave no record of a call that was
        // never served.
        let usage = self.meter().report(caller.org_id).await.mcp()?;

        let mut tx = self.tx(&caller).await?;
        self.charge(&mut tx, &caller, "usage").await?;
        tx.commit().await.mcp()?;

        Ok(Json(out::UsageOut { usage }))
    }
}
