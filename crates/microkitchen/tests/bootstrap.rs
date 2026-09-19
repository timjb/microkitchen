//! `mise bootstrap` in the guest (needs a microVM and internet access).

use serde_json::Value;
use test_utils::{TestKitchen, mk_test, stdout};

const KITCHEN: &str = r#"[tools]
jq = "1.7.1"

[tasks.bootstrap]
run = "echo ran > $HOME/bootstrap-marker"

[_.microkitchen]
cpus = 1
memory = "1G"
"#;

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

#[mk_test]
async fn bootstrap_installs_tools_and_runs_the_task() {
    let k = TestKitchen::new(env!("CARGO_BIN_EXE_microkitchen"));
    k.write("mise.toml", KITCHEN);
    let first = k.up();
    assert!(stderr(&first).contains("running mise bootstrap"));

    // Tools are on PATH in exec, from any directory (global config + shims).
    assert_eq!(stdout(&k.exec(&["jq", "--version"])).trim(), "jq-1.7.1");
    let elsewhere = k.exec(&["sh", "-c", "cd /tmp && jq --version"]);
    assert_eq!(stdout(&elsewhere).trim(), "jq-1.7.1");
    // The bootstrap task runs as chef, in chef's home.
    assert_eq!(
        stdout(&k.exec(&["stat", "-c", "%U %n", "/home/chef/bootstrap-marker"])).trim(),
        "chef /home/chef/bootstrap-marker"
    );
    assert_eq!(
        stdout(&k.exec(&["cat", "/home/chef/bootstrap-marker"])).trim(),
        "ran"
    );

    let status: Value = serde_json::from_str(&stdout(&k.run("", &["status", "--json"]))).unwrap();
    let name = status["name"].as_str().unwrap().to_owned();
    let state: Value = serde_json::from_str(
        &std::fs::read_to_string(k.home().join("sandboxes").join(&name).join("state.json"))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(state["bootstrapped"], true);
    let log =
        std::fs::read_to_string(k.home().join("logs").join(&name).join("bootstrap.log")).unwrap();
    assert!(log.contains("jq"), "{log}");

    // Once bootstrapped, `up` does not run it again; `bootstrap` does.
    let second = k.up();
    assert!(
        !stderr(&second).contains("running mise bootstrap"),
        "{}",
        stderr(&second)
    );
    let again = k.run("", &["bootstrap"]);
    assert!(again.status.success(), "{}", stderr(&again));
}

#[mk_test]
async fn failed_bootstrap_is_retried() {
    let k = TestKitchen::new(env!("CARGO_BIN_EXE_microkitchen"));
    k.write(
        "mise.toml",
        "[tasks.bootstrap]\nrun = \"test -f $HOME/allow-bootstrap\"\n\n[_.microkitchen]\ncpus = 1\nmemory = \"1G\"\n",
    );
    k.track_sandbox();
    let failed = k.run("", &["up", "--no-shell"]);
    assert!(!failed.status.success());
    assert!(
        stderr(&failed).contains("microkitchen bootstrap"),
        "{}",
        stderr(&failed)
    );

    // The sandbox stays up; fix the cause and retry.
    assert!(
        k.exec(&["touch", "/home/chef/allow-bootstrap"])
            .status
            .success()
    );
    let retried = k.run("", &["bootstrap"]);
    assert!(retried.status.success(), "{}", stderr(&retried));
    let up = k.up();
    assert!(!stderr(&up).contains("running mise bootstrap"));
}
