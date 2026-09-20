//! mise `[dotfiles]` and `[bootstrap.files]` staged from the host into the
//! guest (needs a microVM and internet access).

use std::fs;
use std::process::Output;

use serde_json::Value;
use test_utils::{TestKitchen, mk_test, stdout};

const KITCHEN: &str = r#"[dotfiles]
"~/.gitconfig" = { source = "dotfiles/gitconfig", mode = "copy" }
"~/.vimrc" = { source = "dotfiles/vimrc", mode = "symlink" }
"~/.config/nvim" = { source = "dotfiles/nvim", mode = "symlink", exclude = ["notes.md"] }
"~/.inline" = { content = "from content\n" }
"~/.shared" = { source = "../shared/toolrc", mode = "copy" }

[bootstrap.files."/etc/microkitchen.conf"]
source = "etc/sample.conf"
owner = "root"
group = "root"
mode = "0644"

[bootstrap.directories."/srv/data"]
owner = "chef"
mode = "0750"

[_.microkitchen]
cpus = 1
memory = "1G"
"#;

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        stdout(output),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn guest(k: &TestKitchen, script: &str) -> String {
    stdout(&k.exec(&["sh", "-c", script])).trim().to_owned()
}

/// Write the host tree the kitchen file references. `../shared` deliberately
/// sits outside the project, next to it in the fixture's temporary root.
fn write_sources(k: &TestKitchen) {
    k.mkdir("dotfiles/nvim");
    k.mkdir("etc");
    k.write("dotfiles/gitconfig", "[user]\n\tname = chef\n");
    k.write("dotfiles/vimrc", "set number\n");
    k.write("dotfiles/nvim/init.lua", "-- nvim\n");
    k.write("dotfiles/nvim/notes.md", "excluded\n");
    k.write("etc/sample.conf", "staged = true\n");

    let shared = k.project().parent().unwrap().join("shared");
    fs::create_dir_all(&shared).unwrap();
    fs::write(shared.join("toolrc"), "from outside\n").unwrap();
}

fn state(k: &TestKitchen) -> Value {
    let status: Value = serde_json::from_str(&stdout(&k.run("", &["status", "--json"]))).unwrap();
    let name = status["name"].as_str().unwrap();
    serde_json::from_str(
        &fs::read_to_string(k.home().join("sandboxes").join(name).join("state.json")).unwrap(),
    )
    .unwrap()
}

fn up_to_date(k: &TestKitchen) -> bool {
    let status: Value = serde_json::from_str(&stdout(&k.run("", &["status", "--json"]))).unwrap();
    status["up_to_date"].as_bool().unwrap()
}

#[mk_test]
async fn dotfiles_and_system_files_are_staged_and_applied() {
    let k = TestKitchen::new(env!("CARGO_BIN_EXE_microkitchen"));
    write_sources(&k);
    k.write("mise.toml", KITCHEN);
    k.up();

    // Copied and templated entries land in chef's home with the host contents.
    assert_eq!(guest(&k, "cat ~/.gitconfig"), "[user]\n\tname = chef");
    assert_eq!(guest(&k, "cat ~/.inline"), "from content");
    // A source outside the project is staged just like a project-local one.
    assert_eq!(guest(&k, "cat ~/.shared"), "from outside");

    // Symlinked entries point into the staged tree, and chef can edit through
    // them: the staged copy is chef-owned.
    let target = guest(&k, "readlink -f ~/.vimrc");
    assert!(target.starts_with("/opt/kitchen/files/"), "{target}");
    assert_eq!(guest(&k, "stat -c %U ~/.vimrc"), "chef");
    assert!(
        k.exec(&["sh", "-c", "echo 'set ruler' >> ~/.vimrc"])
            .status
            .success(),
        "chef cannot write through a symlinked dotfile"
    );

    // `exclude` is applied on the host, so the file never enters the sandbox.
    assert_eq!(guest(&k, "cat ~/.config/nvim/init.lua"), "-- nvim");
    assert_eq!(
        guest(&k, "test -e ~/.config/nvim/notes.md && echo yes || echo no"),
        "no"
    );
    assert_eq!(
        guest(&k, "find /opt/kitchen/files -name notes.md | wc -l"),
        "0",
        "an excluded file was staged anyway"
    );

    // `[bootstrap.files]` with owner = root works through chef's sudo.
    assert_eq!(
        guest(&k, "stat -c '%U %G %a' /etc/microkitchen.conf"),
        "root root 644"
    );
    assert_eq!(guest(&k, "cat /etc/microkitchen.conf"), "staged = true");
    assert_eq!(guest(&k, "stat -c '%U %a' /srv/data"), "chef 750");

    // The guest config names only absolute guest paths, never host ones.
    let config = guest(&k, "cat /opt/kitchen/mise.toml");
    assert!(
        !config.contains(&k.project().display().to_string()),
        "a host path leaked into the guest:\n{config}"
    );
    let sources: Vec<&str> = config
        .lines()
        .map(str::trim_start)
        .filter(|l| l.starts_with("source =") || l.contains("{ source ="))
        .collect();
    assert_eq!(sources.len(), 5, "{config}");
    for line in sources {
        assert!(
            line.contains("source = \"/opt/kitchen/files/"),
            "source is not an absolute guest path: {line}"
        );
    }
    // mise's own config stays root-owned; only the staged tree is chef's.
    assert_eq!(guest(&k, "stat -c %U /opt/kitchen/mise.toml"), "root");
}

#[mk_test]
async fn editing_a_source_is_applied_by_remodel() {
    let k = TestKitchen::new(env!("CARGO_BIN_EXE_microkitchen"));
    write_sources(&k);
    k.write("mise.toml", KITCHEN);
    k.up();
    assert!(up_to_date(&k));
    let first = state(&k);
    assert!(!first["staged_digest"].as_str().unwrap().is_empty());

    // Change a staged file only; the kitchen file itself is untouched.
    k.write("dotfiles/gitconfig", "[user]\n\tname = edited\n");
    assert!(
        !up_to_date(&k),
        "a changed dotfile must make the sandbox out of date"
    );

    let out = text(&k.run("", &["remodel", "--yes"]));
    assert!(out.contains("staged files"), "{out}");
    assert_eq!(guest(&k, "cat ~/.gitconfig"), "[user]\n\tname = edited");
    assert!(up_to_date(&k));
    assert_ne!(
        state(&k)["staged_digest"],
        first["staged_digest"],
        "the digest must follow the contents"
    );
}

#[mk_test]
async fn a_removed_entry_is_cleaned_out_of_the_guest() {
    let k = TestKitchen::new(env!("CARGO_BIN_EXE_microkitchen"));
    write_sources(&k);
    k.write("mise.toml", KITCHEN);
    k.up();
    assert_eq!(guest(&k, "cat ~/.shared"), "from outside");
    let staged = guest(&k, "ls /opt/kitchen/files | wc -l");
    assert!(staged.parse::<u32>().unwrap() >= 2, "{staged}");

    // Drop the entry whose source lives outside the project.
    let without = KITCHEN
        .lines()
        .filter(|l| !l.starts_with("\"~/.shared\""))
        .collect::<Vec<_>>()
        .join("\n");
    k.write("mise.toml", &format!("{without}\n"));
    let out = text(&k.run("", &["remodel", "--yes"]));
    assert!(out.contains("staged files"), "{out}");

    assert_eq!(
        guest(&k, "find /opt/kitchen/files -name toolrc | wc -l"),
        "0",
        "the staged copy of a removed entry was left behind"
    );
    // What is still referenced survives, and mise.toml is never at risk.
    assert_eq!(guest(&k, "cat ~/.gitconfig"), "[user]\n\tname = chef");
    assert_eq!(
        guest(&k, "test -f /opt/kitchen/mise.toml && echo yes"),
        "yes"
    );
}

#[mk_test]
async fn a_missing_source_is_reported_before_anything_is_created() {
    let k = TestKitchen::new(env!("CARGO_BIN_EXE_microkitchen"));
    k.write(
        "mise.toml",
        "[dotfiles]\n\"~/.gitconfig\" = \"dotfiles/gitconfig\"\n\n[_.microkitchen]\ncpus = 1\nmemory = \"1G\"\n",
    );
    let output = k.run("", &["validate"]);
    let out = text(&output);
    assert!(!output.status.success(), "{out}");
    assert!(out.contains("does not exist"), "{out}");
    assert!(out.contains("dotfiles/gitconfig"), "{out}");

    let up = k.run("", &["up", "--no-shell"]);
    assert!(!up.status.success(), "{}", text(&up));
}

#[mk_test]
async fn validate_lists_what_will_be_staged() {
    let k = TestKitchen::new(env!("CARGO_BIN_EXE_microkitchen"));
    write_sources(&k);
    k.write("mise.toml", KITCHEN);

    let report: Value = serde_json::from_str(&stdout(&k.run("", &["validate", "--json"]))).unwrap();
    assert_eq!(report["valid"], true, "{report}");
    let staged = report["staged"].as_array().expect("a staged array");

    let outside: Vec<&Value> = staged
        .iter()
        .filter(|e| e["outside"].as_bool().unwrap())
        .collect();
    assert_eq!(outside.len(), 1, "{staged:#?}");
    assert!(
        outside[0]["host"]
            .as_str()
            .unwrap()
            .ends_with("shared/toolrc"),
        "{outside:#?}"
    );
    assert!(
        staged.iter().all(|e| e["guest"]
            .as_str()
            .unwrap()
            .starts_with("/opt/kitchen/files/")),
        "{staged:#?}"
    );

    // The text form names both ends and flags what leaves the project.
    let plain = text(&k.run("", &["validate"]));
    assert!(plain.contains("stage"), "{plain}");
    assert!(plain.contains("(outside the project)"), "{plain}");
}

/// A templated dotfile that interpolates a microkitchen secret gets the
/// *placeholder*, not the value: mise renders in the guest, and the guest only
/// ever sees the placeholder. The real value is substituted on the wire, for
/// the secret's allowed hosts only.
#[mk_test]
async fn a_templated_dotfile_holds_the_placeholder_not_the_secret() {
    const REAL_TOKEN: &str = "real-token-value-9c2e";

    let k = TestKitchen::new(env!("CARGO_BIN_EXE_microkitchen"));
    k.mkdir("dotfiles");
    k.write(
        "dotfiles/auth.tmpl",
        "Authorization: Bearer {{ env.TOKEN }}\n",
    );
    k.write(
        "mise.toml",
        r#"[env]
TOKEN = { required = true }

[dotfiles]
"~/.api-auth" = { source = "dotfiles/auth.tmpl", mode = "template" }

[_.microkitchen]
cpus = 1
memory = "1G"

[_.microkitchen.secrets.TOKEN]
allow = ["api.github.com"]
"#,
    );
    k.up_with(|command| {
        command.env("TOKEN", REAL_TOKEN);
    });

    let rendered = guest(&k, "cat ~/.api-auth");
    let placeholder = guest(&k, "printenv TOKEN");
    assert!(!placeholder.is_empty(), "the secret was not injected");
    assert_ne!(placeholder, REAL_TOKEN, "the guest must not see the value");
    assert_eq!(rendered, format!("Authorization: Bearer {placeholder}"));
    assert!(
        !rendered.contains(REAL_TOKEN),
        "the real secret was written to the guest's disk"
    );

    // The staged template is chef's, and holds the template, not the value.
    let staged = guest(&k, "cat /opt/kitchen/files/project/dotfiles/auth.tmpl");
    assert!(staged.contains("{{ env.TOKEN }}"), "{staged}");
    assert!(!staged.contains(REAL_TOKEN), "{staged}");
    assert_eq!(
        guest(
            &k,
            "stat -c %U /opt/kitchen/files/project/dotfiles/auth.tmpl"
        ),
        "chef"
    );
}
