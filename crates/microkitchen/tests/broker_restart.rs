//! Egress fails closed while the broker is down; rules survive a restart,
//! session decisions do not (needs a microVM and internet).

use std::time::{Duration, Instant};

use microsandbox::Sandbox;
use serde_json::Value;
use test_utils::{TestKitchen, mk_test, names_of, stdout};

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

fn broker_status(k: &TestKitchen) -> Value {
    serde_json::from_str(&stdout(&k.run("", &["broker", "status", "--json"]))).unwrap()
}

#[mk_test]
async fn the_broker_restarts_without_opening_egress() {
    let k = TestKitchen::new(env!("CARGO_BIN_EXE_microkitchen"));
    k.write(
        "mise.toml",
        "[_.microkitchen]\ncpus = 1\nmemory = \"1G\"\n\n[_.microkitchen.network]\nallow = [\"example.com\"]\n",
    );
    k.up();
    assert_eq!(stdout(&k.exec_owned(curl("https://example.com"))), "200");
    assert!(k.run("", &["net", "temp", "crates.io"]).status.success());
    assert_ne!(stdout(&k.exec_owned(curl("https://crates.io"))), "000");
    let status: Value = serde_json::from_str(&stdout(&k.run("", &["status", "--json"]))).unwrap();
    let name = status["name"].as_str().expect("sandbox name").to_owned();

    // Kill the broker hard.
    let pid = broker_status(&k)["pid"].as_u64().expect("broker pid");
    let kill = std::process::Command::new("kill")
        .args(["-9", &pid.to_string()])
        .output()
        .unwrap();
    assert!(kill.status.success(), "{kill:?}");
    let deadline = Instant::now() + Duration::from_secs(10);
    while broker_status(&k)["running"] == true {
        assert!(Instant::now() < deadline, "the broker did not die");
        std::thread::sleep(Duration::from_millis(200));
    }

    // Without the broker there is no egress at all. Every microkitchen command
    // that touches a sandbox restarts the broker, so reach the guest through
    // the SDK instead.
    let sandbox = Sandbox::get(&name).await.unwrap().connect().await.unwrap();
    let args = curl("https://example.com").into_iter().skip(1);
    let output = sandbox.exec("curl", args).await.unwrap();
    assert_eq!(output.stdout().unwrap(), "000");
    assert_eq!(
        broker_status(&k)["running"],
        false,
        "nothing may have restarted the broker during the check"
    );

    // `up` restarts it; the persistent rule applies again.
    k.up();
    assert_eq!(broker_status(&k)["running"], true);
    assert_eq!(stdout(&k.exec_owned(curl("https://example.com"))), "200");

    // The temporary allow did not survive: crates.io prompts again.
    let flow = k.spawn_exec(curl("https://crates.io"));
    let pending = k.wait_for_pending(|p| names_of(p).contains(&"crates.io".to_string()));
    k.decide(&pending, "deny");
    assert_eq!(stdout(&flow.join().unwrap()), "000");
}
