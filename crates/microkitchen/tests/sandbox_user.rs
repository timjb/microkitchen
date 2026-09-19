//! The sandbox user, chef (needs a microVM and internet).

use serde_json::Value;
use test_utils::{TestKitchen, mk_test, stdout};

const KITCHEN: &str = "[tools]\njq = \"1.7.1\"\n\n[_.microkitchen]\ncpus = 1\nmemory = \"1G\"\nmounts = [\"./work:/work\"]\n";

const DECLARED: &str =
    "[_.microkitchen]\ncpus = 1\nmemory = \"1G\"\n\n[bootstrap.users.chef]\nshell = \"/bin/sh\"\n";

fn guest(k: &TestKitchen, script: &str) -> String {
    stdout(&k.exec(&["sh", "-c", script])).trim().to_owned()
}

#[mk_test]
async fn chef_is_the_default_user() {
    let k = TestKitchen::new(env!("CARGO_BIN_EXE_microkitchen"));
    k.mkdir("work");
    k.write("work/host.txt", "from the host\n");
    k.write("mise.toml", KITCHEN);
    k.up();

    assert_eq!(
        guest(&k, "id -un; echo $HOME; pwd"),
        "chef\n/home/chef\n/home/chef"
    );
    let root = k.run("", &["exec", "--root", "--", "id", "-un"]);
    assert_eq!(stdout(&root).trim(), "root");

    // Passwordless sudo and Docker by default.
    assert_eq!(guest(&k, "sudo -n id -u"), "0");
    assert!(k.exec(&["docker", "ps"]).status.success());

    // chef owns the tools and has a global config of its own.
    assert_eq!(guest(&k, "stat -c %U /opt/mise"), "chef");
    let used = k.exec(&["mise", "use", "-g", "jq@1.7.1"]);
    assert!(used.status.success(), "{used:?}");
    assert!(guest(&k, "cat ~/.config/mise/config.toml").contains("jq"));

    // Host files in mounts belong to chef, who can write there.
    assert_eq!(guest(&k, "stat -c %u /work/host.txt"), "1001");
    assert!(k.exec(&["touch", "/work/from-guest"]).status.success());
    assert!(k.project().join("work/from-guest").exists());

    // Docker's daemon is unaffected.
    let dockerd = k.run(
        "",
        &["exec", "--root", "--", "pgrep", "-u", "root", "dockerd"],
    );
    assert!(dockerd.status.success());
}

#[mk_test]
async fn chef_can_be_declared() {
    let k = TestKitchen::new(env!("CARGO_BIN_EXE_microkitchen"));
    k.write("mise.toml", DECLARED);
    k.up();

    // Only what is declared: no sudo group, so no sudo.
    assert_eq!(
        guest(&k, "getent passwd chef | cut -d: -f3,7"),
        "1001:/bin/sh"
    );
    assert!(!k.exec(&["sudo", "-n", "true"]).status.success());

    // Other ids need a new sandbox; the guest keeps the old ones until then.
    k.write(
        "mise.toml",
        &k.read("mise.toml")
            .replace("shell = \"/bin/sh\"\n", "shell = \"/bin/sh\"\nuid = 2000\n"),
    );
    let out = k.run("", &["remodel", "--yes"]);
    let text = format!("{}{}", stdout(&out), String::from_utf8_lossy(&out.stderr));
    assert!(text.contains("1001:1001 -> 2000:1001"), "{text}");
    assert!(text.contains("--recreate"), "{text}");
    assert_eq!(guest(&k, "id -u"), "1001");

    let out = k.run("", &["remodel", "--yes", "--recreate"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(guest(&k, "id -un; id -u"), "chef\n2000");
    let status: Value = serde_json::from_str(&stdout(&k.run("", &["status", "--json"]))).unwrap();
    assert_eq!(status["up_to_date"], true);
}
