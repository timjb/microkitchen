//! The copy of the kitchen file placed in the guest at `/opt/kitchen/mise.toml`.
//!
//! `[env]` declarations stay, so `mise bootstrap` in the guest sees the same
//! variables (required ones are satisfied by injected env vars and secret
//! placeholders). Host-file directives (`_.file`, `_.source`, `_.path`) are
//! removed so `.env` values and host paths never enter the guest. The
//! `[_.microkitchen]` table configures the sandbox from the host; the guest
//! gets no copy, so it changes only when something mise uses does.
//!
//! The sandbox user is declared here too: `[bootstrap.users.chef]` is added
//! when the kitchen file has none, and chef's ids are filled in when missing,
//! since the sandbox was created to run as them (see [`GuestUser`]).
//!
//! [`GuestUser`]: crate::config::schema::GuestUser

use anyhow::{Context, Result};
use toml_edit::{DocumentMut, Item, Table, TableLike, Value, value};

use crate::config::schema::{CHEF, DEFAULT_CHEF_ID};
use crate::config::staging::{RewriteAction, StagePlan, Step};

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

/// Directive tables inside `[env]`: `_` and the older `mise`.
const DIRECTIVE_TABLES: &[&str] = &["_", "mise"];

/// Directives that read host files or add host paths.
const HOST_DIRECTIVES: &[&str] = &["file", "source", "path"];

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// Render the guest copy of the kitchen file, preserving everything else.
pub fn render_guest_config(text: &str) -> Result<String> {
    render_guest_config_with(text, &StagePlan::default())
}

/// As [`render_guest_config`], with each staged `source` rewritten to the
/// absolute guest path of its copy.
///
/// Rewrites run first: they address keys as written in the kitchen file, before
/// `[_.microkitchen]` is removed.
pub fn render_guest_config_with(text: &str, staging: &StagePlan) -> Result<String> {
    let mut document: DocumentMut = text.parse().context("parsing the kitchen file")?;
    for rewrite in &staging.rewrites {
        apply_rewrite(document.as_item_mut(), &rewrite.path, &rewrite.action);
    }
    if let Some(user) = document.get_mut("_").and_then(Item::as_table_like_mut) {
        user.remove("microkitchen");
        if user.is_empty() {
            document.remove("_");
        }
    }
    match document.get_mut("env") {
        None => {}
        Some(Item::ArrayOfTables(tables)) => {
            for table in tables.iter_mut() {
                strip_host_directives(table);
            }
        }
        Some(item) => {
            if let Some(table) = item.as_table_like_mut() {
                strip_host_directives(table);
            }
        }
    }
    add_chef(&mut document);
    Ok(document.to_string())
}

/// Apply one rewrite, descending `path` from `item`.
///
/// [`RewriteAction::Set`] and [`RewriteAction::Remove`] only touch keys that
/// exist, so a rewrite whose entry the user edited away is a no-op;
/// [`RewriteAction::Ensure`] creates the key and any missing parents.
fn apply_rewrite(item: &mut Item, path: &[Step], action: &RewriteAction) {
    match path {
        [] => {}
        [Step::Key(key)] => {
            let Some(table) = item.as_table_like_mut() else {
                return;
            };
            match action {
                RewriteAction::Set(text) if table.contains_key(key) => {
                    table.insert(key, value(text.as_str()));
                }
                RewriteAction::Ensure(text) => {
                    table.insert(key, value(text.as_str()));
                }
                RewriteAction::Remove => {
                    table.remove(key);
                }
                RewriteAction::Set(_) => {}
            }
        }
        [Step::Key(key), rest @ ..] => {
            let Some(table) = item.as_table_like_mut() else {
                return;
            };
            // `Ensure` creates the parents it descends through; the others
            // leave a config that no longer has the key alone.
            let child = match action {
                RewriteAction::Ensure(_) => {
                    if implicit_table(table, key).is_none() {
                        return;
                    }
                    table.get_mut(key)
                }
                _ => table.get_mut(key),
            };
            if let Some(child) = child {
                apply_rewrite(child, rest, action);
            }
        }
        [Step::Index(index), rest @ ..] => {
            let Some(array) = item.as_array_mut() else {
                return;
            };
            if let Some(element) = array.get_mut(*index) {
                apply_rewrite_value(element, rest, action);
            }
        }
    }
}

/// [`apply_rewrite`] inside an array element, which is a [`Value`], not an
/// [`Item`]: `variants = [{ source = "…" }, …]`.
fn apply_rewrite_value(value_: &mut Value, path: &[Step], action: &RewriteAction) {
    let Some(table) = value_.as_inline_table_mut() else {
        return;
    };
    match path {
        [Step::Key(key)] => match action {
            RewriteAction::Set(text) if table.contains_key(key) => {
                table.insert(key, text.as_str().into());
            }
            RewriteAction::Ensure(text) => {
                table.insert(key, text.as_str().into());
            }
            RewriteAction::Remove => {
                table.remove(key);
            }
            RewriteAction::Set(_) => {}
        },
        [Step::Key(key), rest @ ..] => {
            if let Some(child) = table.get_mut(key) {
                apply_rewrite_value(child, rest, action);
            }
        }
        _ => {}
    }
}

/// Declare chef: passwordless sudo (through the `sudo` group, see
/// `sandbox::bootstrap`) and Docker unless the kitchen file says otherwise.
fn add_chef(document: &mut DocumentMut) {
    let Some(bootstrap) = implicit_table(document.as_table_mut(), "bootstrap") else {
        return;
    };
    let Some(users) = implicit_table(bootstrap, "users") else {
        return;
    };
    let group = match users.get_mut(CHEF).and_then(Item::as_table_like_mut) {
        Some(chef) => {
            if !chef.contains_key("uid") {
                chef.insert("uid", value(i64::from(DEFAULT_CHEF_ID)));
            }
            if !chef.contains_key("group") {
                chef.insert("group", value(CHEF));
            }
            chef.get("group")
                .and_then(Item::as_str)
                .unwrap_or(CHEF)
                .to_owned()
        }
        None => {
            let mut chef = Table::new();
            chef.insert("uid", value(i64::from(DEFAULT_CHEF_ID)));
            chef.insert("group", value(CHEF));
            chef.insert(
                "groups",
                value(toml_edit::Array::from_iter(["sudo", "docker"])),
            );
            chef.insert("shell", value("/bin/bash"));
            chef.insert("comment", value("sandbox user"));
            users.insert(CHEF, Item::Table(chef));
            CHEF.to_owned()
        }
    };
    if group != CHEF {
        return;
    }
    let Some(groups) = implicit_table(bootstrap, "groups") else {
        return;
    };
    match groups.get_mut(CHEF).and_then(Item::as_table_like_mut) {
        Some(chef) => {
            if !chef.contains_key("gid") {
                chef.insert("gid", value(i64::from(DEFAULT_CHEF_ID)));
            }
        }
        None => {
            let mut chef = Table::new();
            chef.insert("gid", value(i64::from(DEFAULT_CHEF_ID)));
            groups.insert(CHEF, Item::Table(chef));
        }
    }
}

/// `parent.key` as a table, created without a header of its own if missing;
/// `None` if it is something else (mise reports that).
fn implicit_table<'a>(parent: &'a mut dyn TableLike, key: &str) -> Option<&'a mut dyn TableLike> {
    parent
        .entry(key)
        .or_insert_with(|| {
            let mut table = Table::new();
            table.set_implicit(true);
            Item::Table(table)
        })
        .as_table_like_mut()
}

fn strip_host_directives(env: &mut dyn TableLike) {
    for key in DIRECTIVE_TABLES {
        let Some(directives) = env.get_mut(key).and_then(Item::as_table_like_mut) else {
            continue;
        };
        for directive in HOST_DIRECTIVES {
            directives.remove(directive);
        }
        if directives.is_empty() {
            env.remove(key);
        }
    }
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// What a kitchen file without `[bootstrap.users.chef]` gains.
    const DEFAULT_CHEF: &str = "\n[bootstrap.users.chef]\nuid = 1001\ngroup = \"chef\"\n\
        groups = [\"sudo\", \"docker\"]\nshell = \"/bin/bash\"\ncomment = \"sandbox user\"\n\
        \n[bootstrap.groups.chef]\ngid = 1001\n";

    #[test]
    fn strips_host_directives_only() {
        let text = r#"# kitchen
[env]
_.file = ".env"
_.path = ["./bin"]
_.source = "./setup.sh"
_.python.venv = ".venv"
A = "1"
REQ = { required = true }

[tools]
node = "22"

[_.microkitchen]
cpus = 2
"#;
        let out = render_guest_config(text).unwrap();
        for gone in [".env\"", "./bin", "setup.sh"] {
            assert!(!out.contains(gone), "{gone} survived:\n{out}");
        }
        for kept in [
            "# kitchen",
            "_.python.venv = \".venv\"",
            "A = \"1\"",
            "REQ = { required = true }",
            "node = \"22\"",
        ] {
            assert!(out.contains(kept), "{kept} missing:\n{out}");
        }
        assert!(!out.contains("microkitchen"), "{out}");
    }

    #[test]
    fn keeps_other_user_tables() {
        let text = "[tools]\nnode = \"22\"\n\n[_.other]\na = 1\n\n[_.microkitchen]\ncpus = 2\n\n[_.microkitchen.network]\nallow = [\"a.com\"]\n";
        let out = render_guest_config(text).unwrap();
        assert!(out.contains("[_.other]") && out.contains("a = 1"), "{out}");
        assert!(
            !out.contains("microkitchen") && !out.contains("a.com"),
            "{out}"
        );

        let out =
            render_guest_config("[tools]\nnode = \"22\"\n\n[_.microkitchen]\ncpus = 2\n").unwrap();
        assert_eq!(out, format!("[tools]\nnode = \"22\"\n{DEFAULT_CHEF}"));
    }

    #[test]
    fn removes_emptied_directive_tables() {
        let out = render_guest_config("[env]\n_.file = \".env\"\nmise.file = \".e\"\nA = \"1\"\n")
            .unwrap();
        assert_eq!(out, format!("[env]\nA = \"1\"\n{DEFAULT_CHEF}"));
    }

    #[test]
    fn handles_env_arrays() {
        let out = render_guest_config("[[env]]\n_.file = \".env\"\n[[env]]\nA = \"1\"\n").unwrap();
        assert!(!out.contains(".env"), "{out}");
        assert!(out.contains("A = \"1\""));
    }

    /// `bootstrap.<section>.<name>.<key>` of rendered output.
    fn account(out: &str, section: &str, name: &str, key: &str) -> Option<String> {
        let document: DocumentMut = out.parse().unwrap();
        let value = document["bootstrap"][section][name].get(key)?;
        Some(value.to_string().trim().to_owned())
    }

    #[test]
    fn adds_chef_next_to_other_accounts() {
        let text = "[bootstrap.users.alice]\ngroup = \"staff\"\n\n[bootstrap.groups.staff]\n";
        let out = render_guest_config(text).unwrap();
        assert!(
            out.starts_with("[bootstrap.users.alice]\ngroup = \"staff\"\n"),
            "{out}"
        );
        assert!(out.contains("[bootstrap.groups.staff]\n"), "{out}");
        assert_eq!(
            account(&out, "users", "chef", "uid").as_deref(),
            Some("1001")
        );
        assert_eq!(
            account(&out, "users", "chef", "groups").as_deref(),
            Some("[\"sudo\", \"docker\"]")
        );
        assert_eq!(
            account(&out, "groups", "chef", "gid").as_deref(),
            Some("1001")
        );
    }

    #[test]
    fn keeps_a_declared_chef_and_fills_in_its_ids() {
        let text = "[bootstrap.users.chef]\nshell = \"/bin/zsh\"\n\n[bootstrap.groups.chef]\nsystem = false\n";
        let out = render_guest_config(text).unwrap();
        assert_eq!(
            out,
            "[bootstrap.users.chef]\nshell = \"/bin/zsh\"\nuid = 1001\ngroup = \"chef\"\n\n\
             [bootstrap.groups.chef]\nsystem = false\ngid = 1001\n"
        );

        let text = "[bootstrap.users.chef]\nuid = 2000\ngroup = \"chef\"\n\n[bootstrap.groups.chef]\ngid = 2000\n";
        assert_eq!(render_guest_config(text).unwrap(), text);
    }

    #[test]
    fn leaves_another_primary_group_alone() {
        let text = "[bootstrap.users.chef]\nuid = 1001\ngroup = \"staff\"\n\n[bootstrap.groups.staff]\ngid = 50\n";
        assert_eq!(render_guest_config(text).unwrap(), text);
    }

    /// Render with the staging plan the kitchen file itself produces.
    fn staged(text: &str, dotfiles: Option<&str>) -> String {
        use crate::config::Diagnostics;
        use crate::config::staging;
        use std::path::Path;

        let source = crate::config::Source::new("/p/app/mise.toml", text);
        let mut diagnostics = Diagnostics::default();
        let plan = staging::plan(
            &source,
            Path::new("/p/app"),
            Path::new("/p/app"),
            dotfiles.map(Path::new),
            &mut diagnostics,
        );
        assert!(!diagnostics.has_errors(), "{diagnostics:?}");
        render_guest_config_with(text, &plan).unwrap()
    }

    #[test]
    fn rewrites_sources_to_absolute_guest_paths() {
        let text = r#"# kitchen
[dotfiles]
"~/.gitconfig" = "dotfiles/gitconfig"
"~/.vimrc" = { source = "dotfiles/vimrc", mode = "copy" }

[dotfiles."~/.config/nvim"]
source = "dotfiles/nvim"
mode = "symlink"

[bootstrap.files."/etc/a.conf"]
source = "etc/a.conf"
mode = "0644"
"#;
        let out = staged(text, None);
        for expected in [
            "\"~/.gitconfig\" = \"/opt/kitchen/files/project/dotfiles/gitconfig\"",
            "source = \"/opt/kitchen/files/project/dotfiles/vimrc\"",
            "source = \"/opt/kitchen/files/project/dotfiles/nvim\"",
            "source = \"/opt/kitchen/files/project/etc/a.conf\"",
        ] {
            assert!(out.contains(expected), "{expected} missing:\n{out}");
        }
        // Comments, unrelated keys and table headers survive.
        for kept in [
            "# kitchen",
            "mode = \"copy\"",
            "mode = \"symlink\"",
            "mode = \"0644\"",
            "[dotfiles.\"~/.config/nvim\"]",
        ] {
            assert!(out.contains(kept), "{kept} missing:\n{out}");
        }
        assert!(!out.contains("\"dotfiles/gitconfig\""), "{out}");
    }

    #[test]
    fn rewrites_every_variant_and_keeps_the_pattern_of_a_glob() {
        let text = r#"
[dotfiles."~/.config/*.toml"]
source = "conf/*.toml"

[dotfiles."vscode/settings.json"]
variants = [
  { os = "macos", source = "dot/macos.json" },
  { os = "linux", source = "dot/linux.json" },
]
"#;
        let out = staged(text, None);
        assert!(
            out.contains("source = \"/opt/kitchen/files/project/conf/*.toml\""),
            "{out}"
        );
        assert!(
            out.contains("source = \"/opt/kitchen/files/project/dot/macos.json\""),
            "{out}"
        );
        assert!(
            out.contains("source = \"/opt/kitchen/files/project/dot/linux.json\""),
            "{out}"
        );
        assert!(out.contains("os = \"macos\""), "{out}");
    }

    #[test]
    fn git_manifest_is_dropped_since_the_copy_has_no_git() {
        let text =
            "[dotfiles.\"~\"]\nsource = \"dots\"\nmode = \"symlink-each\"\nmanifest = \"git\"\n";
        let out = staged(text, None);
        assert!(!out.contains("manifest"), "{out}");
        assert!(out.contains("mode = \"symlink-each\""), "{out}");
    }

    #[test]
    fn the_dotfiles_root_is_created_or_replaced() {
        // Created when the kitchen file has no [settings] at all.
        let out = staged("[tools]\nnode = \"22\"\n", Some("/home/t/.dots"));
        let document: DocumentMut = out.parse().unwrap();
        let root = document["settings"]["dotfiles"]["root"].as_str().unwrap();
        assert!(root.starts_with("/opt/kitchen/files/"), "{out}");
        assert!(root.ends_with("/.dots"), "{out}");

        // Replaced in place when it is already written.
        let out = staged("[settings.dotfiles]\nroot = \"~/.dotfiles\"\n", None);
        assert!(!out.contains("~/.dotfiles"), "{out}");
        assert!(out.contains("[settings.dotfiles]"), "{out}");
    }

    #[test]
    fn rewrites_run_before_the_microkitchen_table_is_stripped() {
        let text = "[dotfiles]\n\"~/.x\" = \"d/x\"\n\n[_.microkitchen]\ncpus = 1\n";
        let out = staged(text, None);
        assert!(
            out.contains("\"~/.x\" = \"/opt/kitchen/files/project/d/x\""),
            "{out}"
        );
        assert!(!out.contains("microkitchen"), "{out}");
    }

    #[test]
    fn fills_in_inline_and_dotted_declarations() {
        let out =
            render_guest_config("[bootstrap]\nusers.chef = { shell = \"/bin/zsh\" }\n").unwrap();
        assert_eq!(
            account(&out, "users", "chef", "shell").as_deref(),
            Some("\"/bin/zsh\"")
        );
        assert_eq!(
            account(&out, "users", "chef", "uid").as_deref(),
            Some("1001")
        );
        assert_eq!(account(&out, "users", "chef", "groups"), None);
        assert_eq!(
            account(&out, "groups", "chef", "gid").as_deref(),
            Some("1001")
        );
    }
}
