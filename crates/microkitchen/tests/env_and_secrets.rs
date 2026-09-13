//! Environment variables and secrets in the guest (needs a microVM).

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::sync::mpsc;
use std::thread;

use test_utils::{TestKitchen, mk_test, stdout};

const REAL_TOKEN: &str = "real-token-value-4f1d";

/// Accepts one HTTP request on host loopback and reports its Authorization header.
fn capture_authorization() -> (u16, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind fixture");
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut authorization = String::new();
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                break;
            }
            if let Some(value) = line.strip_prefix("Authorization: ") {
                authorization = value.trim_end().to_owned();
            }
        }
        let _ = stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
        let _ = tx.send(authorization);
    });
    (port, rx)
}

#[mk_test]
async fn env_vars_and_secrets_reach_the_guest() {
    let k = TestKitchen::new(env!("CARGO_BIN_EXE_microkitchen"));
    k.write(
        "mise.toml",
        r#"[env]
PLAIN = "plain-value"
HOST_ONLY = { required = true }
TOKEN = { required = true }
OPT = { default = "" }

[_.microkitchen]
cpus = 1
memory = "1G"

[_.microkitchen.network]
network = "open"

[_.microkitchen.secrets.TOKEN]
allow = ["api.github.com"]

[_.microkitchen.secrets.OPT]
allow = ["api.figma.com"]
"#,
    );
    k.up_with(|cmd| {
        cmd.env("HOST_ONLY", "from-host")
            .env("TOKEN", REAL_TOKEN)
            .env_remove("OPT");
    });

    assert_eq!(
        stdout(&k.exec(&["printenv", "PLAIN"])).trim(),
        "plain-value"
    );
    assert_eq!(
        stdout(&k.exec(&["printenv", "HOST_ONLY"])).trim(),
        "from-host"
    );
    assert!(
        !k.exec(&["printenv", "OPT"]).status.success(),
        "empty optional variables are not forwarded"
    );

    let placeholder = stdout(&k.exec(&["printenv", "TOKEN"])).trim().to_owned();
    assert!(!placeholder.is_empty());
    assert_ne!(
        placeholder, REAL_TOKEN,
        "the guest must only see a placeholder"
    );

    // A host outside the secret's allow list receives the placeholder unchanged.
    let (port, received) = capture_authorization();
    let request = format!(
        "curl -s -H \"Authorization: Bearer $TOKEN\" http://host.microsandbox.internal:{port}/"
    );
    let response = k.exec(&["sh", "-c", &request]);
    assert_eq!(stdout(&response), "ok", "{response:?}");
    assert_eq!(received.recv().unwrap(), format!("Bearer {placeholder}"));
}
