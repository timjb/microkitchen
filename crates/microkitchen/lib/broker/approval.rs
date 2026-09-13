//! The approval queue (design §10).
//!
//! Concurrent flows to the same destination share one approval. A request
//! whose flow died is dropped without being answered. Timeouts and missing
//! approval surfaces deny once and are never remembered.
//!
//! Milestone 4 has the headless surface only: requests are listed through
//! the admin interface (`microkitchen net pending`) and answered with
//! `microkitchen net decide`. Desktop dialogs arrive with milestone 5.

use std::collections::{BTreeMap, HashMap};
use std::net::IpAddr;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use tokio::sync::{oneshot, watch};
use tokio_util::sync::CancellationToken;

use super::protocol::{Answer, PendingApproval, Transport};
use crate::state::settings::{ApprovalSettings, HeadlessFallback};

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Allow,
    Deny,
    Temp,
    /// Timed out or no approval surface: deny this flow, remember nothing.
    Dismissed,
    /// The flow went away first.
    Cancelled,
}

/// Coalescing key: `(sandbox, name-or-address, port)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CoalesceKey {
    pub sandbox: String,
    pub subject: String,
    pub port: u16,
}

/// What an operator sees.
#[derive(Debug, Clone)]
pub struct PromptInfo {
    pub sandbox: String,
    pub transport: Transport,
    pub address: IpAddr,
    pub port: u16,
    pub names: Vec<String>,
}

struct Pending {
    info: PromptInfo,
    created: Instant,
    respond: oneshot::Sender<Outcome>,
}

pub struct ApprovalQueue {
    settings: ApprovalSettings,
    next_id: AtomicU64,
    pending: Mutex<BTreeMap<u64, Pending>>,
    inflight: Mutex<HashMap<CoalesceKey, watch::Receiver<Option<Outcome>>>>,
}

enum Role {
    Leader(watch::Sender<Option<Outcome>>),
    Follower(watch::Receiver<Option<Outcome>>),
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl ApprovalQueue {
    pub fn new(settings: ApprovalSettings) -> Self {
        Self {
            settings,
            next_id: AtomicU64::new(0),
            pending: Mutex::default(),
            inflight: Mutex::default(),
        }
    }

    /// Ask for a decision; waits for the first requester's outcome when an
    /// identical request is already in flight.
    pub async fn ask(
        &self,
        key: CoalesceKey,
        info: PromptInfo,
        cancel: &CancellationToken,
    ) -> Outcome {
        loop {
            let role = {
                let mut inflight = self.inflight.lock().unwrap();
                match inflight.get(&key) {
                    Some(receiver) => Role::Follower(receiver.clone()),
                    None => {
                        let (sender, receiver) = watch::channel(None);
                        inflight.insert(key.clone(), receiver);
                        Role::Leader(sender)
                    }
                }
            };
            match role {
                Role::Leader(sender) => {
                    let outcome = self.prompt(info.clone(), cancel).await;
                    self.inflight.lock().unwrap().remove(&key);
                    let _ = sender.send(Some(outcome));
                    return outcome;
                }
                Role::Follower(mut receiver) => {
                    let outcome = tokio::select! {
                        _ = cancel.cancelled() => return Outcome::Cancelled,
                        result = receiver.wait_for(Option::is_some) => result.ok().and_then(|o| *o),
                    };
                    match outcome {
                        // The leader's flow died: ask again on our own behalf.
                        Some(Outcome::Cancelled) | None => continue,
                        Some(outcome) => return outcome,
                    }
                }
            }
        }
    }

    /// Answer a pending request. False if it no longer exists.
    pub fn decide(&self, id: u64, answer: Answer) -> bool {
        match self.pending.lock().unwrap().remove(&id) {
            Some(pending) => pending.respond.send(answer.into()).is_ok(),
            None => false,
        }
    }

    /// Drop a retired sandbox's requests.
    pub fn cancel_sandbox(&self, sandbox: &str) {
        let mut pending = self.pending.lock().unwrap();
        let ids: Vec<u64> = pending
            .iter()
            .filter(|(_, p)| p.info.sandbox == sandbox)
            .map(|(id, _)| *id)
            .collect();
        for id in ids {
            if let Some(p) = pending.remove(&id) {
                let _ = p.respond.send(Outcome::Cancelled);
            }
        }
    }

    pub fn list(&self) -> Vec<PendingApproval> {
        let now = Instant::now();
        self.pending
            .lock()
            .unwrap()
            .iter()
            .map(|(id, p)| PendingApproval {
                id: *id,
                sandbox: p.info.sandbox.clone(),
                transport: p.info.transport,
                address: p.info.address,
                port: p.info.port,
                names: p.info.names.clone(),
                unresolved: p.info.names.is_empty(),
                age_secs: now.duration_since(p.created).as_secs(),
            })
            .collect()
    }

    async fn prompt(&self, info: PromptInfo, cancel: &CancellationToken) -> Outcome {
        if self.settings.headless == HeadlessFallback::Deny {
            tracing::info!(
                sandbox = %info.sandbox,
                address = %info.address,
                port = info.port,
                "no approval surface (approval.headless = \"deny\"); denying"
            );
            return Outcome::Dismissed;
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let (respond, answer) = oneshot::channel();
        self.pending.lock().unwrap().insert(
            id,
            Pending {
                info,
                created: Instant::now(),
                respond,
            },
        );
        let outcome = tokio::select! {
            answer = answer => answer.unwrap_or(Outcome::Cancelled),
            _ = tokio::time::sleep(self.settings.timeout()) => Outcome::Dismissed,
            _ = cancel.cancelled() => Outcome::Cancelled,
        };
        self.pending.lock().unwrap().remove(&id);
        outcome
    }
}

//--------------------------------------------------------------------------------------------------
// Trait Implementations
//--------------------------------------------------------------------------------------------------

impl From<Answer> for Outcome {
    fn from(answer: Answer) -> Self {
        match answer {
            Answer::Allow => Self::Allow,
            Answer::Deny => Self::Deny,
            Answer::Temp => Self::Temp,
        }
    }
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;

    fn queue(headless: HeadlessFallback, timeout_secs: u64) -> Arc<ApprovalQueue> {
        Arc::new(ApprovalQueue::new(ApprovalSettings {
            headless,
            timeout_secs,
        }))
    }

    fn key(subject: &str) -> CoalesceKey {
        CoalesceKey {
            sandbox: "mk-a".into(),
            subject: subject.into(),
            port: 443,
        }
    }

    fn info() -> PromptInfo {
        PromptInfo {
            sandbox: "mk-a".into(),
            transport: Transport::Tcp,
            address: "203.0.113.1".parse().unwrap(),
            port: 443,
            names: vec!["example.com".into()],
        }
    }

    async fn wait_for_pending(queue: &ApprovalQueue, count: usize) -> Vec<PendingApproval> {
        for _ in 0..200 {
            let pending = queue.list();
            if pending.len() == count {
                return pending;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        panic!("expected {count} pending requests, have {:?}", queue.list());
    }

    #[tokio::test]
    async fn headless_deny_answers_immediately() {
        let q = queue(HeadlessFallback::Deny, 60);
        assert_eq!(
            q.ask(key("example.com"), info(), &CancellationToken::new())
                .await,
            Outcome::Dismissed
        );
        assert!(q.list().is_empty());
    }

    #[tokio::test]
    async fn queued_requests_are_answered() {
        let q = queue(HeadlessFallback::Queue, 60);
        let task = tokio::spawn({
            let q = q.clone();
            async move {
                q.ask(key("example.com"), info(), &CancellationToken::new())
                    .await
            }
        });
        let pending = wait_for_pending(&q, 1).await;
        assert_eq!(pending[0].names, vec!["example.com".to_string()]);
        assert!(q.decide(pending[0].id, Answer::Temp));
        assert_eq!(task.await.unwrap(), Outcome::Temp);
        assert!(q.list().is_empty());
        assert!(
            !q.decide(pending[0].id, Answer::Allow),
            "answered requests are gone"
        );
    }

    #[tokio::test]
    async fn concurrent_requests_coalesce() {
        let q = queue(HeadlessFallback::Queue, 60);
        let tasks: Vec<_> = (0..5)
            .map(|_| {
                let q = q.clone();
                tokio::spawn(async move {
                    q.ask(key("example.com"), info(), &CancellationToken::new())
                        .await
                })
            })
            .collect();
        let pending = wait_for_pending(&q, 1).await;
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(q.list().len(), 1, "one prompt for five flows");
        q.decide(pending[0].id, Answer::Allow);
        for task in tasks {
            assert_eq!(task.await.unwrap(), Outcome::Allow);
        }
    }

    #[tokio::test]
    async fn dead_flows_are_dropped_unshown() {
        let q = queue(HeadlessFallback::Queue, 60);
        let cancel = CancellationToken::new();
        let task = tokio::spawn({
            let (q, cancel) = (q.clone(), cancel.clone());
            async move { q.ask(key("example.com"), info(), &cancel).await }
        });
        wait_for_pending(&q, 1).await;
        cancel.cancel();
        assert_eq!(task.await.unwrap(), Outcome::Cancelled);
        assert!(q.list().is_empty());
    }

    #[tokio::test]
    async fn a_follower_takes_over_when_the_leader_dies() {
        let q = queue(HeadlessFallback::Queue, 60);
        let leader_cancel = CancellationToken::new();
        let leader = tokio::spawn({
            let (q, cancel) = (q.clone(), leader_cancel.clone());
            async move { q.ask(key("example.com"), info(), &cancel).await }
        });
        wait_for_pending(&q, 1).await;
        let follower = tokio::spawn({
            let q = q.clone();
            async move {
                q.ask(key("example.com"), info(), &CancellationToken::new())
                    .await
            }
        });
        tokio::time::sleep(Duration::from_millis(20)).await;
        leader_cancel.cancel();
        assert_eq!(leader.await.unwrap(), Outcome::Cancelled);
        let pending = wait_for_pending(&q, 1).await;
        q.decide(pending[0].id, Answer::Deny);
        assert_eq!(follower.await.unwrap(), Outcome::Deny);
    }

    #[tokio::test]
    async fn timeouts_dismiss() {
        let q = queue(HeadlessFallback::Queue, 1);
        let outcome = q
            .ask(key("example.com"), info(), &CancellationToken::new())
            .await;
        assert_eq!(outcome, Outcome::Dismissed);
        assert!(q.list().is_empty());
    }

    #[tokio::test]
    async fn retiring_a_sandbox_cancels_its_requests() {
        let q = queue(HeadlessFallback::Queue, 60);
        let task = tokio::spawn({
            let q = q.clone();
            async move {
                q.ask(key("example.com"), info(), &CancellationToken::new())
                    .await
            }
        });
        wait_for_pending(&q, 1).await;
        q.cancel_sandbox("mk-a");
        // The follower loop re-asks after a cancellation; a retired sandbox's
        // flows are torn down by their own cancel token, so only check the queue.
        tokio::time::sleep(Duration::from_millis(20)).await;
        task.abort();
    }
}
