//! One broker, two sandboxes (needs two microVMs).

use serde_json::Value;
use test_utils::{TestKitchen, mk_test, names_of, stdout};

const KITCHEN: &str = "[_.microkitchen]\ncpus = 1\nmemory = \"1G\"\n";

#[mk_test]
async fn sandboxes_share_a_broker_but_not_bindings() {
    let bin = env!("CARGO_BIN_EXE_microkitchen");
    let a = TestKitchen::new(bin);
    a.write("mise.toml", KITCHEN);
    let b = TestKitchen::with_home(bin, a.home());
    b.write("mise.toml", KITCHEN);
    a.up();
    b.up();

    let status: Value =
        serde_json::from_str(&stdout(&a.run("", &["broker", "status", "--json"]))).unwrap();
    assert_eq!(status["sandboxes"].as_array().unwrap().len(), 2, "{status}");
    let b_name: Value = serde_json::from_str(&stdout(&b.run("", &["status", "--json"]))).unwrap();

    // `a` resolves a name; `b` connects to that address without resolving it.
    let lookup = a.exec(&[
        "sh",
        "-c",
        "getent ahostsv4 example.com | awk 'NR==1{print $1}'",
    ]);
    let address = stdout(&lookup).trim().to_owned();
    let url = format!("http://{address}/");
    let flow = b.spawn_exec(
        ["curl", "-s", "-o", "/dev/null", "--max-time", "90", &url]
            .map(String::from)
            .to_vec(),
    );
    let pending = b.wait_for_pending(|p| p["address"] == address.as_str());
    assert_eq!(pending["sandbox"], b_name["name"]);
    assert_eq!(
        pending["unresolved"], true,
        "a's bindings must not leak into b"
    );
    assert!(names_of(&pending).is_empty());
    b.decide(&pending, "deny");
    flow.join().unwrap();

    // Retiring `a` leaves `b` working.
    assert!(a.run("", &["down"]).status.success());
    assert!(b.exec(&["getent", "hosts", "example.com"]).status.success());
    let status: Value =
        serde_json::from_str(&stdout(&b.run("", &["broker", "status", "--json"]))).unwrap();
    assert_eq!(status["sandboxes"].as_array().unwrap().len(), 1);
}
