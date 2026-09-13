//! Pending approvals name the guest process behind a flow (needs a microVM and internet).

use serde_json::Value;
use test_utils::{TestKitchen, mk_test, names_of};

fn curl(url: &str) -> Vec<String> {
    ["curl", "-s", "-o", "/dev/null", "--max-time", "90", url]
        .map(String::from)
        .to_vec()
}

/// The pending approval for `name` once attribution has filled in its origin.
fn pending_with_origin(k: &TestKitchen, name: &str) -> Value {
    k.wait_for_pending(|p| names_of(p).contains(&name.to_string()) && p["origin"].is_object())
}

#[mk_test]
async fn pending_approvals_show_the_process() {
    let k = TestKitchen::new(env!("CARGO_BIN_EXE_microkitchen"));
    // Docker Hub is allowed by rule, so the container image can be pulled.
    k.write(
        "mise.toml",
        "[_.microkitchen]\ncpus = 1\nmemory = \"2G\"\n\n[_.microkitchen.network]\nallow = [\"*.docker.io\", \"*.docker.com\"]\n",
    );
    k.up();

    // A process on the guest.
    let flow = k.spawn_exec(curl("https://www.rust-lang.org"));
    let pending = pending_with_origin(&k, "www.rust-lang.org");
    assert_eq!(pending["origin"]["name"], "curl", "{pending}");
    assert!(pending["origin"]["pid"].as_u64().unwrap() > 1, "{pending}");
    k.decide(&pending, "deny");
    flow.join().unwrap();

    // A process inside a Docker container: another network namespace.
    let pull = k.exec(&["docker", "pull", "-q", "curlimages/curl:8.10.1"]);
    assert!(pull.status.success(), "{pull:?}");
    let mut run = vec![
        "docker".to_string(),
        "run".into(),
        "--rm".into(),
        "curlimages/curl:8.10.1".into(),
    ];
    run.extend(curl("https://crates.io").into_iter().skip(1));
    let flow = k.spawn_exec(run);
    let pending = pending_with_origin(&k, "crates.io");
    assert_eq!(pending["origin"]["name"], "curl", "{pending}");
    k.decide(&pending, "deny");
    flow.join().unwrap();
}
