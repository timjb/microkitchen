//! An allowed name cannot launder an address bound to another name (needs a microVM).

use test_utils::{TestKitchen, mk_test, names_of, stdout};

#[mk_test]
async fn allow_rules_do_not_cover_addresses_of_other_names() {
    let k = TestKitchen::new(env!("CARGO_BIN_EXE_microkitchen"));
    k.write(
        "mise.toml",
        "[_.microkitchen]\ncpus = 1\nmemory = \"1G\"\n\n[_.microkitchen.network]\nallow = [\"example.com\"]\n",
    );
    k.up();

    // The guest resolves another name, then connects to its address while
    // claiming the allowed name (SNI/Host example.com).
    let lookup = k.exec(&[
        "sh",
        "-c",
        "getent ahostsv4 www.wikipedia.org | awk 'NR==1{print $1}'",
    ]);
    let address = stdout(&lookup).trim().to_owned();
    assert!(!address.is_empty(), "{lookup:?}");

    let resolve = format!("example.com:443:{address}");
    let flow = k.spawn_exec(
        [
            "curl",
            "-sk",
            "-o",
            "/dev/null",
            "--max-time",
            "90",
            "--resolve",
            &resolve,
            "https://example.com",
        ]
        .map(String::from)
        .to_vec(),
    );
    let pending = k.wait_for_pending(|p| p["address"] == address.as_str());
    let names = names_of(&pending);
    assert!(
        names.contains(&"www.wikipedia.org".to_string()),
        "{names:?}"
    );
    assert!(!names.contains(&"example.com".to_string()), "{names:?}");
    k.decide(&pending, "deny");
    flow.join().unwrap();
}
