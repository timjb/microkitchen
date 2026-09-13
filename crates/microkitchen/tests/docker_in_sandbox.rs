//! Docker inside the sandbox (needs a microVM).

use test_utils::{TestKitchen, mk_test, stdout};

#[mk_test]
async fn docker_runs_inside_the_sandbox() {
    let k = TestKitchen::new(env!("CARGO_BIN_EXE_microkitchen"));
    k.write("mise.toml", "[_.microkitchen]\nmemory = \"2G\"\n");
    k.up();

    let root = k.exec(&["sh", "-c", "grep ' / ' /proc/mounts | cut -d ' ' -f 3"]);
    assert_eq!(
        stdout(&root).trim(),
        "ext4",
        "the root disk must be flat ext4"
    );

    let info = k.exec(&["docker", "info", "--format", "{{.ServerVersion}}"]);
    assert!(info.status.success(), "{info:?}");
    assert!(!stdout(&info).trim().is_empty());

    let hello = k.exec(&["docker", "run", "--rm", "hello-world"]);
    assert!(stdout(&hello).contains("Hello from Docker!"), "{hello:?}");

    let config = k.exec(&["cat", "/root/kitchen/mise.toml"]);
    assert_eq!(stdout(&config), k.read("mise.toml"));
}
