//! The guest cannot bypass the observed resolver (needs a microVM).

use test_utils::{TestKitchen, mk_test, stdout};

/// A DNS query for example.com sent straight to a public resolver.
/// microsandbox intercepts it and, when policy denies the resolver,
/// replies with a synthesized NXDOMAIN: only real answers count.
const DIRECT_DNS: &str = r#"
import socket
query = bytes.fromhex("abcd01000001000000000000076578616d706c6503636f6d0000010001")
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.settimeout(5)
s.sendto(query, ("1.1.1.1", 53))
try:
    reply = s.recv(512)
    rcode = reply[3] & 0x0F
    answers = int.from_bytes(reply[6:8], "big")
    print("answered" if rcode == 0 and answers > 0 else "blocked")
except socket.timeout:
    print("blocked")
"#;

const DOT: &str = r#"
import socket
try:
    socket.create_connection(("1.1.1.1", 853), timeout=5)
    print("connected")
except OSError:
    print("blocked")
"#;

#[mk_test]
async fn direct_dns_and_dot_are_refused() {
    let k = TestKitchen::new(env!("CARGO_BIN_EXE_microkitchen"));
    k.write("mise.toml", "[_.microkitchen]\ncpus = 1\nmemory = \"1G\"\n");
    k.up();

    let dns = k.exec(&["python3", "-c", DIRECT_DNS]);
    assert_eq!(stdout(&dns).trim(), "blocked", "{dns:?}");
    let dot = k.exec(&["python3", "-c", DOT]);
    assert_eq!(stdout(&dot).trim(), "blocked", "{dot:?}");
    assert!(
        k.pending().is_empty(),
        "refused by microsandbox, never reaching the broker"
    );

    // The sandbox's own resolver still works.
    assert!(k.exec(&["getent", "hosts", "example.com"]).status.success());
}
