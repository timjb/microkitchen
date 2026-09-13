//! A hard-coded address the sandbox never resolved (needs a microVM and internet).

use test_utils::{TestKitchen, mk_test, names_of, stdout};

#[mk_test]
async fn unresolved_addresses_are_flagged_and_persisted_as_addresses() {
    let k = TestKitchen::new(env!("CARGO_BIN_EXE_microkitchen"));
    k.write("mise.toml", "[_.microkitchen]\ncpus = 1\nmemory = \"1G\"\n");
    k.up();

    let curl = [
        "curl",
        "-s",
        "-o",
        "/dev/null",
        "-w",
        "%{http_code}",
        "--max-time",
        "90",
        "http://1.1.1.1/",
    ]
    .map(String::from)
    .to_vec();
    let flow = k.spawn_exec(curl.clone());
    let pending = k.wait_for_pending(|p| p["address"] == "1.1.1.1");
    assert_eq!(pending["unresolved"], true);
    assert!(names_of(&pending).is_empty());
    k.decide(&pending, "allow");
    let first = stdout(&flow.join().unwrap());
    assert_ne!(first, "000", "the connection completes after approval");
    assert!(
        k.read("mise.toml").contains("allow = [\"1.1.1.1\"]"),
        "{}",
        k.read("mise.toml")
    );

    // Now allowed by the address rule, without a prompt.
    assert_eq!(stdout(&k.exec_owned(curl)), first);
    assert!(k.pending().is_empty());
}
