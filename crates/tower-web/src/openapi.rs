//! The OpenAPI 3.1 document, rendered from [`crate::catalog`].
//!
//! The same list the router is built from, read a second way. A route that
//! exists is described, because describing it and mounting it are the same
//! declaration — see the catalog's module docs for why that is the shape chosen.
//!
//! Component schemas are hand-written here rather than derived from the Rust
//! types. That keeps OpenAPI's vocabulary out of `tower-core`, which describes
//! itself to MCP clients through `schemars` already and has no business
//! carrying a second documentation dependency for a surface it does not serve.

use std::sync::{Arc, OnceLock};

use axum::extract::State;
use axum::Json;
use serde_json::{json, Value};

use crate::catalog::{catalog, Auth, Endpoint};
use crate::state::AppState;

/// `GET /api/openapi.json`.
pub async fn serve(State(_state): State<AppState>) -> Json<Value> {
    static DOC: OnceLock<Arc<Value>> = OnceLock::new();
    let doc = DOC.get_or_init(|| Arc::new(document(&catalog())));
    Json((**doc).clone())
}

pub fn document(endpoints: &[Endpoint]) -> Value {
    let mut paths = serde_json::Map::new();

    for endpoint in endpoints {
        let entry = paths
            .entry(endpoint.path.to_string())
            .or_insert_with(|| json!({}));

        let mut operation = json!({
            "operationId": endpoint.operation_id(),
            "summary": endpoint.summary,
            "description": endpoint.description,
            "tags": [tag_for(endpoint.path)],
            "responses": responses(endpoint),
        });

        let params = endpoint
            .path_params()
            .iter()
            .map(|name| {
                json!({
                    "name": name,
                    "in": "path",
                    "required": true,
                    "description": param_description(name),
                    "schema": { "type": "string" },
                })
            })
            .collect::<Vec<_>>();

        if !params.is_empty() {
            operation["parameters"] = json!(params);
        }

        if let Some(schema) = endpoint.request {
            operation["requestBody"] = json!({
                "required": true,
                "content": { "application/json": { "schema": reference(schema) } },
            });
        }

        if endpoint.auth != Auth::Public {
            operation["security"] = json!([{ "platformBearer": [] }, { "consoleSession": [] }]);
        }
        operation["x-otto-factory-auth"] = json!(endpoint.auth.as_str());

        entry[endpoint.verb.as_str()] = operation;
    }

    json!({
        "openapi": "3.1.0",
        "info": {
            "title": "Savvagent Tower console API",
            "version": env!("CARGO_PKG_VERSION"),
            "description":
                "The console's REST surface: repos, the queue, tracker connections, and \
                 this service's domain audit trail. Identity (sign-in, accounts, orgs, \
                 members, teams, SSO, tokens, usage) and the OAuth 2.1 authorization \
                 server live in the Savvagent platform, not here. Authentication is a platform \
                 access token, introspected on every request, presented either as a bearer \
                 token or, for the console in a browser, held server-side behind a session \
                 cookie obtained by signing in through the platform (`/auth/login`). A \
                 bearer token, when sent, decides the request. Writes made with the \
                 cookie must come from this site (`Origin`); bearer requests are exempt. \
                 The MCP surface uses the same tokens and is described by its own \
                 metadata document, `/.well-known/oauth-protected-resource`.",
        },
        "components": {
            "securitySchemes": {
                "platformBearer": {
                    "type": "http",
                    "scheme": "bearer",
                    "description":
                        "A token issued by the Savvagent platform for this resource server \
                         (OAuth access token or personal access token). Its org is fixed \
                         when it is issued; the `{org}` path segment must name that org.",
                },
                "consoleSession": {
                    "type": "apiKey",
                    "in": "cookie",
                    "name": "__Host-of_session",
                    "description":
                        "The console's session cookie, set by `/auth/callback` after a \
                         sign-in through the Savvagent platform. HttpOnly, Secure, SameSite=Lax. \
                         It is bound to one org: asking for another answers `401` with \
                         code `org_session_mismatch`, and the client signs in again with \
                         `org=<slug>`. Writes must carry this site's `Origin`.",
                },
            },
            "schemas": components(),
        },
        "paths": paths,
    })
}

fn reference(name: &str) -> Value {
    json!({ "$ref": format!("#/components/schemas/{name}") })
}

/// The response block. Every endpoint can fail the same four ways, and saying
/// so once here is what makes the document usable without reading the source.
fn responses(endpoint: &Endpoint) -> Value {
    let success = match (endpoint.verb, endpoint.response) {
        (_, Some(schema)) => json!({
            "description": "Success",
            "content": { "application/json": { "schema": reference(schema) } },
        }),
        (crate::catalog::Verb::Delete, None) | (_, None) => json!({ "description": "Success" }),
    };

    let error = json!({
        "description": "Failure",
        "content": { "application/json": { "schema": reference("Error") } },
    });

    let mut responses = json!({
        "400": error,
        "500": error,
    });
    responses[endpoint.success.to_string()] = success;

    if endpoint.auth != Auth::Public {
        responses["401"] = error.clone();
    }
    if endpoint.auth.needs_org() {
        // 404 rather than 403 for an org you are not in — see `OrgCtx`.
        responses["403"] = error.clone();
        responses["404"] = error;
    }

    responses
}

fn param_description(name: &str) -> &'static str {
    match name {
        "org" => "Org slug.",
        "team" => "Team slug.",
        "repo" => "Repo slug — the handle agents use.",
        "user" => "User id (UUID).",
        "id" => "Resource id (UUID).",
        _ => "",
    }
}

fn tag_for(path: &str) -> &'static str {
    if path.starts_with("/.well-known") {
        "discovery"
    } else if path.starts_with("/auth") || path == "/api/session" {
        "auth"
    } else if path.starts_with("/webhooks") {
        "trackers"
    } else if path.starts_with("/platform") {
        "platform"
    } else if path.contains("/repos") && path.contains("/tracker") {
        "trackers"
    } else if path.contains("/repos") {
        "repos"
    } else if path.contains("/trackers") {
        "trackers"
    } else if path.contains("/jobs") {
        "queue"
    } else if path.contains("/audit") {
        "audit"
    } else {
        "meta"
    }
}

/// The component schemas the catalog refers to by name.
///
/// Split across several functions because `serde_json::json!` is recursive and
/// one literal this size exhausts the macro recursion limit — a compile error,
/// not a runtime one, but a confusing enough compile error to be worth avoiding.
/// A new group of schemas goes in a new function for the same reason; adding
/// them to an existing one is how the limit gets hit again.
fn components() -> Value {
    let mut all = serde_json::Map::new();
    for group in [
        entity_schemas(),
        queue_schemas(),
        tracker_schemas(),
        response_schemas(),
        request_schemas(),
    ] {
        let Value::Object(map) = group else {
            unreachable!("each schema group is an object literal")
        };
        all.extend(map);
    }
    Value::Object(all)
}

/// Tracker connections and bindings — the console's half of Milestone 2.
///
/// Its own group rather than an addition to [`entity_schemas`]: that literal is
/// already at the `json!` recursion limit, and these carry both entities and
/// request bodies anyway.
fn tracker_schemas() -> Value {
    let uuid = json!({ "type": "string", "format": "uuid" });
    let timestamp = json!({ "type": "string", "format": "date-time" });
    let provider = json!({ "type": "string", "enum": ["github", "jira"] });

    let tracker_connection = json!({
        "type": "object",
        "description":
            "One org's connection to a tracker. Stored credentials are never returned — \
             `hasCredentials` says only whether any exist.",
        "properties": {
            "id": uuid,
            "provider": provider,
            "externalId": {
                "type": "string",
                "description":
                    "GitHub: the App installation id. JIRA: the cloud site id. Neither is \
                     secret; both are globally unique across orgs, so connecting one \
                     another org already holds is refused.",
            },
            "hasCredentials": { "type": "boolean" },
            "createdAt": timestamp,
            "updatedAt": timestamp,
        },
        "required": ["id", "provider", "externalId", "hasCredentials", "createdAt", "updatedAt"],
    });

    let provider_setup = json!({
        "type": "object",
        "description":
            "What this deployment can take an admin through for one provider. \
             `configured` is the conjunction of every credential the flow needs, so a \
             console never offers a flow the server cannot finish.",
        "properties": {
            "configured": { "type": "boolean" },
            "startUrl": {
                "type": ["string", "null"],
                "description":
                    "Where to send the browser to begin, minus its `state`. Null whenever \
                     `configured` is false.",
            },
        },
        "required": ["configured"],
    });

    let tracker_binding = json!({
        "type": "object",
        "description": "Which external project or repository one repo's jobs map to.",
        "properties": {
            "id": uuid,
            "repoId": uuid,
            "provider": provider,
            "externalRef": {
                "type": "string",
                "description":
                    "GitHub: \"owner/repo\", matched against repository.full_name. JIRA: a \
                     project key, matched against fields.project.key.",
            },
            "triggerLabel": {
                "type": "string",
                "description": "The label inbound sync watches for. Defaults to otto-tower.",
            },
            "live": {
                "type": "boolean",
                "description":
                    "False while the org has no connection for this provider. The binding \
                     is stored and inert rather than refused.",
            },
            "createdAt": timestamp,
            "updatedAt": timestamp,
        },
        "required": [
            "id",
            "repoId",
            "provider",
            "externalRef",
            "triggerLabel",
            "live",
            "createdAt",
            "updatedAt",
        ],
    });

    json!({
        "TrackerConnection": tracker_connection,
        "ProviderSetup": provider_setup,
        "TrackerConnections": {
            "type": "object",
            "properties": {
                "connections": { "type": "array", "items": reference("TrackerConnection") },
                "github": reference("ProviderSetup"),
                "jira": reference("ProviderSetup"),
            },
            "required": ["connections", "github", "jira"],
        },
        "TrackerBinding": tracker_binding,
        "TrackerBindingList": { "type": "array", "items": reference("TrackerBinding") },
        "ConnectTrackerRequest": {
            "type": "object",
            "description":
                "The single-use artifact the provider handed the browser. A POST body and \
                 never a query string: a link preview following the provider's redirect \
                 must burn nothing.",
            "properties": {
                "code": {
                    "type": "string",
                    "description": "The provider's authorization code. Redeemable once.",
                },
                "installationId": {
                    "type": ["integer", "null"],
                    "description":
                        "GitHub only, and required there. Verified against the \
                         installations the authorizing account administers before \
                         anything is written.",
                },
            },
            "required": ["code"],
        },
        "BindRepoRequest": {
            "type": "object",
            "properties": {
                "externalRef": {
                    "type": "string",
                    "description":
                        "\"owner/repo\" for GitHub, a project key for JIRA. Anything else \
                         is refused rather than stored inert.",
                },
                "triggerLabel": {
                    "type": ["string", "null"],
                    "description": "Defaults to otto-tower when absent or blank.",
                },
            },
            "required": ["externalRef"],
        },
    })
}

fn entity_schemas() -> Value {
    let timestamp = json!({ "type": "string", "format": "date-time" });
    let uuid = json!({ "type": "string", "format": "uuid" });

    let repo = json!({
        "type": "object",
        "properties": {
            "id": uuid,
            "orgId": uuid,
            "slug": { "type": "string", "description": "The handle agents use." },
            "name": { "type": "string" },
            "provider": { "type": "string", "enum": ["github", "gitlab", "bitbucket", "other"] },
            "defaultBranch": { "type": "string" },
            "teamId": { "type": ["string", "null"], "format": "uuid", "description": "A team id from the Savvagent platform. Checked against the platform before it is stored; an unknown team is refused, never read as org-wide." },
            "defaultAgentType": { "type": ["string", "null"] },
            "trackerBinding": {
                "type": "object",
                "deprecated": true,
                "description":
                    "The free-form binding blob from Milestone 1. Nothing reads it: \
                     webhook ingest and the sync engine read the structured rows under \
                     /api/orgs/{org}/repos/{repo}/tracker-bindings instead. Not writable \
                     through this API — it is returned only because the field still \
                     exists on the row.",
            },
            "active": { "type": "boolean" },
            "createdAt": timestamp,
            "createdBy": { "type": ["string", "null"], "format": "uuid" },
        },
        "required": ["id", "orgId", "slug", "name", "provider", "defaultBranch", "active"],
    });

    json!({
        "Error": {
            "type": "object",
            "description":
                "Every failure. `code` is stable and safe to branch on; `message` is \
                 written to be shown to a person.",
            "properties": {
                "error": {
                    "type": "object",
                    "properties": {
                        "code": { "type": "string", "examples": ["not_found"] },
                        "message": { "type": "string" },
                    },
                    "required": ["code", "message"],
                },
            },
            "required": ["error"],
        },
        "Repo": repo,
        "RepoListItem": {
            "allOf": [
                reference("Repo"),
                {
                    "type": "object",
                    "properties": {
                        "hasActiveLease": {
                            "type": "boolean",
                            "description":
                                "Whether an unexpired lease is held on any resource in this \
                                 repo right now. Present only when the request set \
                                 `includeLeaseStatus=true`; omitted otherwise, since \
                                 computing it costs an extra org-wide read.",
                        },
                    },
                },
            ],
        },
        "RepoList": { "type": "array", "items": reference("RepoListItem") },
        "Lease": {
            "type": "object",
            "description":
                "An advisory, time-bounded claim on one resource of one repo — a \
                 branch, or anything else a team needs to serialize on. The server \
                 cannot enforce it against an operation it cannot see; it makes \
                 collisions visible rather than impossible.",
            "properties": {
                "id": uuid,
                "repoId": uuid,
                "resource": { "type": "string" },
                "holderUserId": uuid,
                "holderLabel": { "type": ["string", "null"] },
                "jobId": { "type": ["string", "null"] },
                "acquiredAt": timestamp,
                "renewedAt": timestamp,
                "expiresAt": timestamp,
            },
            "required": ["id", "repoId", "resource", "holderUserId", "expiresAt"],
        },
        "LeaseList": { "type": "array", "items": reference("Lease") },
        "AuditEvent": {
            "type": "object",
            "description": "One entry in this service's domain audit trail (repo, tracker, \
                            and job events). Identity events are the platform's.",
            "properties": {
                "id": { "type": "integer" },
                "orgId": { "type": ["string", "null"], "format": "uuid" },
                "actorUserId": { "type": ["string", "null"], "format": "uuid" },
                "actorLabel": { "type": ["string", "null"] },
                "action": { "type": "string", "examples": ["repo.registered"] },
                "targetType": { "type": ["string", "null"] },
                "targetId": { "type": ["string", "null"] },
                "ip": { "type": ["string", "null"] },
                "userAgent": { "type": ["string", "null"] },
                "detail": { "type": "object" },
                "createdAt": timestamp,
            },
            "required": ["id", "action", "createdAt"],
        },
        "AuditEventList": { "type": "array", "items": reference("AuditEvent") },
    })
}

/// The queue's schemas.
///
/// A separate group only because `serde_json::json!` expands recursively and
/// the entity literal had grown past the macro's recursion limit. Splitting is
/// the honest fix; raising `recursion_limit` crate-wide to keep one big literal
/// would hide the next one.
fn queue_schemas() -> Value {
    let timestamp = json!({ "type": "string", "format": "date-time" });
    let uuid = json!({ "type": "string", "format": "uuid" });

    json!({
        "Job": {
            "type": "object",
            "description":
                "One unit of coordinated work, always anchored to a repo. `metadata` \
                 is opaque — Savvagent Tower never reads it; it is where a customer's own \
                 skill keeps whatever its methodology needs.",
            "properties": {
                "id": { "type": "string", "examples": ["job-42"] },
                "orgId": uuid,
                "repoId": uuid,
                "teamId": { "type": ["string", "null"], "format": "uuid", "description": "A team id from the Savvagent platform. Checked against the platform before it is stored; an unknown team is refused, never read as org-wide." },
                "title": { "type": "string" },
                "description": { "type": ["string", "null"] },
                "status": {
                    "type": "string",
                    "enum": [
                        "pending", "in-progress", "active", "completed", "failed", "cancelled",
                    ],
                },
                "ticketRef": { "type": ["string", "null"], "examples": ["ACME-17"] },
                "tracker": { "type": ["string", "null"], "enum": ["jira", "github", null] },
                "agentType": { "type": ["string", "null"] },
                // Deliberately unconstrained. The column is JSONB and the server
                // never reads it, so anything a client can serialise is a legal
                // value — `{"type": "object"}` would promise a shape nothing
                // enforces, and generated clients would then reject their own
                // data. `true` is JSON Schema's "any value".
                "metadata": true,
                "createdAt": timestamp,
                "startedAt": { "type": ["string", "null"], "format": "date-time" },
                "completedAt": { "type": ["string", "null"], "format": "date-time" },
                "attempts": { "type": "integer" },
                "result": { "type": ["string", "null"] },
                "error": { "type": ["string", "null"] },
                "createdBy": { "type": ["string", "null"], "format": "uuid" },
                "claimedBy": { "type": ["string", "null"], "format": "uuid" },
                "claimedByLabel": { "type": ["string", "null"] },
                "claimExpiresAt": { "type": ["string", "null"], "format": "date-time" },
                "cancelRequestedAt": { "type": ["string", "null"], "format": "date-time" },
                "cancelRequestedBy": { "type": ["string", "null"], "format": "uuid" },
                "cancelReason": { "type": ["string", "null"] },
            },
            "required": ["id", "orgId", "repoId", "title", "status", "createdAt", "attempts"],
        },
        "JobList": { "type": "array", "items": reference("Job") },
        "JobDetail": {
            "allOf": [
                reference("Job"),
                {
                    "type": "object",
                    "properties": {
                        "dependsOn": {
                            "type": "array",
                            "items": { "type": "string", "examples": ["job-41"] },
                            "description":
                                "Job ids that must reach `completed` before this one \
                                 is claimable.",
                        },
                    },
                    "required": ["dependsOn"],
                },
            ],
        },
        "QueueStats": {
            "type": "object",
            "description":
                "`blocked` overlaps `pending` rather than partitioning it: it counts \
                 the pending jobs still waiting on a dependency.",
            "properties": {
                "pending": { "type": "integer" },
                "inProgress": { "type": "integer" },
                "active": { "type": "integer" },
                "completed": { "type": "integer" },
                "failed": { "type": "integer" },
                "cancelled": { "type": "integer" },
                "blocked": { "type": "integer" },
                "total": { "type": "integer" },
            },
            "required": [
                "pending", "inProgress", "active", "completed", "failed", "cancelled", "blocked",
                "total",
            ],
        },
    })
}

/// Responses that are not entities.
fn response_schemas() -> Value {
    json!({
        "Session": {
            "type": "object",
            "description": "Who is signed in to the console, and in which org.",
            "properties": {
                "user": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string", "format": "uuid" },
                        "email": { "type": ["string", "null"] },
                        "name": { "type": ["string", "null"] },
                    },
                    "required": ["id"],
                },
                "org": {
                    "type": "object",
                    "properties": {
                        "id": { "type": "string", "format": "uuid" },
                        "slug": { "type": "string" },
                        "name": { "type": "string" },
                        "plan": { "type": "string" },
                    },
                    "required": ["id", "slug", "name", "plan"],
                },
                "role": { "type": "string", "enum": ["owner", "admin", "member"] },
                "scopes": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "What the platform granted this session.",
                },
                "platformUrl": {
                    "type": "string",
                    "description":
                        "The Savvagent platform's console, for members, teams, SSO, usage, \
                         tokens, and the org list.",
                },
            },
            "required": ["user", "org", "role", "scopes", "platformUrl"],
        },
        "PlatformEventAck": {
            "type": "object",
            "description":
                "Acknowledgement of a platform lifecycle webhook. Any 2xx stops the \
                 platform's retries.",
            "properties": {
                "result": { "type": "string", "enum": ["applied", "duplicate", "ignored"] },
                "detail": {
                    "type": "object",
                    "description": "What was done, as counts. Present when `result` is `applied`.",
                },
            },
            "required": ["result"],
        },
    })
}

/// Request bodies.
fn request_schemas() -> Value {
    json!({
        "RegisterRepoRequest": {
            "type": "object",
            "properties": {
                "slug": {
                    "type": "string",
                    "minLength": 1,
                    "maxLength": tower_core::repo_slug::MAX_LEN,
                    "pattern": tower_core::repo_slug::PATTERN,
                    "description":
                        "The handle agents use. Starts with an ASCII letter or digit; the \
                         rest is ASCII letters, digits, `-`, `_`, `.` or `/`. No path \
                         segment (split on `/`) may be empty, `.` or `..`. Never trimmed. \
                         Refused as `invalid_repo_slug`.",
                },
                "name": { "type": ["string", "null"] },
                "remotes": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description":
                        "Any form git prints. Normalized, so SSH and HTTPS forms of \
                         one repository collapse to a single row.",
                },
                "provider": { "type": ["string", "null"] },
                "defaultBranch": { "type": ["string", "null"] },
                "teamId": { "type": ["string", "null"], "format": "uuid", "description": "A team id from the Savvagent platform. Checked against the platform before it is stored; an unknown team is refused, never read as org-wide." },
                "defaultAgentType": {
                    "type": ["string", "null"],
                    "description": "A free-form hint. Never validated against a list.",
                },
            },
            "required": ["slug"],
        },
        "UpdateRepoRequest": {
            "type": "object",
            "description":
                "Absent fields are left alone. The slug is deliberately absent: agents \
                 name it, so renaming would break every skill that does.",
            "properties": {
                "name": { "type": ["string", "null"] },
                "defaultBranch": { "type": ["string", "null"] },
                "teamId": { "type": ["string", "null"], "format": "uuid", "description": "A team id from the Savvagent platform. Checked against the platform before it is stored; an unknown team is refused, never read as org-wide." },
                "defaultAgentType": { "type": ["string", "null"] },
                "active": { "type": ["boolean", "null"] },
                "addRemotes": { "type": "array", "items": { "type": "string" } },
            },
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc() -> Value {
        document(&catalog())
    }

    #[test]
    fn register_repo_slug_carries_the_slug_rule() {
        let doc = doc();
        let slug = &doc["components"]["schemas"]["RegisterRepoRequest"]["properties"]["slug"];
        assert_eq!(slug["minLength"], 1);
        assert_eq!(slug["maxLength"], tower_core::repo_slug::MAX_LEN);
        assert_eq!(slug["pattern"], tower_core::repo_slug::PATTERN);
        assert!(slug["description"].as_str().is_some_and(|d| !d.is_empty()));
    }

    /// The drift test. Every schema the catalog names has to exist, or a
    /// generated client gets a dangling `$ref` and fails to build.
    #[test]
    fn every_referenced_schema_is_defined() {
        let doc = doc();
        let schemas = doc["components"]["schemas"].as_object().unwrap();

        let mut refs = Vec::new();
        collect_refs(&doc, &mut refs);

        for name in refs {
            assert!(
                schemas.contains_key(&name),
                "the document references #/components/schemas/{name}, which is not defined"
            );
        }
    }

    /// The inverse of the drift test above. `Enrollment`, `SignupRequest`,
    /// `LoginRequest` and `ConfirmTotpRequest` all outlived the TOTP-era
    /// flows that returned or took them, as dead documentation nothing
    /// caught — a hand-maintained document has no compiler to notice an
    /// endpoint stopped existing. A schema no `$ref` anywhere in the
    /// document names is exactly that: describing a shape the server no
    /// longer sends or accepts.
    #[test]
    fn every_defined_schema_is_referenced() {
        let doc = doc();
        let schemas = doc["components"]["schemas"].as_object().unwrap();

        let mut refs = Vec::new();
        collect_refs(&doc, &mut refs);
        let refs: std::collections::HashSet<_> = refs.into_iter().collect();

        for name in schemas.keys() {
            assert!(
                refs.contains(name),
                "components/schemas/{name} is defined but nothing $refs it \
                 anywhere in the document"
            );
        }
    }

    fn collect_refs(value: &Value, out: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                for (key, value) in map {
                    if key == "$ref" {
                        if let Some(name) = value.as_str().and_then(|r| {
                            r.strip_prefix("#/components/schemas/").map(str::to_string)
                        }) {
                            out.push(name);
                        }
                    }
                    collect_refs(value, out);
                }
            }
            Value::Array(items) => items.iter().for_each(|v| collect_refs(v, out)),
            _ => {}
        }
    }

    /// This is a hand-maintained JSON literal, not generated from
    /// `tower_core::jobs::Status`/`Stats` — nothing else catches it drifting out
    /// of sync with a status the server actually returns.
    #[test]
    fn the_job_schema_and_queue_stats_know_about_active() {
        let doc = doc();
        let schemas = &doc["components"]["schemas"];

        let status_enum = schemas["Job"]["properties"]["status"]["enum"]
            .as_array()
            .expect("Job.status has no enum array");
        assert!(
            status_enum.iter().any(|v| v == "active"),
            "Job.status.enum is missing \"active\": {status_enum:?}"
        );

        let stats_props = schemas["QueueStats"]["properties"]
            .as_object()
            .expect("QueueStats has no properties object");
        assert!(
            stats_props.contains_key("active"),
            "QueueStats.properties is missing \"active\""
        );

        let stats_required = schemas["QueueStats"]["required"]
            .as_array()
            .expect("QueueStats has no required array");
        assert!(
            stats_required.iter().any(|v| v == "active"),
            "QueueStats.required is missing \"active\": {stats_required:?}"
        );
    }

    /// This is a hand-maintained JSON literal, not generated from
    /// `tower_core::jobs::Status`/`Job`/`Stats` — nothing else catches it drifting
    /// out of sync with a status or field the server actually returns.
    #[test]
    fn the_job_schema_and_queue_stats_know_about_cancelled() {
        let doc = doc();
        let schemas = &doc["components"]["schemas"];

        let status_enum = schemas["Job"]["properties"]["status"]["enum"]
            .as_array()
            .expect("Job.status has no enum array");
        assert!(
            status_enum.iter().any(|v| v == "cancelled"),
            "Job.status.enum is missing \"cancelled\": {status_enum:?}"
        );

        let job_props = schemas["Job"]["properties"]
            .as_object()
            .expect("Job has no properties object");
        for field in [
            "cancelRequestedAt",
            "cancelRequestedBy",
            "cancelReason",
            "claimExpiresAt",
        ] {
            assert!(
                job_props.contains_key(field),
                "Job.properties is missing {field:?}"
            );
        }

        let stats_props = schemas["QueueStats"]["properties"]
            .as_object()
            .expect("QueueStats has no properties object");
        assert!(
            stats_props.contains_key("cancelled"),
            "QueueStats.properties is missing \"cancelled\""
        );

        let stats_required = schemas["QueueStats"]["required"]
            .as_array()
            .expect("QueueStats has no required array");
        assert!(
            stats_required.iter().any(|v| v == "cancelled"),
            "QueueStats.required is missing \"cancelled\": {stats_required:?}"
        );
    }

    /// An endpoint with no summary is an endpoint nobody outside this repo can
    /// use. Same rule as `tower-mcp`'s tool descriptions, for the same reason.
    #[test]
    fn every_endpoint_describes_itself() {
        for endpoint in catalog() {
            assert!(
                !endpoint.summary.is_empty(),
                "{} {} has no summary",
                endpoint.verb.as_str(),
                endpoint.path
            );
        }
    }

    /// An endpoint that claims an org scope but has no `{org}` segment cannot
    /// resolve one — `OrgCtx` would fail at runtime with an internal error.
    /// This is the wiring bug that check exists to report.
    #[test]
    fn org_scoped_endpoints_sit_under_an_org_segment() {
        for endpoint in catalog() {
            if endpoint.auth.needs_org() {
                assert!(
                    endpoint.path_params().contains(&"org"),
                    "{} {} claims {} but has no {{org}} segment",
                    endpoint.verb.as_str(),
                    endpoint.path,
                    endpoint.auth.as_str()
                );
            }
        }
    }

    /// Operation ids become method names in generated clients, so a duplicate
    /// silently overwrites a method.
    #[test]
    fn operation_ids_are_unique() {
        let mut ids: Vec<String> = catalog().iter().map(|e| e.operation_id()).collect();
        ids.sort();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "duplicate operationId in the catalog");
    }

    /// Webhook receivers are `POST`s that authenticate by signature, and the
    /// identity surface that used to share this file is gone from it for good.
    /// Re-adding any of these here means somebody has quietly rebuilt the
    /// platform inside a resource server.
    #[test]
    fn the_identity_surface_is_not_served_here() {
        for endpoint in catalog() {
            for prefix in ["/oauth", "/sso", "/api/auth", "/api/me"] {
                assert!(
                    !endpoint.path.starts_with(prefix),
                    "{} is identity surface and belongs to the platform",
                    endpoint.path
                );
            }
            for segment in [
                "/members", "/invites", "/teams", "/sso", "/tokens", "/usage",
            ] {
                assert!(
                    !endpoint.path.contains(segment),
                    "{} is identity surface and belongs to the platform",
                    endpoint.path
                );
            }
        }
        assert!(
            !catalog()
                .iter()
                .any(|e| e.path == "/.well-known/oauth-authorization-server"),
            "this service is not an authorization server"
        );
    }

    #[test]
    fn the_platform_webhook_is_a_post() {
        let endpoints = catalog();
        let hooks: Vec<_> = endpoints
            .iter()
            .filter(|e| e.path == "/platform/webhooks")
            .collect();
        assert_eq!(hooks.len(), 1, "the platform webhook is not mounted once");
        assert_eq!(hooks[0].verb, crate::catalog::Verb::Post);
        assert_eq!(hooks[0].auth, Auth::Public, "authenticated by signature");
    }

    /// The console watches the queue; it does not drive it. Every write to a
    /// job — enqueue, claim, complete, fail, repend — belongs to the MCP
    /// surface, because the agent doing the work is the only party that can say
    /// when it is done. A "mark complete" button here would let a human tell
    /// the queue something they cannot observe, and the audit trail would
    /// record it as fact.
    #[test]
    fn the_queue_is_read_only_over_the_console() {
        for endpoint in catalog() {
            if endpoint.path.contains("/jobs") {
                assert_eq!(
                    endpoint.verb,
                    crate::catalog::Verb::Get,
                    "{} {} writes to the queue from the console",
                    endpoint.verb.as_str(),
                    endpoint.path
                );
            }
        }
    }

    #[test]
    fn the_document_is_openapi_31_with_a_bearer_scheme() {
        let doc = doc();
        assert_eq!(doc["openapi"], "3.1.0");
        assert_eq!(
            doc["components"]["securitySchemes"]["platformBearer"]["scheme"],
            "bearer"
        );
        assert!(doc["paths"]["/api/orgs/{org}/repos"]["get"]["security"].is_array());
        assert!(
            doc["paths"]["/platform/webhooks"]["post"]
                .get("security")
                .is_none(),
            "a public endpoint must not require a token"
        );
    }

    /// This schema is hand-written, not derived from `tower_core::leases::Lease`,
    /// so a field rename on the Rust side has no compiler to catch it here —
    /// this test is the only thing that would have caught `Lease.branch`
    /// surviving in the published document after the struct's field was
    /// renamed to `resource`.
    #[test]
    fn the_lease_schema_matches_the_wire_field_it_actually_returns() {
        let doc = doc();
        let lease = &doc["components"]["schemas"]["Lease"];
        assert!(
            lease["properties"]["resource"].is_object(),
            "Lease schema is missing a resource property: {lease}"
        );
        assert!(
            lease["properties"].get("branch").is_none(),
            "Lease schema still advertises the retired branch field: {lease}"
        );
        assert!(
            lease["required"]
                .as_array()
                .unwrap()
                .contains(&serde_json::json!("resource")),
            "resource is required on every Lease this route returns: {lease}"
        );
    }

    #[test]
    fn the_repo_list_item_schema_advertises_has_active_lease() {
        let doc = doc();
        let repo_list_item = &doc["components"]["schemas"]["RepoListItem"];
        let extension = &repo_list_item["allOf"][1];
        assert_eq!(
            extension["properties"]["hasActiveLease"]["type"], "boolean",
            "RepoListItem schema is missing a boolean hasActiveLease property: {repo_list_item}"
        );
        // Not required: the handler omits it entirely unless the caller asked
        // for it via `?includeLeaseStatus=true`, since computing it costs an
        // extra org-wide read that most callers of this endpoint don't need.
        assert!(
            extension["required"].is_null(),
            "hasActiveLease is opt-in, not required, on RepoListItem: {repo_list_item}"
        );
    }

    /// One path serving several methods must render as one entry with several
    /// operations, not as the last one written.
    #[test]
    fn a_path_with_several_methods_keeps_all_of_them() {
        let doc = doc();
        let repos = &doc["paths"]["/api/orgs/{org}/repos"];
        assert!(repos["get"].is_object(), "GET was lost");
        assert!(repos["post"].is_object(), "POST was lost");
    }
}
