//! `remodel` applies kitchen changes to an existing sandbox (needs a microVM
//! and internet).

use std::process::Output;

use serde_json::Value;
use test_utils::{TestKitchen, mk_test, stdout};

const BASE: &str = "[env]\nGREETING = \"hello\"\n\n[_.microkitchen]\ncpus = 1\nmemory = \"1G\"\n";

const CHANGED: &str = "[env]\nGREETING = \"hi\"\nEXTRA = \"1\"\n\n[_.microkitchen]\ncpus = 2\nmemory = \"2G\"\ndisk = \"12G\"\n\n[_.microkitchen.network]\nallow = [\"example.com\"]\n";

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        stdout(output),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn remodel(k: &TestKitchen, extra: &[&str]) -> String {
    let mut args = vec!["remodel", "--yes"];
    args.extend_from_slice(extra);
    let output = k.run("", &args);
    let text = text(&output);
    assert!(output.status.success(), "remodel failed:\n{text}");
    text
}

fn up_to_date(k: &TestKitchen) -> bool {
    let status: Value = serde_json::from_str(&stdout(&k.run("", &["status", "--json"]))).unwrap();
    status["up_to_date"].as_bool().unwrap()
}

fn guest(k: &TestKitchen, script: &str) -> String {
    stdout(&k.exec(&["sh", "-c", script])).trim().to_owned()
}

#[mk_test]
async fn remodel_applies_changes_where_they_can_go() {
    let k = TestKitchen::new(env!("CARGO_BIN_EXE_microkitchen"));
    k.write("mise.toml", BASE);
    k.up();
    let unchanged = remodel(&k, &[]);
    assert!(unchanged.contains("nothing to remodel"), "{unchanged}");

    // Resources, disk, environment and a network rule.
    k.write("mise.toml", CHANGED);
    let out = remodel(&k, &[]);
    for expected in [
        "-GREETING = \"hello\"",
        "+EXTRA = \"1\"",
        "cpus",
        "memory",
        "root_disk_size",
        "network.allow",
        "microkitchen restart",
    ] {
        assert!(out.contains(expected), "missing {expected:?} in:\n{out}");
    }

    // The network rule is live: no prompt.
    let curl = [
        "curl",
        "-s",
        "-o",
        "/dev/null",
        "-w",
        "%{http_code}",
        "--max-time",
        "30",
        "https://example.com",
    ];
    assert_eq!(stdout(&k.exec(&curl)), "200");
    assert!(k.pending().is_empty());

    // The rest after a restart.
    let restart = k.run("", &["restart"]);
    assert!(restart.status.success(), "{}", text(&restart));
    assert_eq!(guest(&k, "nproc"), "2");
    let mem_kib: u64 = guest(&k, "awk '/MemTotal/{print $2}' /proc/meminfo")
        .parse()
        .unwrap();
    assert!(mem_kib > 1_700_000, "MemTotal {mem_kib} kB");
    assert_eq!(guest(&k, "echo $GREETING $EXTRA"), "hi 1");
    assert!(
        !guest(&k, "echo $DOCKER_VERSION").is_empty(),
        "the image's own environment survives"
    );
    let disk_mib: u64 = guest(&k, "df --output=size -BM / | tail -1 | tr -dc 0-9")
        .parse()
        .unwrap();
    assert!(disk_mib > 11_000, "root disk {disk_mib} MiB");
    assert!(up_to_date(&k));

    // A mount needs a new sandbox: reported, not applied.
    k.mkdir("shared");
    k.write("shared/hello.txt", "from the host\n");
    k.write(
        "mise.toml",
        &CHANGED.replace(
            "disk = \"12G\"\n",
            "disk = \"12G\"\nmounts = [\"./shared:/shared\"]\n",
        ),
    );
    let out = remodel(&k, &[]);
    assert!(
        out.contains("mounts") && out.contains("--recreate"),
        "{out}"
    );
    assert!(!up_to_date(&k));
    assert!(
        !k.exec(&["test", "-e", "/shared/hello.txt"])
            .status
            .success()
    );

    // --recreate applies it.
    remodel(&k, &["--recreate"]);
    assert_eq!(guest(&k, "cat /shared/hello.txt"), "from the host");
    assert!(up_to_date(&k));
}
