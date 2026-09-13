//! Discovery, validation and rule edits against the real `mise` binary.
//!
//! No microVM is needed, so these run with plain `cargo test`.

use std::process::Output;

use serde_json::Value;
use test_utils::TestKitchen;

//--------------------------------------------------------------------------------------------------
// Functions: Helpers
//--------------------------------------------------------------------------------------------------

fn kitchen() -> TestKitchen {
    TestKitchen::new(env!("CARGO_BIN_EXE_microkitchen"))
}

fn parse(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "{error}\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        )
    })
}

fn validate(kitchen: &TestKitchen, cwd: &str) -> (Output, Value) {
    let output = kitchen.run(cwd, &["validate", "--json"]);
    let json = parse(&output);
    (output, json)
}

fn path(kitchen: &TestKitchen, relative: &str) -> String {
    let project = kitchen.project();
    let path = if relative.is_empty() {
        project
    } else {
        project.join(relative)
    };
    path.to_string_lossy().into_owned()
}

fn names(value: &Value) -> Vec<&str> {
    value
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect()
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[test]
fn walks_up_to_the_kitchen_file() {
    let k = kitchen();
    k.write("mise.toml", "[_.microkitchen]\ncpus = 3\n");
    k.mkdir("src/deep");

    let (output, json) = validate(&k, "src/deep");
    assert!(output.status.success(), "{json:#}");
    assert_eq!(json["kitchen_file"], path(&k, "mise.toml"));
    assert_eq!(json["kitchen_dir"], path(&k, ""));
    assert_eq!(json["section"], "underscore");
    assert_eq!(json["config"]["cpus"], 3);
    assert!(
        json["sandbox_name"]
            .as_str()
            .unwrap()
            .starts_with("mk-project-")
    );
}

#[test]
fn highest_precedence_file_with_a_section_wins() {
    let k = kitchen();
    k.write("mise.toml", "[_.microkitchen]\ncpus = 3\n");
    k.write("mise.local.toml", "[_.microkitchen]\ncpus = 5\n");
    k.write("sub/mise.toml", "[env]\nA = \"1\"\n");

    let (_, json) = validate(&k, "sub");
    assert_eq!(json["kitchen_file"], path(&k, "mise.local.toml"));
    assert_eq!(json["kitchen_dir"], path(&k, ""));
    assert_eq!(json["config"]["cpus"], 5);
    // Lowest precedence first.
    assert_eq!(
        names(&json["loaded_files"]),
        [
            path(&k, "mise.toml"),
            path(&k, "mise.local.toml"),
            path(&k, "sub/mise.toml")
        ]
    );
    assert_eq!(names(&json["env"]), ["A"]);
}

#[test]
fn falls_back_to_the_nearest_mise_toml() {
    let k = kitchen();
    k.write("mise.toml", "[env]\nA = \"1\"\n");
    k.write("sub/.config/mise/config.toml", "[env]\nB = \"2\"\n");

    let (output, json) = validate(&k, "sub");
    assert!(output.status.success(), "{json:#}");
    assert_eq!(
        json["kitchen_file"],
        path(&k, "sub/.config/mise/config.toml")
    );
    assert_eq!(json["kitchen_dir"], path(&k, "sub"));
    assert_eq!(json["section"], Value::Null);
    assert_eq!(json["config"]["cpus"], 2);
    assert_eq!(names(&json["env"]), ["A", "B"]);
}

#[test]
fn no_config_is_an_error() {
    let k = kitchen();
    let output = k.run("", &["validate"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no mise.toml found"));
}

#[test]
fn legacy_section_is_used_with_a_warning() {
    let k = kitchen();
    k.write("mise.toml", "[microkitchen]\ncpus = 4\n");

    let (output, json) = validate(&k, "");
    assert!(output.status.success(), "{json:#}");
    assert_eq!(json["section"], "legacy");
    assert_eq!(json["config"]["cpus"], 4);
    assert_eq!(json["diagnostics"][0]["severity"], "warning");
}

#[test]
fn errors_are_reported_together_with_lines() {
    let k = kitchen();
    k.write(
        "mise.toml",
        "[env]\nPLAIN = \"x\"\n\n[_.microkitchen]\ncpus = 0\n\n[_.microkitchen.secrets.TOKEN]\nallow = [\"github.com\"]\n",
    );

    let (output, json) = validate(&k, "");
    assert!(!output.status.success());
    assert_eq!(json["valid"], false);
    let diagnostics = json["diagnostics"].as_array().unwrap();
    assert_eq!(diagnostics.len(), 2, "{json:#}");
    assert_eq!(diagnostics[0]["line"], 5);
    assert_eq!(diagnostics[1]["line"], 7);
    assert!(
        diagnostics[1]["message"]
            .as_str()
            .unwrap()
            .contains("not declared in [env]")
    );

    let human = k.run("", &["validate"]);
    let stderr = String::from_utf8_lossy(&human.stderr);
    assert!(
        stderr.contains(&format!(
            "error: {}:5:8: _.microkitchen.cpus:",
            path(&k, "mise.toml")
        )),
        "{stderr}"
    );
    assert!(stderr.contains("2 errors"), "{stderr}");
}

#[test]
fn env_and_secrets_resolve_on_the_host() {
    let k = kitchen();
    k.write(".env", "FROM_FILE=file-value\n");
    k.write(
        "mise.toml",
        r#"[env]
PLAIN = "x"
REQ = { required = true }
OPT = { default = "" }
_.file = ".env"

[_.microkitchen.secrets.REQ]
allow = ["github.com"]

[_.microkitchen.secrets.OPT]
allow = ["api.figma.com"]
"#,
    );

    // Required variable missing on the host: mise's message is reported.
    let output = k
        .command("", ["validate", "--json"])
        .env_remove("REQ")
        .env_remove("OPT")
        .output()
        .unwrap();
    let json = parse(&output);
    assert!(!output.status.success());
    assert!(
        json["diagnostics"][0]["message"]
            .as_str()
            .unwrap()
            .contains("'REQ'"),
        "{json:#}"
    );

    // Required set, optional unset: the optional secret is skipped.
    let output = k
        .command("", ["validate", "--json"])
        .env("REQ", "tok")
        .env_remove("OPT")
        .output()
        .unwrap();
    let json = parse(&output);
    assert!(output.status.success(), "{json:#}");
    assert_eq!(names(&json["env"]), ["FROM_FILE", "PLAIN"]);
    assert_eq!(names(&json["secrets"]), ["REQ"]);
    assert_eq!(names(&json["skipped_secrets"]), ["OPT"]);
    assert_eq!(json["diagnostics"][0]["severity"], "notice");
    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("tok"),
        "values must not be printed"
    );

    // Optional set: forwarded as a secret too.
    let output = k
        .command("", ["validate", "--json"])
        .env("REQ", "tok")
        .env("OPT", "fig")
        .output()
        .unwrap();
    let json = parse(&output);
    assert_eq!(names(&json["secrets"]), ["OPT", "REQ"]);
    assert_eq!(names(&json["skipped_secrets"]), Vec::<&str>::new());
}

#[test]
fn net_allow_and_deny_edit_the_kitchen_file() {
    let k = kitchen();
    k.write(
        "mise.toml",
        "# my project\n[_.microkitchen.network]\n# trusted hosts\nallow = [\"a.com\"]\ndeny = [\"b.com\"]\n",
    );
    k.mkdir("src");

    let output = k.run("src", &["net", "allow", "b.com"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        k.read("mise.toml"),
        "# my project\n[_.microkitchen.network]\n# trusted hosts\nallow = [\"a.com\", \"b.com\"]\ndeny = []\n"
    );

    assert!(k.run("", &["net", "deny", "*.evil.com"]).status.success());
    assert!(k.read("mise.toml").contains("deny = [\"*.evil.com\"]"));

    let output = k.run("", &["net", "allow", "bad host"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid rule"));

    let (output, _) = validate(&k, "");
    assert!(output.status.success());
}
