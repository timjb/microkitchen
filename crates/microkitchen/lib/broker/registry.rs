//! Registered sandboxes, their endpoints, and the admission path shared by
//! the TCP and UDP mediators (design §4, §9).
//!
//! Which listener a request arrives on *is* the sandbox identity; the SOCKS5
//! username and password only confirm it.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use serde::{Deserialize, Serialize};
use subtle::ConstantTimeEq;
use tokio::net::{TcpListener, UdpSocket};
use tokio_util::sync::CancellationToken;

use super::approval::{ApprovalQueue, CoalesceKey, Outcome, PromptInfo};
use super::audit::{Audit, AuditEvent};
use super::bindings::BindingStore;
use super::decision::{self, Admission, Decision, Session};
use super::mediator;
use super::observer::{self, Recorder};
use super::protocol::{
    Answer, BindingInfo, Mode, PendingApproval, Registration, SandboxInfo, Transport,
};
use super::rules::RuleSource;
use crate::config::edit::{self, RuleList};
use crate::config::hostpat::HostPattern;
use crate::state::settings::Settings;
use crate::state::{Home, secret, write_atomic};

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

/// How long "Allow 5 min" lasts.
pub const TEMP_ALLOW: Duration = Duration::from_secs(5 * 60);

/// Overrides [`TEMP_ALLOW`] in seconds (for tests).
pub const TEMP_ALLOW_ENV: &str = "MICROKITCHEN_TEMP_ALLOW_SECS";

const RANDOM_PORT_ATTEMPTS: usize = 64;

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

pub struct SandboxEntry {
    pub name: String,
    pub kitchen_file: PathBuf,
    pub resolver_port: u16,
    pub proxy_port: u16,
    secret: Vec<u8>,
    mode: Mutex<Mode>,
    bindings: Mutex<BindingStore>,
    session: Mutex<Session>,
    rules: RuleSource,
    /// Cancelled on retirement: listeners, flows and pending approvals end.
    pub cancel: CancellationToken,
}

pub struct Broker {
    home: Home,
    port_range: (u16, u16),
    upstreams: Arc<[SocketAddr]>,
    sandboxes: Mutex<HashMap<String, Arc<SandboxEntry>>>,
    approvals: ApprovalQueue,
    audit: Audit,
    temp_allow: Duration,
}

/// `broker/registry.json`: registrations survive a broker restart.
#[derive(Serialize, Deserialize)]
struct Persisted {
    name: String,
    kitchen_file: PathBuf,
    mode: Mode,
    resolver_port: u16,
    proxy_port: u16,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl SandboxEntry {
    /// Username must be the sandbox name; the password is compared in constant time.
    pub fn authenticate(&self, username: &[u8], password: &[u8]) -> bool {
        let username_ok = username == self.name.as_bytes();
        let password_ok: bool = password.ct_eq(&self.secret).into();
        username_ok & password_ok
    }

    pub fn mode(&self) -> Mode {
        *self.mode.lock().unwrap()
    }

    fn record_dns(&self, response: &[u8]) {
        let learned = observer::extract_bindings(response);
        if learned.is_empty() {
            return;
        }
        let now = Instant::now();
        let mut bindings = self.bindings.lock().unwrap();
        for (address, names, ttl) in learned {
            bindings.record(address, names, Duration::from_secs(ttl.into()), now);
        }
    }

    fn registration(&self) -> Registration {
        Registration {
            resolver_port: self.resolver_port,
            proxy_port: self.proxy_port,
        }
    }
}

impl Broker {
    pub fn new(home: Home, settings: &Settings) -> Result<Arc<Self>> {
        let upstreams = observer::upstreams(&settings.broker.upstream_dns)?;
        let temp_allow = std::env::var(TEMP_ALLOW_ENV)
            .ok()
            .and_then(|v| v.parse().ok())
            .map_or(TEMP_ALLOW, Duration::from_secs);
        Ok(Arc::new(Self {
            audit: Audit::open(&home.broker_dir().join("audit.log")),
            approvals: ApprovalQueue::new(settings.approval.clone()),
            port_range: settings.broker.port_range,
            upstreams: Arc::from(upstreams),
            sandboxes: Mutex::default(),
            temp_allow,
            home,
        }))
    }

    /// Register a sandbox, binding its resolver and proxy endpoints. A
    /// compatible existing registration is kept (only the mode changes).
    pub async fn register(
        self: &Arc<Self>,
        name: &str,
        kitchen_file: PathBuf,
        mode: Mode,
        resolver_port: Option<u16>,
        proxy_port: Option<u16>,
    ) -> Result<Registration> {
        if let Some(existing) = self.get(name) {
            let compatible = existing.kitchen_file == kitchen_file
                && resolver_port.is_none_or(|p| p == existing.resolver_port)
                && proxy_port.is_none_or(|p| p == existing.proxy_port);
            if compatible {
                *existing.mode.lock().unwrap() = mode;
                self.persist();
                return Ok(existing.registration());
            }
            self.retire(name);
        }

        let secret = secret::load(&self.home, name)?
            .with_context(|| format!("sandbox {name} has no proxy secret"))?;
        let (resolver_udp, resolver_tcp, resolver_port) =
            bind_resolver(resolver_port, self.port_range).await?;
        let (proxy, proxy_port) = bind_proxy(proxy_port, self.port_range).await?;

        let entry = Arc::new(SandboxEntry {
            name: name.to_owned(),
            rules: RuleSource::new(&kitchen_file),
            kitchen_file,
            resolver_port,
            proxy_port,
            secret: secret.into_bytes(),
            mode: Mutex::new(mode),
            bindings: Mutex::default(),
            session: Mutex::default(),
            cancel: CancellationToken::new(),
        });

        let recorder: Recorder = {
            let entry = Arc::downgrade(&entry);
            Arc::new(move |response: &[u8]| {
                if let Some(entry) = entry.upgrade() {
                    entry.record_dns(response);
                }
            })
        };
        tokio::spawn(observer::serve_udp(
            resolver_udp,
            self.upstreams.clone(),
            recorder.clone(),
            entry.cancel.clone(),
        ));
        tokio::spawn(observer::serve_tcp(
            resolver_tcp,
            self.upstreams.clone(),
            recorder,
            entry.cancel.clone(),
        ));
        tokio::spawn(mediator::serve(self.clone(), entry.clone(), proxy));

        self.sandboxes
            .lock()
            .unwrap()
            .insert(name.to_owned(), entry.clone());
        self.persist();
        tracing::info!(
            sandbox = name,
            resolver_port,
            proxy_port,
            ?mode,
            "registered"
        );
        Ok(entry.registration())
    }

    /// Drop the endpoints, expire bindings, cancel approvals, tear down flows.
    pub fn retire(&self, name: &str) -> bool {
        let removed = self.sandboxes.lock().unwrap().remove(name);
        let Some(entry) = removed else {
            return false;
        };
        entry.cancel.cancel();
        self.approvals.cancel_sandbox(name);
        self.persist();
        tracing::info!(sandbox = name, "retired");
        true
    }

    pub fn set_mode(&self, name: &str, mode: Mode) -> Result<()> {
        let entry = self
            .get(name)
            .with_context(|| format!("{name} is not registered"))?;
        *entry.mode.lock().unwrap() = mode;
        self.persist();
        tracing::info!(sandbox = name, ?mode, "mode changed");
        Ok(())
    }

    /// Temporarily allow a name or address for one sandbox.
    pub fn grant(&self, name: &str, subject: &str) -> Result<()> {
        let entry = self
            .get(name)
            .with_context(|| format!("{name} is not registered"))?;
        let pattern: HostPattern = subject
            .parse()
            .with_context(|| format!("invalid host {subject:?}"))?;
        let key = session_key(&pattern)
            .ok_or_else(|| anyhow!("temporary allows take a host name or an address"))?;
        entry
            .session
            .lock()
            .unwrap()
            .grant(key, Instant::now() + self.temp_allow);
        Ok(())
    }

    pub fn list(&self) -> Vec<SandboxInfo> {
        let mut out: Vec<SandboxInfo> = self
            .sandboxes
            .lock()
            .unwrap()
            .values()
            .map(|e| SandboxInfo {
                name: e.name.clone(),
                kitchen_file: e.kitchen_file.clone(),
                mode: e.mode(),
                resolver_port: e.resolver_port,
                proxy_port: e.proxy_port,
                bindings: e.bindings.lock().unwrap().len(),
            })
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name));
        out
    }

    pub fn bindings(&self, name: &str) -> Result<Vec<BindingInfo>> {
        let entry = self
            .get(name)
            .with_context(|| format!("{name} is not registered"))?;
        let now = Instant::now();
        Ok(entry
            .bindings
            .lock()
            .unwrap()
            .snapshot(now)
            .into_iter()
            .map(|(address, names, remaining)| BindingInfo {
                address,
                names,
                expires_in_secs: remaining.as_secs(),
            })
            .collect())
    }

    pub fn pending(&self) -> Vec<PendingApproval> {
        self.approvals.list()
    }

    pub fn decide(&self, id: u64, answer: Answer) -> bool {
        self.approvals.decide(id, answer)
    }

    /// Re-register what was registered before a restart.
    pub async fn restore(self: &Arc<Self>) {
        let path = self.registry_file();
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        let entries: Vec<Persisted> = match serde_json::from_str(&text) {
            Ok(entries) => entries,
            Err(error) => {
                tracing::error!(%error, path = %path.display(), "ignoring an unreadable registry");
                return;
            }
        };
        for p in entries {
            if let Err(error) = self
                .register(
                    &p.name,
                    p.kitchen_file,
                    p.mode,
                    Some(p.resolver_port),
                    Some(p.proxy_port),
                )
                .await
            {
                tracing::warn!(sandbox = %p.name, error = %format!("{error:#}"), "could not restore registration");
            }
        }
    }

    /// Decide whether a flow may proceed, asking a human if nothing stored applies.
    /// `claimed` is the name from a SOCKS domain request, resolved by the broker itself.
    pub async fn admit(
        &self,
        entry: &SandboxEntry,
        transport: Transport,
        address: IpAddr,
        port: u16,
        claimed: Option<String>,
        cancel: &CancellationToken,
    ) -> bool {
        let now = Instant::now();
        let address = address.to_canonical();
        let candidates = match claimed {
            Some(name) => vec![vec![name]],
            None => entry.bindings.lock().unwrap().lookup(address, now),
        };
        let admission = Admission {
            transport,
            address,
            port,
            candidates,
            malformed: false,
        };
        let rules = entry.rules.current();
        let decision = {
            let session = entry.session.lock().unwrap();
            decision::decide(&admission, entry.mode(), &rules, &session, now)
        };

        match decision {
            Decision::Allow { reason, ambiguous } => {
                self.log(entry, &admission, true, &reason.to_string(), ambiguous);
                true
            }
            Decision::Deny { reason, ambiguous } => {
                self.log(entry, &admission, false, &reason.to_string(), ambiguous);
                false
            }
            Decision::Prompt(kind) => {
                let subjects = kind.subjects(&admission);
                let subject = if admission.candidates.is_empty() {
                    address.to_string()
                } else {
                    let queries: Vec<&str> = admission
                        .candidates
                        .iter()
                        .filter_map(|c| c.first().map(String::as_str))
                        .collect();
                    queries.join(",")
                };
                let key = CoalesceKey {
                    sandbox: entry.name.clone(),
                    subject,
                    port,
                };
                let info = PromptInfo {
                    sandbox: entry.name.clone(),
                    transport,
                    address,
                    port,
                    names: admission.names(),
                };
                let outcome = self.approvals.ask(key, info, cancel).await;
                let allowed = self.apply(entry, &subjects, outcome);
                let source = format!("prompt:{}", outcome_name(outcome));
                self.log(
                    entry,
                    &admission,
                    allowed,
                    &source,
                    admission.candidates.len() > 1,
                );
                allowed
            }
        }
    }

    /// Record a request refused before admission (e.g. a malformed domain).
    pub fn deny_malformed(&self, entry: &SandboxEntry, transport: Transport, detail: &str) {
        tracing::warn!(sandbox = %entry.name, detail, "malformed request refused");
        self.audit.record(&AuditEvent {
            sandbox: &entry.name,
            transport,
            address: IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            port: 0,
            names: &[],
            allowed: false,
            source: &format!("malformed:{detail}"),
            ambiguous: false,
        });
    }

    /// Apply an operator's answer. Persisting is best effort: a failed write
    /// is logged and the current flow still gets its verdict.
    fn apply(&self, entry: &SandboxEntry, subjects: &[HostPattern], outcome: Outcome) -> bool {
        match outcome {
            Outcome::Allow | Outcome::Deny => {
                let list = if outcome == Outcome::Allow {
                    RuleList::Allow
                } else {
                    RuleList::Deny
                };
                for subject in subjects {
                    if let Err(error) =
                        edit::set_network_rule(&self.home, &entry.kitchen_file, list, subject)
                    {
                        tracing::error!(
                            sandbox = %entry.name,
                            rule = %subject,
                            error = %format!("{error:#}"),
                            "could not persist the decision; it applies to this flow only"
                        );
                    }
                }
                outcome == Outcome::Allow
            }
            Outcome::Temp => {
                let until = Instant::now() + self.temp_allow;
                let mut session = entry.session.lock().unwrap();
                for key in subjects.iter().filter_map(session_key) {
                    session.grant(key, until);
                }
                true
            }
            Outcome::Dismissed | Outcome::Cancelled => false,
        }
    }

    fn log(
        &self,
        entry: &SandboxEntry,
        admission: &Admission,
        allowed: bool,
        source: &str,
        ambiguous: bool,
    ) {
        let names = admission.names();
        tracing::info!(
            sandbox = %entry.name,
            address = %admission.address,
            port = admission.port,
            names = ?names,
            allowed,
            source,
            "verdict"
        );
        self.audit.record(&AuditEvent {
            sandbox: &entry.name,
            transport: admission.transport,
            address: admission.address,
            port: admission.port,
            names: &names,
            allowed,
            source,
            ambiguous,
        });
    }

    fn get(&self, name: &str) -> Option<Arc<SandboxEntry>> {
        self.sandboxes.lock().unwrap().get(name).cloned()
    }

    fn registry_file(&self) -> PathBuf {
        self.home.broker_dir().join("registry.json")
    }

    fn persist(&self) {
        let entries: Vec<Persisted> = self
            .sandboxes
            .lock()
            .unwrap()
            .values()
            .map(|e| Persisted {
                name: e.name.clone(),
                kitchen_file: e.kitchen_file.clone(),
                mode: e.mode(),
                resolver_port: e.resolver_port,
                proxy_port: e.proxy_port,
            })
            .collect();
        let result = serde_json::to_vec_pretty(&entries)
            .map_err(anyhow::Error::from)
            .and_then(|json| write_atomic(&self.registry_file(), &json));
        if let Err(error) = result {
            tracing::error!(error = %format!("{error:#}"), "could not persist the broker registry");
        }
    }
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

fn session_key(pattern: &HostPattern) -> Option<String> {
    match pattern {
        HostPattern::Exact(name) => Some(name.clone()),
        HostPattern::Address(address) => Some(address.to_canonical().to_string()),
        HostPattern::Suffix(_) | HostPattern::Network(_) => None,
    }
}

fn outcome_name(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Allow => "allow",
        Outcome::Deny => "deny",
        Outcome::Temp => "temp",
        Outcome::Dismissed => "dismissed",
        Outcome::Cancelled => "cancelled",
    }
}

fn candidate_ports(requested: Option<u16>, (low, high): (u16, u16)) -> Vec<u16> {
    match requested {
        Some(port) => vec![port],
        None => {
            let span = u32::from(high - low) + 1;
            (0..RANDOM_PORT_ATTEMPTS)
                .map(|_| low + (getrandom::u32().unwrap_or(0) % span) as u16)
                .collect()
        }
    }
}

fn unavailable(requested: Option<u16>) -> anyhow::Error {
    match requested {
        Some(port) => anyhow!(
            "port {port} is in use; a sandbox's broker endpoints are fixed when it is created, \
             so free the port or recreate the sandbox with `microkitchen up --recreate`"
        ),
        None => anyhow!("no free port in broker.port_range"),
    }
}

/// The resolver endpoint: one port for both UDP and TCP.
async fn bind_resolver(
    requested: Option<u16>,
    range: (u16, u16),
) -> Result<(UdpSocket, TcpListener, u16)> {
    for port in candidate_ports(requested, range) {
        let address = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let Ok(udp) = UdpSocket::bind(address).await else {
            continue;
        };
        let Ok(tcp) = TcpListener::bind(address).await else {
            continue;
        };
        return Ok((udp, tcp, port));
    }
    Err(unavailable(requested))
}

async fn bind_proxy(requested: Option<u16>, range: (u16, u16)) -> Result<(TcpListener, u16)> {
    for port in candidate_ports(requested, range) {
        if let Ok(listener) = TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await {
            return Ok((listener, port));
        }
    }
    Err(unavailable(requested))
}
