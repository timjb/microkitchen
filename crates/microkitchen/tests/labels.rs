//! Sandboxes are identified by their labels (needs a microVM).

use microsandbox::Sandbox;
use serde_json::Value;
use test_utils::{TestKitchen, mk_test, stdout};

#[mk_test]
async fn sandbox_is_found_by_its_labels() {
    let k = TestKitchen::new(env!("CARGO_BIN_EXE_microkitchen"));
    k.write("mise.toml", "[_.microkitchen]\ncpus = 1\nmemory = \"1G\"\n");
    k.up();

    let file = k.project().join("mise.toml").to_string_lossy().into_owned();
    let page = Sandbox::list_with(|l| l.label("microkitchen.config", file.clone()))
        .await
        .expect("listing sandboxes");
    assert_eq!(page.sandboxes.len(), 1);
    let handle = &page.sandboxes[0];
    let labels = handle.config().expect("sandbox config").spec.labels;
    assert_eq!(labels["microkitchen.managed"], "true");
    assert_eq!(labels["microkitchen.version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(labels["microkitchen.bootstrapped"], "false");
    assert_eq!(labels["microkitchen.config-hash"].len(), 16);
    let name = handle.name().to_owned();
    let created_at = handle.created_at();

    let list: Value = serde_json::from_str(&stdout(&k.run("", &["list", "--json"]))).unwrap();
    let entry = list
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["name"] == name.as_str())
        .expect("listed");
    assert_eq!(entry["kitchen_file"], file.as_str());
    assert_eq!(entry["status"], "running");

    // A second `up` reuses the sandbox.
    k.up();
    let again = Sandbox::get(&name).await.expect("sandbox still exists");
    assert_eq!(again.created_at(), created_at);

    let status: Value = serde_json::from_str(&stdout(&k.run("", &["status", "--json"]))).unwrap();
    assert_eq!(status["name"], name.as_str());
    assert_eq!(status["up_to_date"], true);

    // Stop and start keep the sandbox.
    assert!(k.run("", &["stop"]).status.success());
    let status: Value = serde_json::from_str(&stdout(&k.run("", &["status", "--json"]))).unwrap();
    assert_eq!(status["status"], "stopped");
    assert!(k.run("", &["start"]).status.success());
    assert_eq!(stdout(&k.exec(&["hostname"])).trim(), name);

    // A config change is reported, not applied.
    k.write("mise.toml", "[_.microkitchen]\ncpus = 2\nmemory = \"1G\"\n");
    let status: Value = serde_json::from_str(&stdout(&k.run("", &["status", "--json"]))).unwrap();
    assert_eq!(status["up_to_date"], false);
}
