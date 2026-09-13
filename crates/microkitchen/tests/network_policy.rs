//! Rules, prompts and decisions for named destinations (needs a microVM and internet).

use test_utils::{TestKitchen, mk_test, names_of, stdout};

const KITCHEN: &str = r#"[_.microkitchen]
cpus = 1
memory = "1G"

[_.microkitchen.network]
allow = ["example.com"]
deny = ["www.wikipedia.org"]
"#;

fn curl(url: &str) -> Vec<String> {
    [
        "curl",
        "-s",
        "-o",
        "/dev/null",
        "-w",
        "%{http_code}",
        "--max-time",
        "90",
        url,
    ]
    .map(String::from)
    .to_vec()
}

#[mk_test]
async fn rules_prompts_and_decisions() {
    let k = TestKitchen::new(env!("CARGO_BIN_EXE_microkitchen"));
    k.write("mise.toml", KITCHEN);
    k.up_with(|c| {
        c.env("MICROKITCHEN_TEMP_ALLOW_SECS", "15");
    });

    // Sites answer with whatever status they like (redirects, 403s): a flow
    // completed if curl got any HTTP status, and was refused if it got none.
    const REFUSED: &str = "000";

    // Allowed and denied names need no prompt.
    assert_eq!(stdout(&k.exec_owned(curl("https://example.com"))), "200");
    let denied = k.exec_owned(curl("https://www.wikipedia.org"));
    assert_eq!(stdout(&denied), REFUSED);
    assert!(k.pending().is_empty(), "rules never prompt");

    // Unknown name → prompt showing the name → allow completes the flow and persists.
    let flow = k.spawn_exec(curl("https://www.rust-lang.org"));
    let pending = k.wait_for_pending(|p| names_of(p).contains(&"www.rust-lang.org".to_string()));
    assert_eq!(pending["transport"], "tcp");
    assert_eq!(pending["unresolved"], false);
    k.decide(&pending, "allow");
    assert_ne!(stdout(&flow.join().unwrap()), REFUSED);
    assert!(k.read("mise.toml").contains("\"www.rust-lang.org\""));

    // Deny refuses the flow and persists to `deny`.
    let flow = k.spawn_exec(curl("https://www.python.org"));
    let pending = k.wait_for_pending(|p| names_of(p).contains(&"www.python.org".to_string()));
    k.decide(&pending, "deny");
    assert_eq!(stdout(&flow.join().unwrap()), REFUSED);
    let text = k.read("mise.toml");
    let deny_line = text.lines().find(|l| l.starts_with("deny")).unwrap();
    assert!(deny_line.contains("\"www.python.org\""), "{text}");

    // Temp allows for a while, is not persisted, then prompts again.
    let flow = k.spawn_exec(curl("https://crates.io"));
    let pending = k.wait_for_pending(|p| names_of(p).contains(&"crates.io".to_string()));
    k.decide(&pending, "temp");
    assert_ne!(stdout(&flow.join().unwrap()), REFUSED);
    assert!(!k.read("mise.toml").contains("crates.io"));
    assert_ne!(stdout(&k.exec_owned(curl("https://crates.io"))), REFUSED);

    std::thread::sleep(std::time::Duration::from_secs(16));
    let flow = k.spawn_exec(curl("https://crates.io"));
    let pending = k.wait_for_pending(|p| names_of(p).contains(&"crates.io".to_string()));
    k.decide(&pending, "deny");
    assert_eq!(stdout(&flow.join().unwrap()), REFUSED);
}
