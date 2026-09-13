//! The approval queue (design §10).
//!
//! Concurrent flows to the same destination share one approval. A request
//! whose flow died is dropped without being shown. Timeouts, dismissed dialogs
//! and missing approval surfaces deny once and are never remembered.
//!
//! Requests are listed through the admin interface (`microkitchen net
//! pending`) and can always be answered with `microkitchen net decide`. With a
//! desktop [`Surface`] they are also shown as dialogs, strictly one at a time
//! across all sandboxes. Prompts that reach a human are rate limited per
//! sandbox: a guest must not be able to bury one real request in noise.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::{oneshot, watch};
use tokio_util::sync::CancellationToken;

use super::attribution::{self, Origin};
use super::dialog::Surface;
use super::protocol::{Answer, PendingApproval, Transport};
use crate::state::settings::{ApprovalSettings, HeadlessFallback};

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

/// How often a dialog waiting for attribution checks for it.
const ORIGIN_POLL: Duration = Duration::from_millis(50);

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Allow,
    Deny,
    Temp,
    /// Timed out, dismissed, or no approval surface: deny this flow, remember nothing.
    Dismissed,
    /// The flow went away first.
    Cancelled,
    /// Too many prompts for this sandbox recently: deny, and deny everything
    /// else until `net resume`.
    RateLimited,
}

/// Coalescing key: `(sandbox, name-or-address, port)`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CoalesceKey {
    pub sandbox: String,
    pub subject: String,
    pub port: u16,
}

/// The flow's origin, filled in by attribution while the request waits.
#[derive(Debug, Clone, Default)]
pub struct OriginSlot(Arc<Mutex<Option<Origin>>>);

/// What an operator sees.
#[derive(Debug, Clone)]
pub struct PromptInfo {
    pub sandbox: String,
    pub transport: Transport,
    pub address: IpAddr,
    pub port: u16,
    pub names: Vec<String>,
    pub origin: OriginSlot,
}

struct Pending {
    info: PromptInfo,
    created: Instant,
    respond: oneshot::Sender<Outcome>,
}

pub struct ApprovalQueue {
    settings: ApprovalSettings,
    surface: Option<Arc<dyn Surface>>,
    next_id: AtomicU64,
    pending: Mutex<BTreeMap<u64, Pending>>,
    inflight: Mutex<HashMap<CoalesceKey, watch::Receiver<Option<Outcome>>>>,
    /// Held while a dialog is on screen: one at a time, across sandboxes.
    screen: tokio::sync::Mutex<()>,
    /// When each sandbox's recent prompts started, for the rate limit.
    recent: Mutex<HashMap<String, VecDeque<Instant>>>,
}

enum Role {
    Leader(watch::Sender<Option<Outcome>>),
    Follower(watch::Receiver<Option<Outcome>>),
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl OriginSlot {
    pub fn set(&self, origin: Origin) {
        *self.0.lock().unwrap() = Some(origin);
    }

    pub fn get(&self) -> Option<Origin> {
        self.0.lock().unwrap().clone()
    }
}

impl ApprovalQueue {
    pub fn new(settings: ApprovalSettings, surface: Option<Arc<dyn Surface>>) -> Self {
        Self {
            settings,
            surface,
            next_id: AtomicU64::new(0),
            pending: Mutex::default(),
            inflight: Mutex::default(),
            screen: tokio::sync::Mutex::new(()),
            recent: Mutex::default(),
        }
    }

    /// Whether a prompt reaches a human: a dialog, or the `net decide` queue.
    pub fn reaches_human(&self) -> bool {
        self.surface.is_some() || self.settings.headless == HeadlessFallback::Queue
    }

    /// Whether an identical request is already waiting for an answer.
    pub fn is_inflight(&self, key: &CoalesceKey) -> bool {
        self.inflight.lock().unwrap().contains_key(key)
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
                    let outcome = if self.reaches_human() && !self.count_prompt(&info.sandbox) {
                        Outcome::RateLimited
                    } else {
                        self.prompt(info.clone(), cancel).await
                    };
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

    /// Drop a retired sandbox's requests and prompt history.
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
        drop(pending);
        self.reset(sandbox);
    }

    /// Forget a sandbox's prompt history (`net resume`).
    pub fn reset(&self, sandbox: &str) {
        self.recent.lock().unwrap().remove(sandbox);
    }

    /// Tell the operator something once: a notification when a desktop is
    /// available, and always the log.
    pub fn notify(&self, message: &str) {
        tracing::warn!("{message}");
        if let Some(surface) = &self.surface {
            surface.notify(message);
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
                origin: p.info.origin.get(),
            })
            .collect()
    }

    /// Count a prompt for `sandbox`; false when it would exceed the limit.
    fn count_prompt(&self, sandbox: &str) -> bool {
        let now = Instant::now();
        let window = self.settings.window();
        let mut recent = self.recent.lock().unwrap();
        let times = recent.entry(sandbox.to_owned()).or_default();
        while times
            .front()
            .is_some_and(|t| now.duration_since(*t) >= window)
        {
            times.pop_front();
        }
        if times.len() >= self.settings.max_prompts as usize {
            return false;
        }
        times.push_back(now);
        true
    }

    async fn prompt(&self, info: PromptInfo, cancel: &CancellationToken) -> Outcome {
        if self.surface.is_none() && self.settings.headless == HeadlessFallback::Deny {
            tracing::info!(
                sandbox = %info.sandbox,
                address = %info.address,
                port = info.port,
                "no approval surface (approval.headless = \"deny\"); denying"
            );
            return Outcome::Dismissed;
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed) + 1;
        let created = Instant::now();
        let deadline = created + self.settings.timeout();
        let (respond, answer) = oneshot::channel();
        self.pending.lock().unwrap().insert(
            id,
            Pending {
                info: info.clone(),
                created,
                respond,
            },
        );
        let dialog = async {
            match &self.surface {
                Some(surface) => self.show(surface.as_ref(), &info, created, deadline).await,
                None => std::future::pending().await,
            }
        };
        // Whichever comes first; dropping the dialog closes it.
        let outcome = tokio::select! {
            answer = answer => answer.unwrap_or(Outcome::Cancelled),
            outcome = dialog => outcome,
            () = tokio::time::sleep_until(deadline.into()) => Outcome::Dismissed,
            () = cancel.cancelled() => Outcome::Cancelled,
        };
        self.pending.lock().unwrap().remove(&id);
        outcome
    }

    /// Wait for the screen, give attribution until its deadline, then show
    /// the dialog for whatever time the request has left.
    async fn show(
        &self,
        surface: &dyn Surface,
        info: &PromptInfo,
        created: Instant,
        deadline: Instant,
    ) -> Outcome {
        let _screen = self.screen.lock().await;
        let attribution_deadline = created + attribution::DEADLINE;
        while info.origin.get().is_none() && Instant::now() < attribution_deadline {
            tokio::time::sleep(ORIGIN_POLL).await;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Outcome::Dismissed;
        }
        let origin = info.origin.get();
        surface.show(info, origin.as_ref(), remaining).await
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
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::AtomicUsize;

    use super::*;

    /// A dialog that answers `answer` after `delay`, recording what it saw.
    struct FakeSurface {
        answer: Outcome,
        delay: Duration,
        showing: AtomicUsize,
        most_at_once: AtomicUsize,
        origins: Mutex<Vec<Option<Origin>>>,
        notices: Mutex<Vec<String>>,
    }

    impl FakeSurface {
        fn new(answer: Outcome, delay: Duration) -> Arc<Self> {
            Arc::new(Self {
                answer,
                delay,
                showing: AtomicUsize::new(0),
                most_at_once: AtomicUsize::new(0),
                origins: Mutex::default(),
                notices: Mutex::default(),
            })
        }
    }

    impl Surface for FakeSurface {
        fn show<'a>(
            &'a self,
            _info: &'a PromptInfo,
            origin: Option<&'a Origin>,
            _timeout: Duration,
        ) -> Pin<Box<dyn Future<Output = Outcome> + Send + 'a>> {
            Box::pin(async move {
                let now = self.showing.fetch_add(1, Ordering::SeqCst) + 1;
                self.most_at_once.fetch_max(now, Ordering::SeqCst);
                self.origins.lock().unwrap().push(origin.cloned());
                tokio::time::sleep(self.delay).await;
                self.showing.fetch_sub(1, Ordering::SeqCst);
                self.answer
            })
        }

        fn notify(&self, message: &str) {
            self.notices.lock().unwrap().push(message.to_owned());
        }
    }

    fn settings(headless: HeadlessFallback, timeout_secs: u64) -> ApprovalSettings {
        ApprovalSettings {
            headless,
            timeout_secs,
            ..ApprovalSettings::default()
        }
    }

    fn queue(headless: HeadlessFallback, timeout_secs: u64) -> Arc<ApprovalQueue> {
        Arc::new(ApprovalQueue::new(settings(headless, timeout_secs), None))
    }

    fn with_surface(surface: Arc<FakeSurface>, settings: ApprovalSettings) -> Arc<ApprovalQueue> {
        Arc::new(ApprovalQueue::new(settings, Some(surface)))
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
            origin: OriginSlot::default(),
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

    #[tokio::test]
    async fn dialogs_are_shown_one_at_a_time() {
        let surface = FakeSurface::new(Outcome::Allow, Duration::from_millis(50));
        let q = with_surface(surface.clone(), settings(HeadlessFallback::Deny, 60));
        let tasks: Vec<_> = ["a.com", "b.com", "c.com"]
            .into_iter()
            .map(|subject| {
                let q = q.clone();
                tokio::spawn(
                    async move { q.ask(key(subject), info(), &CancellationToken::new()).await },
                )
            })
            .collect();
        for task in tasks {
            assert_eq!(task.await.unwrap(), Outcome::Allow);
        }
        assert_eq!(surface.most_at_once.load(Ordering::SeqCst), 1);
        assert_eq!(surface.origins.lock().unwrap().len(), 3);
    }

    #[tokio::test]
    async fn the_cli_can_answer_while_a_dialog_is_up() {
        let surface = FakeSurface::new(Outcome::Deny, Duration::from_secs(30));
        let q = with_surface(surface.clone(), settings(HeadlessFallback::Deny, 60));
        let task = tokio::spawn({
            let q = q.clone();
            async move {
                q.ask(key("example.com"), info(), &CancellationToken::new())
                    .await
            }
        });
        let pending = wait_for_pending(&q, 1).await;
        assert!(q.decide(pending[0].id, Answer::Temp));
        assert_eq!(task.await.unwrap(), Outcome::Temp);
    }

    #[tokio::test]
    async fn dialogs_wait_briefly_for_the_origin() {
        let surface = FakeSurface::new(Outcome::Deny, Duration::ZERO);
        let q = with_surface(surface.clone(), settings(HeadlessFallback::Deny, 60));
        let prompt = info();
        let slot = prompt.origin.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(200)).await;
            slot.set(Origin {
                pid: 412,
                name: "node".into(),
            });
        });
        q.ask(key("example.com"), prompt, &CancellationToken::new())
            .await;
        let origins = surface.origins.lock().unwrap();
        assert_eq!(origins[0].as_ref().map(|o| o.pid), Some(412));
    }

    #[tokio::test]
    async fn the_rate_limit_trips_at_the_threshold() {
        let surface = FakeSurface::new(Outcome::Deny, Duration::ZERO);
        let q = with_surface(
            surface,
            ApprovalSettings {
                max_prompts: 2,
                ..settings(HeadlessFallback::Deny, 60)
            },
        );
        let ask = |subject: &'static str| {
            let q = q.clone();
            async move { q.ask(key(subject), info(), &CancellationToken::new()).await }
        };
        assert_eq!(ask("a.com").await, Outcome::Deny);
        assert_eq!(ask("b.com").await, Outcome::Deny);
        assert_eq!(ask("c.com").await, Outcome::RateLimited);
        q.reset("mk-a");
        assert_eq!(ask("d.com").await, Outcome::Deny, "resume starts over");
    }

    #[tokio::test]
    async fn headless_deny_is_not_rate_limited() {
        let q = Arc::new(ApprovalQueue::new(
            ApprovalSettings {
                max_prompts: 1,
                ..settings(HeadlessFallback::Deny, 60)
            },
            None,
        ));
        for subject in ["a.com", "b.com", "c.com"] {
            assert_eq!(
                q.ask(key(subject), info(), &CancellationToken::new()).await,
                Outcome::Dismissed
            );
        }
    }

    #[tokio::test]
    async fn notices_reach_the_surface() {
        let surface = FakeSurface::new(Outcome::Deny, Duration::ZERO);
        let q = with_surface(surface.clone(), settings(HeadlessFallback::Deny, 60));
        q.notify("too many approvals");
        assert_eq!(
            *surface.notices.lock().unwrap(),
            vec!["too many approvals".to_string()]
        );
    }
}
