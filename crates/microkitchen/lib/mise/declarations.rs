//! Variables declared in `[env]` across the loaded mise config files.
//!
//! Needed because mise omits pass-through variables from `mise env` output
//! and because every microkitchen secret must be declared in `[env]`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use serde::Serialize;
use toml_edit::{Document, Item, TableLike};

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

/// Keys of `[env]` that hold directives (`_.file`, `_.source`, …), not variables.
const DIRECTIVE_KEYS: &[&str] = &["_", "mise"];

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// How a variable is declared.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DeclarationKind {
    /// `A = "literal"` or `{ value = … }`.
    Value,
    /// `A = "{{ … }}"`.
    Template,
    /// `A = { required = true }`.
    Required,
    /// `A = { default = "" }`.
    Default,
    /// Any other table form mise understands.
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Declaration {
    /// The highest-precedence file declaring the variable.
    pub file: PathBuf,
    pub kind: DeclarationKind,
}

/// Union of `[env]` declarations of the loaded files.
#[derive(Debug, Clone, Default, Serialize)]
#[serde(transparent)]
pub struct EnvDeclarations {
    pub vars: BTreeMap<String, Declaration>,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl EnvDeclarations {
    /// Read `files`, lowest precedence first, so later files override earlier ones.
    pub fn collect(files: &[PathBuf]) -> Result<Self> {
        let mut declarations = Self::default();
        for file in files {
            let text = std::fs::read_to_string(file)
                .with_context(|| format!("reading {}", file.display()))?;
            declarations.add_document(file, &text)?;
        }
        Ok(declarations)
    }

    /// Merge the `[env]` of one file on top of what is already collected.
    pub fn add_document(&mut self, file: &Path, text: &str) -> Result<()> {
        let document = Document::parse(text).map_err(|e| anyhow!("{}: {e}", file.display()))?;
        match document.get("env") {
            None => {}
            Some(Item::ArrayOfTables(tables)) => {
                for table in tables.iter() {
                    self.add_table(file, table);
                }
            }
            Some(item) => {
                if let Some(table) = item.as_table_like() {
                    self.add_table(file, table);
                }
            }
        }
        Ok(())
    }

    pub fn contains(&self, name: &str) -> bool {
        self.vars.contains_key(name)
    }

    fn add_table(&mut self, file: &Path, table: &dyn TableLike) {
        for (name, item) in table.iter() {
            if DIRECTIVE_KEYS.contains(&name) {
                continue;
            }
            // `A = false` unsets a variable declared by a lower-precedence file.
            if item.as_bool() == Some(false) {
                self.vars.remove(name);
                continue;
            }
            let declaration = Declaration {
                file: file.to_owned(),
                kind: kind_of(item),
            };
            self.vars.insert(name.to_owned(), declaration);
        }
    }
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

fn kind_of(item: &Item) -> DeclarationKind {
    if let Some(text) = item.as_str() {
        return if text.contains("{{") {
            DeclarationKind::Template
        } else {
            DeclarationKind::Value
        };
    }
    let Some(table) = item.as_table_like() else {
        return DeclarationKind::Value;
    };
    let required = table
        .get("required")
        .is_some_and(|r| r.as_bool() != Some(false));
    if required {
        DeclarationKind::Required
    } else if table.contains_key("default") {
        DeclarationKind::Default
    } else if table.contains_key("value") {
        DeclarationKind::Value
    } else {
        DeclarationKind::Other
    }
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_declarations_and_skips_directives() {
        let mut d = EnvDeclarations::default();
        d.add_document(
            Path::new("/p/mise.toml"),
            r#"
[env]
A = "1"
T = "{{ env.HOME }}/x"
R = { required = true }
D = { default = "" }
V = { value = "v", redact = true }
_.file = ".env"
mise.file = ".env2"
"#,
        )
        .unwrap();
        let kinds: Vec<_> = d.vars.iter().map(|(k, v)| (k.as_str(), v.kind)).collect();
        use DeclarationKind::*;
        assert_eq!(
            kinds,
            [
                ("A", Value),
                ("D", Default),
                ("R", Required),
                ("T", Template),
                ("V", Value)
            ]
        );
    }

    #[test]
    fn later_files_override_and_unset() {
        let mut d = EnvDeclarations::default();
        d.add_document(Path::new("/p/mise.toml"), "[env]\nA = \"1\"\nB = \"2\"\n")
            .unwrap();
        d.add_document(
            Path::new("/p/mise.local.toml"),
            "[env]\nA = false\nB = { required = true }\n",
        )
        .unwrap();
        assert!(!d.contains("A"));
        assert_eq!(d.vars["B"].file, Path::new("/p/mise.local.toml"));
        assert_eq!(d.vars["B"].kind, DeclarationKind::Required);
    }

    #[test]
    fn reads_env_arrays() {
        let mut d = EnvDeclarations::default();
        d.add_document(
            Path::new("/p/mise.toml"),
            "[[env]]\nA = \"1\"\n[[env]]\nB = \"{{ env.A }}\"\n",
        )
        .unwrap();
        assert!(d.contains("A") && d.contains("B"));
    }
}
