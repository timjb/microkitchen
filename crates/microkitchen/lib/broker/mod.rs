//! Egress broker: one host daemon mediating DNS, TCP and UDP for every
//! sandbox (see `specs/egress-broker-design.md`).
//!
//! Only the hostname grammar exists so far; configuration validation shares it
//! so that rule entries and SOCKS5 domain names are judged by the same code.

pub mod mediator;
