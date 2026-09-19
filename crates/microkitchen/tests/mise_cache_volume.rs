//! The mise cache volume is shared by all kitchens (needs a microVM).

use test_utils::{TestKitchen, mk_test, stdout};

const KITCHEN: &str = "[_.microkitchen]\ncpus = 1\nmemory = \"1G\"\n";

#[mk_test]
async fn kitchens_share_the_mise_cache() {
    let bin = env!("CARGO_BIN_EXE_microkitchen");
    let marker = format!("/var/cache/mise/microkitchen-test-{}", std::process::id());

    let a = TestKitchen::new(bin);
    a.write("mise.toml", KITCHEN);
    a.up();
    assert!(
        a.exec(&["mise", "--version"]).status.success(),
        "bootstrap installs mise"
    );
    let write = a.exec(&["sh", "-c", &format!("echo shared > {marker}")]);
    assert!(write.status.success(), "{write:?}");

    let b = TestKitchen::new(bin);
    b.write("mise.toml", KITCHEN);
    b.up();
    let seen = b.exec(&["cat", &marker]);
    let _ = b.exec(&["rm", "-f", &marker]);
    assert_eq!(stdout(&seen).trim(), "shared", "{seen:?}");
}
