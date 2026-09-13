//! Network rule entries: exact host, `*.suffix`, IP address, or CIDR.

use std::fmt;
use std::net::IpAddr;
use std::str::FromStr;

use ipnet::IpNet;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::broker::mediator::grammar::{GrammarError, validate_hostname};

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// One entry of an `allow` or `deny` list.
///
/// Names are stored lowercased; matching is case-insensitive.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum HostPattern {
    /// `example.com`: that name only.
    Exact(String),

    /// `*.example.com`: `example.com` and every name below it.
    Suffix(String),

    /// `203.0.113.7` or `2001:db8::1`.
    Address(IpAddr),

    /// `203.0.113.0/24`. Host bits must be zero.
    Network(IpNet),
}

/// Why a rule entry was rejected.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PatternError {
    #[error("{0}")]
    Grammar(#[from] GrammarError),

    #[error("not a valid CIDR range")]
    Cidr,

    #[error("the CIDR range has host bits set; did you mean {0}?")]
    HostBits(String),

    #[error("the last label is numeric, so this is neither a hostname nor a valid IP address")]
    NumericTld,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl HostPattern {
    /// Whether this entry matches by name rather than by address.
    pub fn is_name(&self) -> bool {
        matches!(self, Self::Exact(_) | Self::Suffix(_))
    }

    /// Whether `name` is matched by this entry. Address entries never match names.
    pub fn matches_name(&self, name: &str) -> bool {
        let name = name.to_ascii_lowercase();
        match self {
            Self::Exact(exact) => name == *exact,
            Self::Suffix(suffix) => {
                name == *suffix
                    || name
                        .strip_suffix(suffix.as_str())
                        .is_some_and(|prefix| prefix.ends_with('.'))
            }
            Self::Address(_) | Self::Network(_) => false,
        }
    }

    /// Whether `address` is matched by this entry. Name entries never match addresses.
    pub fn matches_address(&self, address: IpAddr) -> bool {
        let address = address.to_canonical();
        match self {
            Self::Address(own) => own.to_canonical() == address,
            Self::Network(net) => net.contains(&address),
            Self::Exact(_) | Self::Suffix(_) => false,
        }
    }
}

//--------------------------------------------------------------------------------------------------
// Trait Implementations
//--------------------------------------------------------------------------------------------------

impl FromStr for HostPattern {
    type Err = PatternError;

    fn from_str(input: &str) -> Result<Self, Self::Err> {
        if input.contains('/') {
            let net: IpNet = input.parse().map_err(|_| PatternError::Cidr)?;
            if net.trunc() != net {
                return Err(PatternError::HostBits(net.trunc().to_string()));
            }
            return Ok(Self::Network(net));
        }
        if let Ok(address) = input.parse::<IpAddr>() {
            return Ok(Self::Address(address));
        }

        let (name, wildcard) = match input.strip_prefix("*.") {
            Some(rest) => (rest, true),
            None => (input, false),
        };
        let name = validate_hostname(name.as_bytes())?;
        let last_label = name.rsplit('.').next().unwrap_or(name);
        if last_label.bytes().all(|b| b.is_ascii_digit()) {
            return Err(PatternError::NumericTld);
        }

        let name = name.to_ascii_lowercase();
        Ok(if wildcard {
            Self::Suffix(name)
        } else {
            Self::Exact(name)
        })
    }
}

impl fmt::Display for HostPattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Exact(name) => f.write_str(name),
            Self::Suffix(name) => write!(f, "*.{name}"),
            Self::Address(address) => write!(f, "{address}"),
            Self::Network(net) => write!(f, "{net}"),
        }
    }
}

impl TryFrom<String> for HostPattern {
    type Error = PatternError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<HostPattern> for String {
    fn from(pattern: HostPattern) -> Self {
        pattern.to_string()
    }
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern(s: &str) -> HostPattern {
        s.parse().unwrap_or_else(|e| panic!("{s}: {e}"))
    }

    #[test]
    fn parses_each_kind() {
        assert_eq!(
            pattern("Example.COM"),
            HostPattern::Exact("example.com".into())
        );
        assert_eq!(
            pattern("*.github.com"),
            HostPattern::Suffix("github.com".into())
        );
        assert_eq!(
            pattern("203.0.113.7"),
            HostPattern::Address("203.0.113.7".parse().unwrap())
        );
        assert_eq!(
            pattern("2001:db8::1"),
            HostPattern::Address("2001:db8::1".parse().unwrap())
        );
        assert_eq!(
            pattern("203.0.113.0/24"),
            HostPattern::Network("203.0.113.0/24".parse().unwrap())
        );
    }

    #[test]
    fn round_trips_through_display() {
        for s in [
            "example.com",
            "*.github.com",
            "203.0.113.7",
            "2001:db8::/32",
        ] {
            assert_eq!(pattern(s).to_string(), s);
        }
    }

    #[test]
    fn rejects_malformed_entries() {
        for s in [
            "",
            "*",
            "*.",
            "**.example.com",
            "foo.*.com",
            "exa mple.com",
            "example.com\0",
            "https://example.com",
            "example.com:443",
            "203.0.113.7/24",
            "203.0.113.0/33",
            "1.2.3",
            "999.1.1.1",
            "b\u{fc}cher.de",
        ] {
            assert!(
                s.parse::<HostPattern>().is_err(),
                "{s:?} should be rejected"
            );
        }
        assert_eq!(
            "203.0.113.7/24".parse::<HostPattern>(),
            Err(PatternError::HostBits("203.0.113.0/24".into()))
        );
    }

    #[test]
    fn suffix_matches_apex_and_subdomains_only() {
        let p = pattern("*.github.com");
        assert!(p.matches_name("github.com"));
        assert!(p.matches_name("api.github.com"));
        assert!(p.matches_name("A.B.GitHub.com"));
        assert!(!p.matches_name("evilgithub.com"));
        assert!(!p.matches_name("github.com.evil.net"));
        assert!(!p.matches_address("140.82.121.3".parse().unwrap()));
    }

    #[test]
    fn exact_matches_only_that_name() {
        let p = pattern("example.com");
        assert!(p.matches_name("EXAMPLE.com"));
        assert!(!p.matches_name("www.example.com"));
    }

    #[test]
    fn address_rules_match_addresses() {
        assert!(pattern("203.0.113.0/24").matches_address("203.0.113.99".parse().unwrap()));
        assert!(!pattern("203.0.113.0/24").matches_address("203.0.114.1".parse().unwrap()));
        assert!(pattern("203.0.113.7").matches_address("::ffff:203.0.113.7".parse().unwrap()));
        assert!(!pattern("203.0.113.7").matches_name("203.0.113.7"));
    }
}
