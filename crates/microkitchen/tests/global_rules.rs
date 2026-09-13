//! `~/.microkitchen/rules.toml` applies to every sandbox, after the kitchen
//! file's own rules (needs a microVM and internet).

use std::fs;

use test_utils::{TestKitchen, mk_test, names_of, stdout};

const REFUSED: &str = "000";

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

fn net(k: &TestKitchen, args: &[&str]) -> String {
    let mut all = vec!["net"];
    all.extend_from_slice(args);
    let output = k.run("", &all);
    assert!(
        output.status.success(),
        "net {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    stdout(&output)
}

#[mk_test]
async fn global_rules_apply_after_the_kitchen_file() {
    let k = TestKitchen::new(env!("CARGO_BIN_EXE_microkitchen"));
    k.write("mise.toml", "[_.microkitchen]\ncpus = 1\nmemory = \"1G\"\n");
    k.up();
    let site = || stdout(&k.exec_owned(curl("https://example.org")));

    // A global allow needs no prompt.
    net(&k, &["allow", "--global", "example.org"]);
    assert!(
        fs::read_to_string(k.home().join("rules.toml"))
            .unwrap()
            .contains("example.org")
    );
    assert!(!k.read("mise.toml").contains("example.org"));
    assert_ne!(site(), REFUSED);
    assert!(k.pending().is_empty());
    assert!(net(&k, &["rules"]).contains("example.org"));

    // The kitchen file comes first: its deny wins.
    net(&k, &["deny", "example.org"]);
    assert_eq!(site(), REFUSED);
    net(&k, &["revoke", "example.org"]);
    assert_ne!(site(), REFUSED);

    // Without the global rule the name prompts again.
    net(&k, &["revoke", "--global", "example.org"]);
    let flow = k.spawn_exec(curl("https://example.org"));
    let pending = k.wait_for_pending(|p| names_of(p).contains(&"example.org".to_string()));
    k.decide(&pending, "deny");
    assert_eq!(stdout(&flow.join().unwrap()), REFUSED);
}
