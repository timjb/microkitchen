//! Which names a sandbox resolved to which addresses (design §6).
//!
//! One store per sandbox: bindings are never shared, or one sandbox's lookup
//! would launder another's connection. Expiry bounds memory; it is not a
//! security control.
//!
//! Names are kept as *chains*: the query name of one lookup followed by its
//! CNAME targets. A chain is one candidate for the decision engine, so a
//! CDN alias does not count as a second, unknown destination.

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

/// Guests cache DNS well past the TTL, so bindings live at least this long.
pub const TTL_FLOOR: Duration = Duration::from_secs(60 * 60);

pub const TTL_CAP: Duration = Duration::from_secs(24 * 60 * 60);

pub const DEFAULT_CAPACITY: usize = 10_000;

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// One lookup's names: the query name first, then the CNAME chain.
pub type Chain = Vec<String>;

#[derive(Debug)]
struct Entry {
    chains: Vec<Chain>,
    expires_at: Instant,
    last_seen: Instant,
}

#[derive(Debug)]
pub struct BindingStore {
    entries: HashMap<IpAddr, Entry>,
    capacity: usize,
    evictions: u64,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl BindingStore {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: HashMap::new(),
            capacity: capacity.max(1),
            evictions: 0,
        }
    }

    /// Bind `chain` to `address` for `ttl` (clamped to the floor and cap).
    pub fn record(&mut self, address: IpAddr, chain: Chain, ttl: Duration, now: Instant) {
        if chain.is_empty() {
            return;
        }
        let address = address.to_canonical();
        let expires_at = now + ttl.clamp(TTL_FLOOR, TTL_CAP);
        let entry = self.entries.entry(address).or_insert_with(|| Entry {
            chains: Vec::new(),
            expires_at,
            last_seen: now,
        });
        if !entry.chains.contains(&chain) {
            entry.chains.push(chain);
        }
        entry.expires_at = entry.expires_at.max(expires_at);
        entry.last_seen = now;

        while self.entries.len() > self.capacity {
            let oldest = self
                .entries
                .iter()
                .min_by_key(|(_, e)| e.last_seen)
                .map(|(a, _)| *a)
                .expect("store is not empty");
            self.entries.remove(&oldest);
            self.evictions += 1;
            if self.evictions.is_power_of_two() {
                tracing::warn!(
                    evictions = self.evictions,
                    capacity = self.capacity,
                    "binding store is evicting; the sandbox resolves an unusual number of names"
                );
            }
        }
    }

    /// Chains bound to `address`, refreshing the binding. Empty when unknown or expired.
    pub fn lookup(&mut self, address: IpAddr, now: Instant) -> Vec<Chain> {
        let address = address.to_canonical();
        let Some(entry) = self.entries.get_mut(&address) else {
            return Vec::new();
        };
        if entry.expires_at <= now {
            self.entries.remove(&address);
            return Vec::new();
        }
        entry.last_seen = now;
        entry.expires_at = entry.expires_at.max(now + TTL_FLOOR);
        entry.chains.clone()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Live bindings (all names, in order) with their remaining lifetime.
    pub fn snapshot(&self, now: Instant) -> Vec<(IpAddr, Vec<String>, Duration)> {
        let mut out: Vec<_> = self
            .entries
            .iter()
            .filter(|(_, e)| e.expires_at > now)
            .map(|(a, e)| (*a, flatten(&e.chains), e.expires_at - now))
            .collect();
        out.sort_by_key(|(a, _, _)| *a);
        out
    }
}

impl Default for BindingStore {
    fn default() -> Self {
        Self::new(DEFAULT_CAPACITY)
    }
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// Every name of `chains` once, in order.
pub fn flatten(chains: &[Chain]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for name in chains.iter().flatten() {
        if !names.contains(name) {
            names.push(name.clone());
        }
    }
    names
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    fn chain(names: &[&str]) -> Chain {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn keeps_chains_per_lookup() {
        let now = Instant::now();
        let mut store = BindingStore::default();
        let cdn = chain(&["api.example.com", "example.map.cdn.net"]);
        store.record(ip("192.0.2.1"), cdn.clone(), Duration::from_secs(30), now);
        store.record(ip("192.0.2.1"), cdn.clone(), Duration::from_secs(30), now);
        store.record(
            ip("192.0.2.1"),
            chain(&["other.example.com"]),
            Duration::from_secs(30),
            now,
        );
        assert_eq!(
            store.lookup(ip("192.0.2.1"), now),
            vec![cdn, chain(&["other.example.com"])]
        );
        assert!(store.lookup(ip("192.0.2.2"), now).is_empty());
        assert_eq!(
            store.lookup(ip("::ffff:192.0.2.1"), now).len(),
            2,
            "mapped addresses are canonical"
        );
        assert_eq!(
            store.snapshot(now)[0].1,
            chain(&[
                "api.example.com",
                "example.map.cdn.net",
                "other.example.com"
            ])
        );
    }

    #[test]
    fn ttl_has_a_floor_and_a_cap() {
        let now = Instant::now();
        let mut store = BindingStore::default();
        store.record(
            ip("192.0.2.1"),
            chain(&["a.com"]),
            Duration::from_secs(5),
            now,
        );
        let before_floor = now + TTL_FLOOR - Duration::from_secs(1);
        assert_eq!(store.lookup(ip("192.0.2.1"), before_floor).len(), 1);

        let mut store = BindingStore::default();
        store.record(
            ip("192.0.2.1"),
            chain(&["a.com"]),
            Duration::from_secs(10 * 24 * 3600),
            now,
        );
        let after_cap = now + TTL_CAP + Duration::from_secs(1);
        assert!(store.lookup(ip("192.0.2.1"), after_cap).is_empty());
    }

    #[test]
    fn lookups_refresh_bindings() {
        let now = Instant::now();
        let mut store = BindingStore::default();
        store.record(ip("192.0.2.1"), chain(&["a.com"]), Duration::ZERO, now);
        let later = now + TTL_FLOOR - Duration::from_secs(1);
        assert_eq!(store.lookup(ip("192.0.2.1"), later).len(), 1);
        let even_later = later + TTL_FLOOR - Duration::from_secs(1);
        assert_eq!(store.lookup(ip("192.0.2.1"), even_later).len(), 1);
    }

    #[test]
    fn evicts_least_recently_seen() {
        let now = Instant::now();
        let mut store = BindingStore::new(2);
        store.record(ip("192.0.2.1"), chain(&["a.com"]), Duration::ZERO, now);
        store.record(
            ip("192.0.2.2"),
            chain(&["b.com"]),
            Duration::ZERO,
            now + Duration::from_secs(1),
        );
        store.lookup(ip("192.0.2.1"), now + Duration::from_secs(2));
        store.record(
            ip("192.0.2.3"),
            chain(&["c.com"]),
            Duration::ZERO,
            now + Duration::from_secs(3),
        );
        assert_eq!(store.len(), 2);
        assert!(
            store
                .lookup(ip("192.0.2.2"), now + Duration::from_secs(4))
                .is_empty()
        );
    }

    #[test]
    fn stores_are_independent() {
        let now = Instant::now();
        let mut a = BindingStore::default();
        let b = BindingStore::default();
        a.record(ip("192.0.2.1"), chain(&["a.com"]), Duration::ZERO, now);
        assert!(b.snapshot(now).is_empty());
    }
}
