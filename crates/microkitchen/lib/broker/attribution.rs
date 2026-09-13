//! Which guest process opened a flow (design §8).
//!
//! Display only: the origin is shown to the operator and written to the audit
//! log. It never reaches the decision engine, whose input
//! ([`super::decision::Admission`]) has no field for it, because the guest
//! could present whatever process table it likes.

use std::fmt;
use std::net::IpAddr;
use std::time::Duration;

use microsandbox::Sandbox;
use serde::{Deserialize, Serialize};

use super::protocol::Transport;

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

/// The helper's name in the guest (`/.msb/scripts`, on `PATH`).
pub const SCRIPT_NAME: &str = "mk-whodial";

pub const SCRIPT: &str = include_str!("../../../../scripts/guest/whodial.sh");

/// A lookup never holds up the prompt longer than this.
pub const DEADLINE: Duration = Duration::from_secs(1);

const MAX_NAME: usize = 64;

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Origin {
    pub pid: u32,
    /// The process's short name (`comm`), never its command line.
    pub name: String,
}

/// The helper's output contract.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Report {
    found: bool,
    #[serde(default)]
    pid: Option<u32>,
    #[serde(default)]
    name: Option<String>,
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// Parse the helper's output strictly; anything unexpected is "not found".
pub fn parse(output: &str) -> Option<Origin> {
    let report: Report = serde_json::from_str(output.trim()).ok()?;
    if !report.found {
        return None;
    }
    let (pid, name) = (report.pid?, report.name?);
    let valid = pid > 0
        && !name.is_empty()
        && name.len() <= MAX_NAME
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._+-?".contains(&b));
    valid.then_some(Origin { pid, name })
}

/// Ask the sandbox which process holds the socket to `address:port`.
/// `None` on any failure, timeout, or when the sandbox has no helper.
pub async fn lookup(
    sandbox: &str,
    transport: Transport,
    address: IpAddr,
    port: u16,
) -> Option<Origin> {
    let transport = match transport {
        Transport::Tcp => "tcp",
        Transport::Udp => "udp",
    };
    let args = [
        transport.to_owned(),
        address.to_canonical().to_string(),
        port.to_string(),
    ];
    let run = async {
        let handle = Sandbox::get(sandbox).await.ok()?;
        let connected = handle.connect_with_timeout(DEADLINE).await.ok()?;
        let output = connected.exec(SCRIPT_NAME, args).await.ok()?;
        if !output.status().success {
            return None;
        }
        parse(&output.stdout().ok()?)
    };
    match tokio::time::timeout(DEADLINE, run).await {
        Ok(origin) => origin,
        Err(_) => {
            tracing::debug!(sandbox, "process attribution timed out");
            None
        }
    }
}

//--------------------------------------------------------------------------------------------------
// Trait Implementations
//--------------------------------------------------------------------------------------------------

impl fmt::Display for Origin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "pid {} ({})", self.pid, self.name)
    }
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::PathBuf;
    use std::process::Command;

    use super::*;

    const NOT_FOUND: &str = "{\"found\":false}";

    /// A fake `/proc` tree for the helper.
    struct Proc {
        dir: tempfile::TempDir,
    }

    impl Proc {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            fs::create_dir(dir.path().join("proc")).unwrap();
            fs::write(dir.path().join("whodial.sh"), SCRIPT).unwrap();
            Self { dir }
        }

        fn root(&self) -> PathBuf {
            self.dir.path().join("proc")
        }

        /// A process in network namespace `netns` holding `sockets` (inodes).
        fn process(&self, pid: u32, netns: u64, comm: &str, sockets: &[u64]) {
            let dir = self.root().join(pid.to_string());
            fs::create_dir_all(dir.join("ns")).unwrap();
            fs::create_dir_all(dir.join("fd")).unwrap();
            fs::create_dir_all(dir.join("net")).unwrap();
            symlink(format!("net:[{netns}]"), dir.join("ns/net")).unwrap();
            fs::write(dir.join("comm"), format!("{comm}\n")).unwrap();
            symlink("/dev/null", dir.join("fd/0")).unwrap();
            for (i, inode) in sockets.iter().enumerate() {
                symlink(
                    format!("socket:[{inode}]"),
                    dir.join(format!("fd/{}", i + 3)),
                )
                .unwrap();
            }
        }

        /// `/proc/<pid>/net/<table>` with a header line and `rows`.
        fn table(&self, pid: u32, table: &str, rows: &[String]) {
            let mut text = String::from(
                "  sl  local_address rem_address   st tx_queue rx_queue tr tm->when retrnsmt   uid  timeout inode\n",
            );
            for row in rows {
                text.push_str(row);
                text.push('\n');
            }
            fs::write(self.root().join(format!("{pid}/net/{table}")), text).unwrap();
        }

        fn run(&self, args: &[&str]) -> String {
            let output = Command::new("sh")
                .arg(self.dir.path().join("whodial.sh"))
                .args(args)
                .env("PROC_ROOT", self.root())
                .output()
                .unwrap();
            assert!(output.status.success(), "{output:?}");
            String::from_utf8(output.stdout).unwrap().trim().to_owned()
        }

        fn lookup(&self, args: &[&str]) -> Option<Origin> {
            parse(&self.run(args))
        }
    }

    fn row(remote: &str, state: &str, inode: u64) -> String {
        format!(
            "   0: 0100007F:9C40 {remote} {state} 00000000:00000000 00:00000000 00000000     0        0 {inode} 1 0000000000000000 100 0 0 10 0"
        )
    }

    fn origin(pid: u32, name: &str) -> Option<Origin> {
        Some(Origin {
            pid,
            name: name.into(),
        })
    }

    #[test]
    fn a_connecting_tcp_socket_is_attributed() {
        let p = Proc::new();
        p.process(412, 1, "node", &[5001]);
        // 140.82.121.3:443, SYN_SENT.
        p.table(412, "tcp", &[row("0379528C:01BB", "02", 5001)]);
        assert_eq!(
            p.lookup(&["tcp", "140.82.121.3", "443"]),
            origin(412, "node")
        );
        assert_eq!(p.run(&["tcp", "140.82.121.3", "80"]), NOT_FOUND);
    }

    #[test]
    fn listening_sockets_are_ignored() {
        let p = Proc::new();
        p.process(7, 1, "server", &[42]);
        p.table(7, "tcp", &[row("0379528C:01BB", "0A", 42)]);
        assert_eq!(p.run(&["tcp", "140.82.121.3", "443"]), NOT_FOUND);
    }

    #[test]
    fn ipv6_words_are_byte_swapped() {
        let p = Proc::new();
        p.process(9, 1, "curl", &[77]);
        // 2001:db8::1 port 443.
        p.table(
            9,
            "tcp6",
            &[row("B80D0120000000000000000001000000:01BB", "02", 77)],
        );
        assert_eq!(p.lookup(&["tcp", "2001:db8::1", "443"]), origin(9, "curl"));
        assert_eq!(
            p.lookup(&["tcp", "2001:0DB8:0:0:0:0:0:1", "443"]),
            origin(9, "curl")
        );
    }

    #[test]
    fn ipv4_destinations_match_mapped_ipv6_sockets() {
        let p = Proc::new();
        p.process(9, 1, "java", &[88]);
        // ::ffff:1.2.3.4 port 80.
        p.table(
            9,
            "tcp6",
            &[row("0000000000000000FFFF000004030201:0050", "01", 88)],
        );
        assert_eq!(p.lookup(&["tcp", "1.2.3.4", "80"]), origin(9, "java"));
    }

    #[test]
    fn sockets_in_other_namespaces_are_found_there() {
        let p = Proc::new();
        // Host namespace: pid 50 holds an unrelated socket with the same inode.
        p.process(1, 100, "systemd", &[]);
        p.process(50, 100, "decoy", &[7000]);
        p.table(1, "tcp", &[]);
        // A container namespace: pid 900 connects.
        p.process(900, 200, "curl", &[7000]);
        p.table(900, "tcp", &[row("04030201:01BB", "02", 7000)]);
        assert_eq!(p.lookup(&["tcp", "1.2.3.4", "443"]), origin(900, "curl"));
    }

    #[test]
    fn only_connected_udp_sockets_are_found() {
        // Every process in a namespace sees the same table; the script reads
        // it through the namespace's first process.
        let p = Proc::new();
        p.process(3, 1, "ntpd", &[10]);
        let unconnected = row("00000000:0000", "07", 10);
        p.table(3, "udp", std::slice::from_ref(&unconnected));
        assert_eq!(p.run(&["udp", "1.2.3.4", "123"]), NOT_FOUND);

        p.process(4, 1, "quic", &[11]);
        p.table(3, "udp", &[unconnected, row("04030201:007B", "01", 11)]);
        assert_eq!(p.lookup(&["udp", "1.2.3.4", "123"]), origin(4, "quic"));
    }

    #[test]
    fn malformed_input_is_not_found() {
        let p = Proc::new();
        p.process(5, 1, "curl", &[12]);
        p.table(
            5,
            "tcp",
            &[
                "garbage".into(),
                row("04030201:01BB", "02", 0),
                row("04030201:01BB", "02", 12),
            ],
        );
        assert_eq!(p.lookup(&["tcp", "1.2.3.4", "443"]), origin(5, "curl"));
        for args in [
            &["icmp", "1.2.3.4", "443"][..],
            &["tcp", "1.2.3.999", "443"],
            &["tcp", "1.2.3.4", "99999"],
            &["tcp", "1.2.3.4", "44x"],
            &["tcp", "1::2::3", "443"],
            &["tcp", "example.com", "443"],
            &["tcp", "1.2.3.4"],
        ] {
            assert_eq!(p.run(args), NOT_FOUND, "{args:?}");
        }
    }

    #[test]
    fn names_are_reduced_to_safe_characters() {
        let p = Proc::new();
        p.process(6, 1, "evil\"},{\"x", &[13]);
        p.table(6, "tcp", &[row("04030201:01BB", "02", 13)]);
        assert_eq!(p.lookup(&["tcp", "1.2.3.4", "443"]), origin(6, "evilx"));
    }

    #[test]
    fn parsing_is_strict() {
        assert_eq!(
            parse("{\"found\":true,\"pid\":3,\"name\":\"curl\"}\n"),
            origin(3, "curl")
        );
        for bad in [
            "",
            "{\"found\":false}",
            "{\"found\":true}",
            "{\"found\":true,\"pid\":0,\"name\":\"x\"}",
            "{\"found\":true,\"pid\":3,\"name\":\"\"}",
            "{\"found\":true,\"pid\":3,\"name\":\"a b\"}",
            "{\"found\":true,\"pid\":3,\"name\":\"x\",\"cmdline\":\"curl -u secret\"}",
            "not json",
        ] {
            assert_eq!(parse(bad), None, "{bad}");
        }
    }
}
