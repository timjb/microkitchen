//! Published ports (needs a microVM).

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::time::{Duration, Instant};

use test_utils::{TestKitchen, mk_test};

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn http_get(port: u16, path: &str) -> Option<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    write!(stream, "GET {path} HTTP/1.0\r\n\r\n").ok()?;

    // Keep what arrived even without EOF: microsandbox's port forwarder does
    // not pass the guest's close on to the host connection.
    let mut response = Vec::new();
    let mut buffer = [0u8; 4096];
    while let Ok(read) = stream.read(&mut buffer) {
        if read == 0 {
            break;
        }
        response.extend_from_slice(&buffer[..read]);
    }
    (!response.is_empty()).then(|| String::from_utf8_lossy(&response).into_owned())
}

#[mk_test]
async fn published_tcp_port_is_reachable_from_the_host() {
    let port = free_port();
    let k = TestKitchen::new(env!("CARGO_BIN_EXE_microkitchen"));
    k.write(
        "mise.toml",
        &format!("[_.microkitchen]\ncpus = 1\nmemory = \"1G\"\n\n[_.microkitchen.network]\nports = [\"{port}:8080\"]\n"),
    );
    k.up();

    let server = k.exec(&[
        "sh",
        "-c",
        "setsid -f python3 -m http.server 8080 --directory /etc >/tmp/http.log 2>&1 </dev/null",
    ]);
    assert!(server.status.success(), "{server:?}");

    let deadline = Instant::now() + Duration::from_secs(30);
    let response = loop {
        if let Some(response) = http_get(port, "/hostname").filter(|r| r.contains("200 OK")) {
            break response;
        }
        assert!(Instant::now() < deadline, "port {port} never answered");
        std::thread::sleep(Duration::from_millis(500));
    };
    assert!(response.contains("mk-project-"), "{response}");
}
