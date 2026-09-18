//! The copy of the kitchen file placed in the guest at `/root/kitchen/mise.toml`.
//!
//! `[env]` declarations stay, so `mise bootstrap` in the guest sees the same
//! variables (required ones are satisfied by injected env vars and secret
//! placeholders). Host-file directives (`_.file`, `_.source`, `_.path`) are
//! removed so `.env` values and host paths never enter the guest. The
//! `[_.microkitchen]` table configures the sandbox from the host; the guest
//! gets no copy, so it changes only when something mise uses does.

use anyhow::{Context, Result};
use toml_edit::{DocumentMut, Item, TableLike};

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
    let mut document: DocumentMut = text.parse().context("parsing the kitchen file")?;
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
    Ok(document.to_string())
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
        assert_eq!(out, "[tools]\nnode = \"22\"\n");
    }

    #[test]
    fn removes_emptied_directive_tables() {
        let out = render_guest_config("[env]\n_.file = \".env\"\nmise.file = \".e\"\nA = \"1\"\n")
            .unwrap();
        assert_eq!(out, "[env]\nA = \"1\"\n");
    }

    #[test]
    fn handles_env_arrays() {
        let out = render_guest_config("[[env]]\n_.file = \".env\"\n[[env]]\nA = \"1\"\n").unwrap();
        assert!(!out.contains(".env"), "{out}");
        assert!(out.contains("A = \"1\""));
    }
}
