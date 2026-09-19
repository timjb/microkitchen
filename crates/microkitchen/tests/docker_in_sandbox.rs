//! Docker inside the sandbox (needs a microVM).

use test_utils::{TestKitchen, mk_test, stdout};

#[mk_test]
async fn docker_runs_inside_the_sandbox() {
    let k = TestKitchen::new(env!("CARGO_BIN_EXE_microkitchen"));
    // After bootstrap the broker enforces; Docker Hub is allowed by rule.
    k.write(
        "mise.toml",
        "[_.microkitchen]\ncpus = 1\nmemory = \"2G\"\n\n[_.microkitchen.network]\nallow = [\"*.docker.io\", \"*.docker.com\"]\n",
    );
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

    let config = k.exec(&["cat", "/opt/kitchen/mise.toml"]);
    let expected = microkitchen::mise::render::render_guest_config(&k.read("mise.toml")).unwrap();
    assert_eq!(stdout(&config), expected);
}
