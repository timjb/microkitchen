//! UDP flows go through the same decisions (needs a microVM and internet).

use test_utils::{TestKitchen, mk_test, names_of, stdout};

const NTP_HOST: &str = "time.cloudflare.com";

/// Resolves time.cloudflare.com (through the observer), then sends SNTP
/// requests over a connected UDP socket until one is answered, giving up
/// after `argv[1]` tries of three seconds each.
const NTP_CLIENT: &str = r#"
import socket, sys
address = socket.getaddrinfo("time.cloudflare.com", 123, socket.AF_INET, socket.SOCK_DGRAM)[0][4]
s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
s.settimeout(3)
s.connect(address)
for _ in range(int(sys.argv[1])):
    s.send(b"\x1b" + 47 * b"\0")
    try:
        print(len(s.recv(512)))
        sys.exit(0)
    except socket.timeout:
        pass
sys.exit(1)
"#;

fn ntp_client(tries: u32) -> Vec<String> {
    ["python3", "-c", NTP_CLIENT, &tries.to_string()]
        .map(String::from)
        .to_vec()
}

#[mk_test]
async fn udp_destinations_prompt_with_their_name() {
    let k = TestKitchen::new(env!("CARGO_BIN_EXE_microkitchen"));
    k.write("mise.toml", "[_.microkitchen]\ncpus = 1\nmemory = \"1G\"\n");
    k.up();

    // Enough tries to outlast the approval.
    let flow = k.spawn_exec(ntp_client(40));
    let pending = k.wait_for_pending(|p| {
        p["transport"] == "udp" && names_of(p).contains(&NTP_HOST.to_string())
    });
    assert_eq!(pending["port"], 123);
    k.decide(&pending, "allow");
    let output = flow.join().unwrap();
    assert_eq!(stdout(&output).trim(), "48", "{output:?}");
}

#[mk_test]
async fn denied_udp_destinations_stay_quiet() {
    let k = TestKitchen::new(env!("CARGO_BIN_EXE_microkitchen"));
    k.write(
        "mise.toml",
        &format!("[_.microkitchen]\ncpus = 1\nmemory = \"1G\"\n\n[_.microkitchen.network]\ndeny = [\"{NTP_HOST}\"]\n"),
    );
    k.up();
    let output = k.exec_owned(ntp_client(3));
    assert!(!output.status.success(), "{output:?}");
    // The guest's own time sync (ntp.ubuntu.com) may prompt; this flow must not.
    let pending: Vec<_> = k
        .pending()
        .into_iter()
        .filter(|p| names_of(p).contains(&NTP_HOST.to_string()))
        .collect();
    assert!(pending.is_empty(), "{pending:#?}");
}
