//! The broker's admin interface: newline-delimited JSON over
//! `~/.microkitchen/broker/admin.sock`, one request per line.

use std::net::IpAddr;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// Whether the broker mediates a sandbox's egress.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Everything not hard-denied is allowed (used while bootstrapping).
    Open,
    /// Rules, then a human.
    Enforce,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Transport {
    Tcp,
    Udp,
}

/// An operator's answer to a pending approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Answer {
    /// Persist to the kitchen file's `allow`.
    Allow,
    /// Persist to the kitchen file's `deny`.
    Deny,
    /// Allow for a few minutes, this sandbox only, not persisted.
    Temp,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Ping,
    Register {
        name: String,
        kitchen_file: PathBuf,
        mode: Mode,
        /// Ports assigned earlier; a sandbox's endpoints cannot change.
        resolver_port: Option<u16>,
        proxy_port: Option<u16>,
    },
    Retire {
        name: String,
    },
    SetMode {
        name: String,
        mode: Mode,
    },
    /// Allow a host or address for this sandbox, temporarily.
    Grant {
        name: String,
        subject: String,
    },
    List,
    Pending,
    Decide {
        id: u64,
        answer: Answer,
    },
    Bindings {
        name: String,
    },
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub data: serde_json::Value,
}

/// Endpoints handed to the sandbox at creation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registration {
    pub resolver_port: u16,
    pub proxy_port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxInfo {
    pub name: String,
    pub kitchen_file: PathBuf,
    pub mode: Mode,
    pub resolver_port: u16,
    pub proxy_port: u16,
    pub bindings: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingApproval {
    pub id: u64,
    pub sandbox: String,
    pub transport: Transport,
    pub address: IpAddr,
    pub port: u16,
    /// Names this sandbox resolved to the address; empty when it never did.
    pub names: Vec<String>,
    /// The sandbox never resolved this address.
    pub unresolved: bool,
    pub age_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BindingInfo {
    pub address: IpAddr,
    pub names: Vec<String>,
    pub expires_in_secs: u64,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl Response {
    pub fn success(data: impl Serialize) -> Self {
        Self {
            ok: true,
            error: None,
            data: serde_json::to_value(data).unwrap_or(serde_json::Value::Null),
        }
    }

    pub fn failure(error: impl std::fmt::Display) -> Self {
        Self {
            ok: false,
            error: Some(error.to_string()),
            data: serde_json::Value::Null,
        }
    }
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_tagged() {
        let json = serde_json::to_string(&Request::Decide {
            id: 3,
            answer: Answer::Temp,
        })
        .unwrap();
        assert_eq!(json, r#"{"op":"decide","id":3,"answer":"temp"}"#);
        let parsed: Request =
            serde_json::from_str(r#"{"op":"set_mode","name":"a","mode":"open"}"#).unwrap();
        assert!(matches!(
            parsed,
            Request::SetMode {
                mode: Mode::Open,
                ..
            }
        ));
    }
}
