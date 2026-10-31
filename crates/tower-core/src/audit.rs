//! Audit actions that belong to the factory's own domain.
//!
//! The audit trail itself — `Entry`, `Db::audit`, `Tx::audit`, and the identity
//! and auth action names — is `savvagent_tenant::audit`. An action is a plain string,
//! so a service adds its own without touching the platform; these are the ones
//! Savvagent Tower writes. Import this module's `action` alongside
//! `savvagent_tenant::audit::action` under another name where both are needed.

pub mod action {
    // Resources.
    pub const REPO_REGISTERED: &str = "repo.registered";
    pub const REPO_UPDATED: &str = "repo.updated";
    pub const TRACKER_CONNECTED: &str = "tracker.connected";
    pub const TRACKER_DISCONNECTED: &str = "tracker.disconnected";
    pub const TRACKER_BOUND: &str = "tracker.repo.bound";
    pub const TRACKER_UNBOUND: &str = "tracker.repo.unbound";

    // Applied from the platform's lifecycle webhooks (tower-core::platform_events).
    /// A team was deleted at the platform; rows scoped to it stay scoped.
    pub const TEAM_SCOPE_ORPHANED: &str = "platform.team.deleted";
    /// A member was removed at the platform; their leases and claims were released.
    pub const MEMBER_RELEASED: &str = "platform.member.removed";

    // Jobs.
    pub const JOB_CANCEL_REQUESTED: &str = "job.cancel.requested";
    pub const JOB_CANCELLED: &str = "job.cancelled";
}
