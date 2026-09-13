//! The audit log: one JSON line per verdict, `~/.microkitchen/broker/audit.log`.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::net::IpAddr;
use std::path::Path;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use super::attribution::Origin;
use super::protocol::Transport;

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

pub struct Audit {
    file: Mutex<Option<File>>,
}

#[derive(Debug, Serialize)]
pub struct AuditEvent<'a> {
    pub sandbox: &'a str,
    pub transport: Transport,
    pub address: IpAddr,
    pub port: u16,
    pub names: &'a [String],
    pub allowed: bool,
    /// Why: the matching rule, `open-mode`, `session`, `prompt:<answer>`, …
    pub source: &'a str,
    /// Several candidate names were bound to the address.
    pub ambiguous: bool,
    /// The guest process behind a prompted flow. Display only: supplied by
    /// the guest, never used to decide.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<&'a Origin>,
}

#[derive(Serialize)]
struct Line<'a> {
    ts: u64,
    #[serde(flatten)]
    event: &'a AuditEvent<'a>,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl Audit {
    /// Open for appending; failures are logged and the broker keeps serving.
    pub fn open(path: &Path) -> Self {
        let file = path
            .parent()
            .map_or(Ok(()), fs::create_dir_all)
            .and_then(|()| OpenOptions::new().create(true).append(true).open(path));
        let file = match file {
            Ok(file) => Some(file),
            Err(error) => {
                tracing::error!(%error, path = %path.display(), "cannot open the audit log");
                None
            }
        };
        Self {
            file: Mutex::new(file),
        }
    }

    pub fn record(&self, event: &AuditEvent<'_>) {
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let Ok(mut line) = serde_json::to_string(&Line { ts, event }) else {
            return;
        };
        line.push('\n');
        if let Some(file) = self.file.lock().unwrap().as_mut()
            && let Err(error) = file.write_all(line.as_bytes())
        {
            tracing::error!(%error, "writing the audit log failed");
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
    fn appends_json_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("broker/audit.log");
        let audit = Audit::open(&path);
        let names = vec!["example.com".to_string()];
        let origin = Origin {
            pid: 412,
            name: "ntpd".into(),
        };
        for allowed in [true, false] {
            audit.record(&AuditEvent {
                sandbox: "mk-a",
                transport: Transport::Udp,
                address: "203.0.113.1".parse().unwrap(),
                port: 123,
                names: &names,
                allowed,
                source: "allow-rule:example.com",
                ambiguous: false,
                origin: (!allowed).then_some(&origin),
            });
        }
        let text = fs::read_to_string(&path).unwrap();
        let lines: Vec<serde_json::Value> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0]["transport"], "udp");
        assert_eq!(lines[1]["allowed"], false);
        assert!(lines[0]["ts"].as_u64().unwrap() > 0);
        assert!(lines[0].get("origin").is_none());
        assert_eq!(lines[1]["origin"]["name"], "ntpd");
    }
}
