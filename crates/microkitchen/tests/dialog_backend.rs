//! Desktop dialogs end to end, against a fake `zenity` and `notify-send`
//! (needs a microVM and internet).

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use test_utils::{TestKitchen, mk_test, names_of, stdout};

const REFUSED: &str = "000";

/// Stands in for `zenity` and `notify-send`: records its arguments, and as
/// zenity answers with the next line of `answers` (a button label; empty =
/// the window was closed; `sleep` = the dialog stays up).
const FAKE: &str = r#"#!/bin/sh
dir=$(dirname "$0")
name=$(basename "$0")
n=$(ls "$dir/calls" | wc -l)
printf '%s\n' "$@" > "$dir/calls/$(printf %03d "$n")-$name"
[ "$name" = zenity ] || exit 0
answer=$(head -n 1 "$dir/answers")
sed -i 1d "$dir/answers"
case $answer in
    sleep) sleep 120; exit 1 ;;
    '') exit 1 ;;
    *) printf '%s\n' "$answer" ;;
esac
"#;

struct FakeDesktop {
    dir: PathBuf,
}

impl FakeDesktop {
    fn install(home: &Path) -> Self {
        let dir = home.join("fake-desktop");
        fs::create_dir_all(dir.join("calls")).unwrap();
        fs::write(dir.join("answers"), "").unwrap();
        for program in ["zenity", "notify-send"] {
            let path = dir.join(program);
            fs::write(&path, FAKE).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        Self { dir }
    }

    fn path(&self) -> String {
        format!(
            "{}:{}",
            self.dir.display(),
            std::env::var("PATH").unwrap_or_default()
        )
    }

    fn answer(&self, answer: &str) {
        let mut answers = OpenOptions::new()
            .append(true)
            .open(self.dir.join("answers"))
            .unwrap();
        writeln!(answers, "{answer}").unwrap();
    }

    /// The recorded arguments of every call to `program`, oldest first.
    fn calls(&self, program: &str) -> Vec<String> {
        let mut names: Vec<_> = fs::read_dir(self.dir.join("calls"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.ends_with(&format!("-{program}")))
            .collect();
        names.sort();
        names
            .iter()
            .map(|n| fs::read_to_string(self.dir.join("calls").join(n)).unwrap())
            .collect()
    }

    fn last_dialog(&self) -> String {
        self.calls("zenity").pop().expect("a dialog was shown")
    }

    fn dialog_open(&self) -> bool {
        Command::new("pgrep")
            .args(["-f", &format!("{}/zenity", self.dir.display())])
            .output()
            .unwrap()
            .status
            .success()
    }
}

fn curl(url: &str) -> Vec<String> {
    [
        "curl",
        "-s",
        "-o",
        "/dev/null",
        "-w",
        "%{http_code}",
        "--max-time",
        "60",
        url,
    ]
    .map(String::from)
    .to_vec()
}

fn wait_until(what: &str, condition: impl Fn() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(15);
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting: {what}");
        std::thread::sleep(Duration::from_millis(200));
    }
}

#[mk_test]
async fn dialogs_answer_prompts() {
    let k = TestKitchen::new(env!("CARGO_BIN_EXE_microkitchen"));
    let desktop = FakeDesktop::install(&k.home());
    fs::write(
        k.home().join("config.toml"),
        "[approval]\ndialog = \"zenity\"\ntimeout_secs = 120\nmax_prompts = 5\n",
    )
    .unwrap();
    k.write("mise.toml", "[_.microkitchen]\ncpus = 1\nmemory = \"1G\"\n");
    // The broker starts here and inherits the fake desktop.
    k.up_with(|c| {
        c.env("PATH", desktop.path()).env("DISPLAY", ":99");
    });

    // Allow: the flow completes, the name is persisted, and the dialog shows
    // the name and the process.
    desktop.answer("Allow");
    assert_ne!(
        stdout(&k.exec_owned(curl("https://www.rust-lang.org"))),
        REFUSED
    );
    let dialog = desktop.last_dialog();
    assert!(dialog.contains("--question"), "{dialog}");
    assert!(
        dialog.contains("Resolved as:  www.rust-lang.org"),
        "{dialog}"
    );
    assert!(
        dialog.contains("(curl)"),
        "attribution reaches the dialog: {dialog}"
    );
    assert!(k.read("mise.toml").contains("\"www.rust-lang.org\""));

    // Allow 5 min: completes, not persisted.
    desktop.answer("Allow 5 min");
    assert_ne!(stdout(&k.exec_owned(curl("https://crates.io"))), REFUSED);
    assert!(!k.read("mise.toml").contains("crates.io"));

    // Closing the dialog denies this flow only.
    desktop.answer("");
    assert_eq!(
        stdout(&k.exec_owned(curl("https://www.python.org"))),
        REFUSED
    );
    assert!(!k.read("mise.toml").contains("python.org"));

    // An address the sandbox never resolved says so; Deny persists it.
    desktop.answer("Deny");
    assert_eq!(stdout(&k.exec_owned(curl("http://1.1.1.1/"))), REFUSED);
    let dialog = desktop.last_dialog();
    assert!(dialog.contains("never resolved this address"), "{dialog}");
    assert!(k.read("mise.toml").contains("\"1.1.1.1\""));

    // Answering from the CLI closes the dialog that is up.
    desktop.answer("sleep");
    let flow = k.spawn_exec(curl("https://example.org"));
    let pending = k.wait_for_pending(|p| names_of(p).contains(&"example.org".to_string()));
    wait_until("the dialog opens", || desktop.dialog_open());
    k.decide(&pending, "temp");
    assert_ne!(stdout(&flow.join().unwrap()), REFUSED);
    wait_until("the dialog closes", || !desktop.dialog_open());

    // The sixth prompt trips the rate limit: no dialog, one notification.
    assert_eq!(desktop.calls("zenity").len(), 5);
    assert_eq!(stdout(&k.exec_owned(curl("http://8.8.8.8/"))), REFUSED);
    assert_eq!(desktop.calls("zenity").len(), 5, "no dialog once limited");
    wait_until("the notification", || {
        !desktop.calls("notify-send").is_empty()
    });
    let notices = desktop.calls("notify-send");
    assert_eq!(notices.len(), 1, "{notices:?}");
    assert!(notices[0].contains("net resume"), "{notices:?}");
}
