//! Change notification: one `LISTEN` for the whole server, fanned out in process.
//!
//! Agents need to react to queue changes without polling, and `watch` is the
//! long-poll tool that lets them. The naive implementation — a Postgres channel
//! per org — does not survive multi-tenancy: channel names are identifiers, so
//! per-org channels mean a `LISTEN` per tenant on every connection, and the
//! `LISTEN` set has to be rewritten every time an org is created.
//!
//! Instead one connection listens on `of_changes`, every payload carries its
//! `org`, and this module dispatches to the waiters for that org. A payload for
//! an org with nobody waiting is dropped, which is the common case and costs
//! nothing.

use crate::error::Result;
use savvagent_tenant::ids::{OrgId, UserId};
use serde::Deserialize;
use sqlx::postgres::PgListener;
use sqlx::PgPool;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;

/// How many changes a slow waiter may fall behind before it is told to resync.
///
/// A lagged receiver is not an error here: `watch` only reports *that*
/// something changed, never what, so a waiter that missed events still does the
/// right thing by returning `Changed` and letting the agent refetch.
const CHANNEL_CAPACITY: usize = 64;

#[derive(Debug, Clone, Deserialize)]
pub struct Change {
    /// `job` | `lease` | `message`
    pub kind: String,
    pub org: OrgId,
    pub id: String,
    pub op: String,
    /// Present only on message changes: who wrote it.
    #[serde(default)]
    pub sender: Option<UserId>,
}

/// The result of a long poll.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Something the caller cares about changed. Refetch.
    Changed,
    /// The wait elapsed with nothing to report. Call again.
    Timeout,
}

pub struct Watcher {
    orgs: Mutex<HashMap<OrgId, broadcast::Sender<Change>>>,
    /// The listener task, so it can be stopped and its connection given back.
    ///
    /// `LISTEN` needs a connection all to itself, and `PgListener` takes one
    /// out of the pool and detaches it — dropping the pool does not reclaim it.
    /// Without a way to stop the task, that connection is held until the
    /// process exits: fine for a server that runs forever, wrong for graceful
    /// shutdown, and fatal in tests, where the throwaway database cannot be
    /// dropped while a session is still attached to it.
    listener: Mutex<Option<JoinHandle<()>>>,
}

impl Watcher {
    /// Start listening. The returned handle is cheap to clone into request
    /// handlers.
    ///
    /// The listener task reconnects on its own: `PgListener` re-establishes the
    /// connection and re-issues `LISTEN` after a drop. A gap in notifications
    /// costs a waiter one spurious `Timeout`, never a missed change permanently
    /// — agents refetch on every wake and on every timeout boundary.
    pub async fn spawn(pool: PgPool) -> Result<Arc<Self>> {
        let watcher = Arc::new(Self {
            orgs: Mutex::new(HashMap::new()),
            listener: Mutex::new(None),
        });

        let mut listener = PgListener::connect_with(&pool).await?;
        listener.listen("of_changes").await?;

        // A `Weak`, deliberately. The task holding a strong reference would be
        // a cycle — task keeps `Watcher` alive, `Watcher` holds the task's
        // handle — and the whole point of holding the handle is to be able to
        // stop the task when the last real handle goes away.
        let weak = Arc::downgrade(&watcher);
        let handle = tokio::spawn(async move {
            loop {
                match listener.recv().await {
                    Ok(note) => match serde_json::from_str::<Change>(note.payload()) {
                        Ok(change) => {
                            let Some(w) = Weak::upgrade(&weak) else {
                                // Nobody is watching any more.
                                break;
                            };
                            w.dispatch(change);
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, payload = note.payload(),
                                           "undecodable change notification");
                        }
                    },
                    Err(e) => {
                        // PgListener reconnects internally; log and keep going
                        // rather than tearing down every waiter on the server.
                        tracing::warn!(error = %e, "change listener error; continuing");
                    }
                }
            }
        });

        *watcher.listener.lock().expect("watcher registry poisoned") = Some(handle);
        Ok(watcher)
    }

    /// Stop listening and hand the connection back, waiting until it is gone.
    ///
    /// Call this on graceful shutdown, and at the end of any test that spawns a
    /// watcher: a `LISTEN` session still attached to a throwaway database stops
    /// it being dropped. [`Drop`] does the same thing on a best-effort basis,
    /// but only this version waits for the task to actually finish, which is
    /// what makes the connection's release observable rather than merely
    /// scheduled.
    pub async fn shutdown(&self) {
        let handle = self
            .listener
            .lock()
            .expect("watcher registry poisoned")
            .take();

        if let Some(handle) = handle {
            handle.abort();
            // `Err(Cancelled)` is the expected outcome and means the task has
            // been dropped, taking the listener and its connection with it.
            let _ = handle.await;
        }
    }

    fn dispatch(&self, change: Change) {
        let tx = {
            let orgs = self.orgs.lock().expect("watcher registry poisoned");
            orgs.get(&change.org).cloned()
        };
        // `send` fails only when no receivers remain, which is the normal
        // steady state for an org with no agent currently polling.
        if let Some(tx) = tx {
            let _ = tx.send(change);
        }
    }

    fn subscribe(&self, org: OrgId) -> broadcast::Receiver<Change> {
        let mut orgs = self.orgs.lock().expect("watcher registry poisoned");
        orgs.entry(org)
            .or_insert_with(|| broadcast::channel(CHANNEL_CAPACITY).0)
            .subscribe()
    }

    /// Wait up to `timeout` for a change in `org`.
    ///
    /// `caller` suppresses self-wakes: a message written by the caller is not a
    /// reason to wake the caller's own long poll. Without this, an agent that
    /// sends a coordination note immediately wakes itself and refetches an inbox
    /// it already knows the contents of — once per send, forever.
    ///
    /// Suppression applies only to `message` changes. A job or lease the caller
    /// changed still wakes them, because other tools may have changed it too and
    /// the payload does not carry enough to tell.
    pub async fn wait(
        &self,
        org: OrgId,
        caller: Option<UserId>,
        timeout: std::time::Duration,
    ) -> Outcome {
        let mut rx = self.subscribe(org);
        let deadline = tokio::time::Instant::now() + timeout;

        loop {
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Outcome::Timeout;
            }

            match tokio::time::timeout(remaining, rx.recv()).await {
                Err(_) => return Outcome::Timeout,
                Ok(Ok(change)) => {
                    let self_authored =
                        change.kind == "message" && caller.is_some() && change.sender == caller;
                    if !self_authored {
                        return Outcome::Changed;
                    }
                    // Keep waiting: a burst of self-sends must not turn into a
                    // burst of Changed replies to the sender.
                }
                // Lagged: we missed some changes, which still means "something
                // changed". Report it rather than silently swallowing.
                Ok(Err(broadcast::error::RecvError::Lagged(_))) => return Outcome::Changed,
                Ok(Err(broadcast::error::RecvError::Closed)) => return Outcome::Timeout,
            }
        }
    }
}

impl Drop for Watcher {
    fn drop(&mut self) {
        // Best effort: aborts the task, but cannot wait for it. Anything that
        // needs the connection released *before* it continues — graceful
        // shutdown, a test tearing down its database — must call
        // [`Watcher::shutdown`] instead.
        if let Ok(mut guard) = self.listener.lock() {
            if let Some(handle) = guard.take() {
                handle.abort();
            }
        }
    }
}
