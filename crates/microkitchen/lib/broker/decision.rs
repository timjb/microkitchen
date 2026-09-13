//! The decision engine (design §9): stored rules first, a human only when
//! nothing stored applies.
//!
//! Attribution is structurally excluded: [`Admission`] has no field for the
//! guest process, so a guest cannot write its own evidence into a verdict.

use std::collections::HashMap;
use std::fmt;
use std::net::{IpAddr, Ipv6Addr};
use std::time::Instant;

use super::bindings::{Chain, flatten};
use super::protocol::{Mode, Transport};
use crate::config::hostpat::HostPattern;

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// What the mediator knows about a flow before deciding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Admission {
    pub transport: Transport,
    pub address: IpAddr,
    pub port: u16,
    /// Lookups by this sandbox that returned `address`, each a query name
    /// followed by its CNAME chain (from the binding store).
    pub candidates: Vec<Chain>,
    /// The request failed validation (e.g. a domain name outside the grammar).
    pub malformed: bool,
}

/// Persistent rules: the kitchen file's `[_.microkitchen.network]`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Rules {
    pub allow: Vec<HostPattern>,
    pub deny: Vec<HostPattern>,
}

/// Temporary allows for one sandbox, keyed by name or address.
#[derive(Debug, Default)]
pub struct Session {
    grants: HashMap<String, Instant>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    Malformed,
    HardDeny,
    OpenMode,
    DenyRule(HostPattern),
    AllowRule(HostPattern),
    Session,
}

/// Why a human has to decide.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptKind {
    /// The sandbox never resolved this address.
    Unresolved,
    /// No candidate has a stored verdict.
    Unknown,
    /// Candidates' stored verdicts disagree (or only some have one), keyed
    /// by each candidate's query name.
    Disagreement(Vec<(String, Option<bool>)>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow { reason: Reason, ambiguous: bool },
    Deny { reason: Reason, ambiguous: bool },
    Prompt(PromptKind),
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl Admission {
    /// Every candidate name once, query names and aliases alike.
    pub fn names(&self) -> Vec<String> {
        flatten(&self.candidates)
    }
}

impl Session {
    pub fn grant(&mut self, key: impl Into<String>, until: Instant) {
        self.grants.insert(key.into(), until);
    }

    pub fn allows(&self, key: &str, now: Instant) -> bool {
        self.grants.get(key).is_some_and(|until| *until > now)
    }
}

impl PromptKind {
    /// What an operator's answer is about: the address when unresolved,
    /// otherwise the query name of every candidate without a stored verdict.
    pub fn subjects(&self, admission: &Admission) -> Vec<HostPattern> {
        let names: Vec<&String> = match self {
            Self::Unresolved => {
                return vec![HostPattern::Address(admission.address.to_canonical())];
            }
            Self::Unknown => admission
                .candidates
                .iter()
                .filter_map(|c| c.first())
                .collect(),
            Self::Disagreement(verdicts) => verdicts
                .iter()
                .filter(|(_, verdict)| verdict.is_none())
                .map(|(name, _)| name)
                .collect(),
        };
        names.into_iter().filter_map(|n| n.parse().ok()).collect()
    }
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// Addresses that are never reachable and never promptable: metadata,
/// link-local, multicast, unspecified, broadcast.
pub fn is_hard_denied(address: IpAddr) -> bool {
    const AWS_METADATA_V6: Ipv6Addr = Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254);
    match address.to_canonical() {
        IpAddr::V4(v4) => {
            v4.is_link_local() || v4.is_multicast() || v4.is_unspecified() || v4.is_broadcast()
        }
        IpAddr::V6(v6) => {
            v6.is_multicast()
                || v6.is_unspecified()
                || v6.is_unicast_link_local()
                || v6 == AWS_METADATA_V6
        }
    }
}

/// Precedence, first match wins: malformed, hard denies, open mode, address
/// rules (deny before allow), then per-candidate verdicts resolved as
/// design §6.1.
pub fn decide(
    admission: &Admission,
    mode: Mode,
    rules: &Rules,
    session: &Session,
    now: Instant,
) -> Decision {
    if admission.malformed {
        return Decision::Deny {
            reason: Reason::Malformed,
            ambiguous: false,
        };
    }
    let address = admission.address.to_canonical();
    if is_hard_denied(address) {
        return Decision::Deny {
            reason: Reason::HardDeny,
            ambiguous: false,
        };
    }
    if mode == Mode::Open {
        return Decision::Allow {
            reason: Reason::OpenMode,
            ambiguous: false,
        };
    }
    if let Some(rule) = rules.deny.iter().find(|r| r.matches_address(address)) {
        return Decision::Deny {
            reason: Reason::DenyRule(rule.clone()),
            ambiguous: false,
        };
    }
    if let Some(rule) = rules.allow.iter().find(|r| r.matches_address(address)) {
        return Decision::Allow {
            reason: Reason::AllowRule(rule.clone()),
            ambiguous: false,
        };
    }

    let candidates: Vec<&Chain> = admission
        .candidates
        .iter()
        .filter(|c| !c.is_empty())
        .collect();
    if candidates.is_empty() {
        return if session.allows(&address.to_string(), now) {
            Decision::Allow {
                reason: Reason::Session,
                ambiguous: false,
            }
        } else {
            Decision::Prompt(PromptKind::Unresolved)
        };
    }

    // Name rules only ever see names bound to this address for this sandbox,
    // which is what makes a name allowlist meaningful here.
    let verdicts: Vec<(String, Option<(bool, Reason)>)> = candidates
        .iter()
        .map(|chain| (chain[0].clone(), chain_verdict(chain, rules, session, now)))
        .collect();
    let ambiguous = verdicts.len() > 1;

    if let Some((allow, reason)) = &verdicts[0].1
        && verdicts
            .iter()
            .all(|(_, v)| v.as_ref().map(|(a, _)| *a) == Some(*allow))
    {
        let reason = reason.clone();
        return if *allow {
            Decision::Allow { reason, ambiguous }
        } else {
            Decision::Deny { reason, ambiguous }
        };
    }
    if verdicts.iter().all(|(_, v)| v.is_none()) {
        return Decision::Prompt(PromptKind::Unknown);
    }
    Decision::Prompt(PromptKind::Disagreement(
        verdicts
            .into_iter()
            .map(|(n, v)| (n, v.map(|(a, _)| a)))
            .collect(),
    ))
}

/// One lookup's verdict: denied if any of its names is denied, else allowed
/// if any is allowed (by rule, then by session), else unknown.
fn chain_verdict(
    chain: &[String],
    rules: &Rules,
    session: &Session,
    now: Instant,
) -> Option<(bool, Reason)> {
    for name in chain {
        if let Some(rule) = rules.deny.iter().find(|r| r.matches_name(name)) {
            return Some((false, Reason::DenyRule(rule.clone())));
        }
    }
    for name in chain {
        if let Some(rule) = rules.allow.iter().find(|r| r.matches_name(name)) {
            return Some((true, Reason::AllowRule(rule.clone())));
        }
    }
    chain
        .iter()
        .any(|name| session.allows(name, now))
        .then_some((true, Reason::Session))
}

//--------------------------------------------------------------------------------------------------
// Trait Implementations
//--------------------------------------------------------------------------------------------------

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed => f.write_str("malformed"),
            Self::HardDeny => f.write_str("hard-deny"),
            Self::OpenMode => f.write_str("open-mode"),
            Self::DenyRule(rule) => write!(f, "deny-rule:{rule}"),
            Self::AllowRule(rule) => write!(f, "allow-rule:{rule}"),
            Self::Session => f.write_str("session"),
        }
    }
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    /// Each name its own lookup.
    fn admission(address: &str, names: &[&str]) -> Admission {
        let chains: Vec<&[&str]> = names.iter().map(std::slice::from_ref).collect();
        chained(address, &chains)
    }

    fn chained(address: &str, chains: &[&[&str]]) -> Admission {
        Admission {
            transport: Transport::Tcp,
            address: address.parse().unwrap(),
            port: 443,
            candidates: chains
                .iter()
                .map(|c| c.iter().map(|s| s.to_string()).collect())
                .collect(),
            malformed: false,
        }
    }

    fn rules(allow: &[&str], deny: &[&str]) -> Rules {
        Rules {
            allow: allow.iter().map(|s| s.parse().unwrap()).collect(),
            deny: deny.iter().map(|s| s.parse().unwrap()).collect(),
        }
    }

    fn run(adm: &Admission, rules: &Rules) -> Decision {
        decide(
            adm,
            Mode::Enforce,
            rules,
            &Session::default(),
            Instant::now(),
        )
    }

    #[test]
    fn malformed_and_hard_denies_win_over_everything() {
        let everything = rules(&["0.0.0.0/0", "::/0"], &[]);
        let mut adm = admission("203.0.113.1", &[]);
        adm.malformed = true;
        let d = decide(
            &adm,
            Mode::Open,
            &everything,
            &Session::default(),
            Instant::now(),
        );
        assert!(matches!(
            d,
            Decision::Deny {
                reason: Reason::Malformed,
                ..
            }
        ));
        for address in [
            "169.254.169.254",
            "169.254.1.1",
            "224.0.0.1",
            "0.0.0.0",
            "255.255.255.255",
            "fe80::1",
            "ff02::1",
            "::",
            "fd00:ec2::254",
        ] {
            let adm = admission(address, &["allowed.com"]);
            let d = decide(
                &adm,
                Mode::Open,
                &everything,
                &Session::default(),
                Instant::now(),
            );
            assert!(
                matches!(
                    d,
                    Decision::Deny {
                        reason: Reason::HardDeny,
                        ..
                    }
                ),
                "{address}: {d:?}"
            );
        }
    }

    #[test]
    fn open_mode_allows_unknown_destinations() {
        let adm = admission("203.0.113.1", &[]);
        let d = decide(
            &adm,
            Mode::Open,
            &Rules::default(),
            &Session::default(),
            Instant::now(),
        );
        assert!(matches!(
            d,
            Decision::Allow {
                reason: Reason::OpenMode,
                ..
            }
        ));
    }

    #[test]
    fn deny_beats_allow() {
        let r = rules(
            &["203.0.113.0/24", "example.com"],
            &["203.0.113.7", "example.com"],
        );
        assert!(matches!(
            run(&admission("203.0.113.7", &[]), &r),
            Decision::Deny { .. }
        ));
        assert!(matches!(
            run(&admission("203.0.113.8", &[]), &r),
            Decision::Allow { .. }
        ));
        assert!(matches!(
            run(&admission("198.51.100.1", &["example.com"]), &r),
            Decision::Deny { .. }
        ));
    }

    #[test]
    fn name_rules_need_a_binding() {
        let r = rules(&["example.com"], &[]);
        // The address was never resolved by this sandbox: the rule cannot apply.
        assert_eq!(
            run(&admission("198.51.100.1", &[]), &r),
            Decision::Prompt(PromptKind::Unresolved)
        );
        // Resolved, but to another name: no laundering through example.com.
        assert_eq!(
            run(&admission("198.51.100.1", &["evil.net"]), &r),
            Decision::Prompt(PromptKind::Unknown)
        );
        let d = run(&admission("198.51.100.1", &["example.com"]), &r);
        assert!(matches!(
            d,
            Decision::Allow {
                ambiguous: false,
                ..
            }
        ));
    }

    #[test]
    fn a_cname_chain_is_one_candidate() {
        // auth.docker.io → auth.docker.io.cdn.cloudflare.net: allowed by the query name.
        let adm = chained(
            "104.18.43.178",
            &[&["auth.docker.io", "auth.docker.io.cdn.cloudflare.net"]],
        );
        let d = run(&adm, &rules(&["*.docker.io"], &[]));
        assert!(
            matches!(
                d,
                Decision::Allow {
                    ambiguous: false,
                    ..
                }
            ),
            "{d:?}"
        );
        // …or by the alias.
        let d = run(&adm, &rules(&["*.cloudflare.net"], &[]));
        assert!(matches!(d, Decision::Allow { .. }));
        // A denied alias denies the lookup.
        let d = run(&adm, &rules(&["*.docker.io"], &["*.cloudflare.net"]));
        assert!(matches!(d, Decision::Deny { .. }));
        // Unknown chains are prompted about by their query name.
        assert_eq!(
            PromptKind::Unknown.subjects(&adm),
            vec!["auth.docker.io".parse::<HostPattern>().unwrap()]
        );
    }

    #[test]
    fn a_shared_alias_does_not_launder() {
        // Two lookups CNAME to the same CDN name: still two candidates.
        let adm = chained(
            "198.51.100.1",
            &[
                &["allowed.com", "edge.cdn.net"],
                &["evil.org", "edge.cdn.net"],
            ],
        );
        let d = run(&adm, &rules(&["allowed.com"], &[]));
        assert_eq!(
            d,
            Decision::Prompt(PromptKind::Disagreement(vec![
                ("allowed.com".into(), Some(true)),
                ("evil.org".into(), None),
            ]))
        );
        if let Decision::Prompt(kind) = d {
            assert_eq!(
                kind.subjects(&adm),
                vec!["evil.org".parse::<HostPattern>().unwrap()]
            );
        }
    }

    #[test]
    fn ambiguity_follows_the_design_table() {
        let r = rules(&["*.github.com"], &["blocked.net"]);
        // Several candidates, verdicts agree.
        let d = run(
            &admission("198.51.100.1", &["api.github.com", "codeload.github.com"]),
            &r,
        );
        assert!(matches!(
            d,
            Decision::Allow {
                ambiguous: true,
                ..
            }
        ));
        // Allowed and denied candidates on one address: ask.
        let d = run(
            &admission("198.51.100.1", &["api.github.com", "blocked.net"]),
            &r,
        );
        assert_eq!(
            d,
            Decision::Prompt(PromptKind::Disagreement(vec![
                ("api.github.com".into(), Some(true)),
                ("blocked.net".into(), Some(false)),
            ]))
        );
        // An allowed candidate does not vouch for an unknown one.
        let adm = admission("198.51.100.1", &["api.github.com", "unknown.org"]);
        let d = run(&adm, &r);
        assert!(matches!(d, Decision::Prompt(PromptKind::Disagreement(_))));
        if let Decision::Prompt(kind) = d {
            assert_eq!(
                kind.subjects(&adm),
                vec!["unknown.org".parse::<HostPattern>().unwrap()]
            );
        }
    }

    #[test]
    fn session_grants_expire() {
        let now = Instant::now();
        let mut session = Session::default();
        session.grant("temp.example", now + Duration::from_secs(300));
        session.grant("203.0.113.9", now + Duration::from_secs(300));
        let adm = admission("198.51.100.1", &["temp.example"]);
        let d = decide(&adm, Mode::Enforce, &Rules::default(), &session, now);
        assert!(matches!(
            d,
            Decision::Allow {
                reason: Reason::Session,
                ..
            }
        ));
        let later = now + Duration::from_secs(301);
        let d = decide(&adm, Mode::Enforce, &Rules::default(), &session, later);
        assert_eq!(d, Decision::Prompt(PromptKind::Unknown));
        let unresolved = admission("203.0.113.9", &[]);
        let d = decide(&unresolved, Mode::Enforce, &Rules::default(), &session, now);
        assert!(matches!(d, Decision::Allow { .. }));
    }

    #[test]
    fn unresolved_prompts_are_about_the_address() {
        let adm = admission("::ffff:203.0.113.5", &[]);
        assert_eq!(
            PromptKind::Unresolved.subjects(&adm),
            vec!["203.0.113.5".parse::<HostPattern>().unwrap()]
        );
    }
}
