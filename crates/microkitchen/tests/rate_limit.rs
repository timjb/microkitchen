//! Too many prompts switch a sandbox to deny-all until `net resume`
//! (needs a microVM and internet).

use std::fs;

use serde_json::Value;
use test_utils::{TestKitchen, mk_test, stdout};

fn curl(url: &str) -> Vec<String> {
    [
        "curl",
        "-s",
        "-o",
        "/dev/null",
        "-w",
        "%{http_code}",
        "--max-time",
        "30",
        url,
    ]
    .map(String::from)
    .to_vec()
}

fn limited(k: &TestKitchen) -> bool {
    let status: Value =
        serde_json::from_str(&stdout(&k.run("", &["broker", "status", "--json"]))).unwrap();
    status["sandboxes"][0]["limited"].as_bool().unwrap()
}

#[mk_test]
async fn a_burst_of_prompts_trips_deny_all() {
    let k = TestKitchen::new(env!("CARGO_BIN_EXE_microkitchen"));
    fs::write(
        k.home().join("config.toml"),
        "[approval]\ndialog = \"none\"\nheadless = \"queue\"\ntimeout_secs = 180\nmax_prompts = 3\nwindow_secs = 600\n",
    )
    .unwrap();
    k.write(
        "mise.toml",
        "[_.microkitchen]\ncpus = 1\nmemory = \"1G\"\n\n[_.microkitchen.network]\nallow = [\"example.com\"]\n",
    );
    k.up();
    assert_eq!(stdout(&k.exec_owned(curl("https://example.com"))), "200");

    // Three prompts are within the limit.
    for address in ["1.1.1.1", "8.8.8.8", "9.9.9.9"] {
        let flow = k.spawn_exec(curl(&format!("http://{address}/")));
        let pending = k.wait_for_pending(|p| p["address"] == address);
        k.decide(&pending, "deny");
        flow.join().unwrap();
    }
    assert!(!limited(&k));

    // The fourth trips deny-all: no prompt, and even allowed names are refused.
    assert_eq!(stdout(&k.exec_owned(curl("http://1.0.0.1/"))), "000");
    assert!(k.pending().is_empty());
    assert!(limited(&k));
    assert_eq!(stdout(&k.exec_owned(curl("https://example.com"))), "000");

    // `net resume` restores rules and prompts.
    let resume = k.run("", &["net", "resume"]);
    assert!(resume.status.success(), "{resume:?}");
    assert!(!limited(&k));
    assert_eq!(stdout(&k.exec_owned(curl("https://example.com"))), "200");
    let flow = k.spawn_exec(curl("http://1.0.0.1/"));
    let pending = k.wait_for_pending(|p| p["address"] == "1.0.0.1");
    k.decide(&pending, "deny");
    flow.join().unwrap();
}
