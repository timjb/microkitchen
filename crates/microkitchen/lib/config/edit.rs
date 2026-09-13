//! Comment-preserving edits to rule lists: the kitchen file's
//! `[_.microkitchen.network]` and the shared `~/.microkitchen/rules.toml`.
//!
//! Used by approval decisions and `microkitchen net allow|deny|revoke`.
//! Writers are serialized through a lock in `~/.microkitchen` and replace the
//! file atomically.

use std::path::Path;

use anyhow::{Context, Result, bail};
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, TableLike, Value};

use super::hostpat::HostPattern;
use crate::state::{Home, write_atomic};

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// Which rule list to edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuleList {
    Allow,
    Deny,
}

/// What an edit changed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RuleChange {
    /// The rule was appended to the target list (it was not already there).
    pub added: bool,
    /// The rule was removed from the opposite list.
    pub removed_from_other: bool,
}

/// Where the rule lists live in a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Scope {
    /// `[_.microkitchen.network]` (or the legacy `[microkitchen.network]`).
    Kitchen,
    /// The top level of `rules.toml`.
    Global,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl RuleList {
    pub fn key(self) -> &'static str {
        match self {
            Self::Allow => "allow",
            Self::Deny => "deny",
        }
    }

    pub fn other(self) -> Self {
        match self {
            Self::Allow => Self::Deny,
            Self::Deny => Self::Allow,
        }
    }
}

impl RuleChange {
    pub fn is_noop(self) -> bool {
        self == Self::default()
    }
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// Put `rule` into `list` of the kitchen file (and take it out of the other list).
pub fn set_network_rule(
    home: &Home,
    file: &Path,
    list: RuleList,
    rule: &HostPattern,
) -> Result<RuleChange> {
    edit_file(
        home,
        file,
        false,
        |c: &RuleChange| !c.is_noop(),
        |text| set_rule_in(text, Scope::Kitchen, list, rule),
    )
}

/// Put `rule` into `list` of `~/.microkitchen/rules.toml`, creating it.
pub fn set_global_rule(home: &Home, list: RuleList, rule: &HostPattern) -> Result<RuleChange> {
    edit_file(
        home,
        &home.rules_file(),
        true,
        |c: &RuleChange| !c.is_noop(),
        |text| set_rule_in(text, Scope::Global, list, rule),
    )
}

/// Take `rule` out of the kitchen file's `allow` and `deny` lists. Returns
/// the lists it was removed from (empty when it was in neither).
pub fn remove_network_rule(home: &Home, file: &Path, rule: &HostPattern) -> Result<Vec<RuleList>> {
    edit_file(
        home,
        file,
        false,
        |r: &Vec<RuleList>| !r.is_empty(),
        |text| remove_rule_in(text, Scope::Kitchen, rule),
    )
}

/// Take `rule` out of `~/.microkitchen/rules.toml`.
pub fn remove_global_rule(home: &Home, rule: &HostPattern) -> Result<Vec<RuleList>> {
    edit_file(
        home,
        &home.rules_file(),
        true,
        |r: &Vec<RuleList>| !r.is_empty(),
        |text| remove_rule_in(text, Scope::Global, rule),
    )
}

/// [`set_network_rule`] on a string. Entries are compared as parsed patterns,
/// so `Example.com` and `example.com` count as the same rule.
pub fn set_network_rule_in(
    text: &str,
    list: RuleList,
    rule: &HostPattern,
) -> Result<(String, RuleChange)> {
    set_rule_in(text, Scope::Kitchen, list, rule)
}

/// [`remove_network_rule`] on a string; never creates a section.
pub fn remove_network_rule_in(text: &str, rule: &HostPattern) -> Result<(String, Vec<RuleList>)> {
    remove_rule_in(text, Scope::Kitchen, rule)
}

/// Read `file` (empty if `missing_ok` and absent), edit it, and write it back
/// under the writers' lock if `changed` says so.
fn edit_file<T>(
    home: &Home,
    file: &Path,
    missing_ok: bool,
    changed: impl FnOnce(&T) -> bool,
    edit: impl FnOnce(&str) -> Result<(String, T)>,
) -> Result<T> {
    let _lock = home.lock_writers()?;
    let text = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(error) if missing_ok && error.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(error).with_context(|| format!("reading {}", file.display())),
    };
    let (updated, result) = edit(&text).with_context(|| format!("editing {}", file.display()))?;
    if changed(&result) {
        write_atomic(file, updated.as_bytes())?;
    }
    Ok(result)
}

fn set_rule_in(
    text: &str,
    scope: Scope,
    list: RuleList,
    rule: &HostPattern,
) -> Result<(String, RuleChange)> {
    let mut document: DocumentMut = text.parse().context("the file is not valid TOML")?;
    let table: &mut dyn TableLike = match scope {
        Scope::Kitchen => network_table(&mut document)?
            .as_table_like_mut()
            .expect("network_table returns a table"),
        Scope::Global => document.as_table_mut(),
    };

    let mut change = RuleChange::default();
    if let Some(other) = table.get_mut(list.other().key()) {
        let Some(other) = other.as_array_mut() else {
            bail!("`{}` is not an array", list.other().key());
        };
        change.removed_from_other = remove_from(other, rule);
    }

    let target = table
        .entry(list.key())
        .or_insert(Item::Value(Value::Array(Array::new())));
    let Some(target) = target.as_array_mut() else {
        bail!("`{}` is not an array", list.key());
    };
    if !target.iter().any(|value| is_rule(value, rule)) {
        push_like_siblings(target, rule.to_string());
        change.added = true;
    }

    Ok((document.to_string(), change))
}

fn remove_rule_in(text: &str, scope: Scope, rule: &HostPattern) -> Result<(String, Vec<RuleList>)> {
    let mut document: DocumentMut = text.parse().context("the file is not valid TOML")?;
    let table: Option<&mut dyn TableLike> = match scope {
        Scope::Kitchen => {
            let underscore = document
                .get("_")
                .and_then(|u| u.get("microkitchen"))
                .is_some();
            let section = if underscore {
                document
                    .get_mut("_")
                    .and_then(|u| u.get_mut("microkitchen"))
            } else {
                document.get_mut("microkitchen")
            };
            section
                .and_then(|s| s.get_mut("network"))
                .and_then(Item::as_table_like_mut)
        }
        Scope::Global => Some(document.as_table_mut()),
    };
    let mut removed = Vec::new();
    let Some(table) = table else {
        return Ok((text.to_owned(), removed));
    };
    for list in [RuleList::Allow, RuleList::Deny] {
        if let Some(array) = table.get_mut(list.key()).and_then(Item::as_array_mut)
            && remove_from(array, rule)
        {
            removed.push(list);
        }
    }
    Ok((document.to_string(), removed))
}

/// Remove `rule` from `array`; true if it was there.
///
/// A comment ending the line before an element is stored in that element's
/// prefix, so it moves to the next element (or the array's closing space)
/// instead of disappearing. A new first element takes over the old first
/// element's spacing.
fn remove_from(array: &mut Array, rule: &HostPattern) -> bool {
    let mut removed = false;
    for index in (0..array.len()).rev() {
        if !array.get(index).is_some_and(|value| is_rule(value, rule)) {
            continue;
        }
        let prefix = array.get(index).and_then(prefix_of).unwrap_or_default();
        array.remove(index);
        removed = true;
        if index == 0 {
            if let Some(first) = array.get_mut(0) {
                first.decor_mut().set_prefix(prefix);
            }
            continue;
        }
        let Some(newline) = prefix.rfind('\n') else {
            continue;
        };
        let comment = &prefix[..newline];
        if comment.trim().is_empty() {
            continue;
        }
        match array.get_mut(index) {
            Some(next) => {
                let rest = prefix_of(next).unwrap_or_default();
                next.decor_mut().set_prefix(format!("{comment}{rest}"));
            }
            None => {
                let trailing = array.trailing().as_str().unwrap_or("").to_owned();
                array.set_trailing(format!("{comment}{trailing}"));
            }
        }
    }
    removed
}

/// The network table of the existing microkitchen section, created as
/// `[_.microkitchen.network]` if needed.
fn network_table(document: &mut DocumentMut) -> Result<&mut Item> {
    let root = document.as_item_mut();
    let has_underscore = root.get("_").and_then(|u| u.get("microkitchen")).is_some();
    let section = if !has_underscore && root.get("microkitchen").is_some() {
        child_table(root, "microkitchen", false)?
    } else {
        let underscore = child_table(root, "_", true)?;
        child_table(underscore, "microkitchen", true)?
    };
    child_table(section, "network", false)
}

/// Get or create table `key` in `parent`. New tables are inline inside inline
/// parents; `implicit` tables print no header of their own.
fn child_table<'a>(parent: &'a mut Item, key: &str, implicit: bool) -> Result<&'a mut Item> {
    let inline = parent.is_inline_table();
    let table = parent.as_table_like_mut().context("expected a table")?;
    if table.get(key).is_none() {
        let item = if inline {
            Item::Value(Value::InlineTable(InlineTable::new()))
        } else {
            let mut new = Table::new();
            new.set_implicit(implicit);
            Item::Table(new)
        };
        table.insert(key, item);
    }
    let child = table.get_mut(key).expect("just inserted");
    if !child.is_table_like() {
        bail!("`{key}` is not a table");
    }
    Ok(child)
}

fn is_rule(value: &Value, rule: &HostPattern) -> bool {
    value
        .as_str()
        .and_then(|s| s.parse::<HostPattern>().ok())
        .is_some_and(|pattern| pattern == *rule)
}

/// Append keeping the layout of multi-line arrays: the new element gets the
/// indentation of the last element (but none of its comments).
fn push_like_siblings(array: &mut Array, value: String) {
    let indent = array.iter().last().map(|last| {
        let prefix = last.decor().prefix().and_then(|p| p.as_str()).unwrap_or("");
        match prefix.rfind('\n') {
            Some(newline) => prefix[newline..].to_owned(),
            None => " ".to_owned(),
        }
    });
    array.push(value);
    if let Some(indent) = indent
        && let Some(last) = array.iter_mut().last()
    {
        last.decor_mut().set_prefix(indent);
        last.decor_mut().set_suffix("");
    }
}

fn prefix_of(value: &Value) -> Option<String> {
    value
        .decor()
        .prefix()
        .and_then(|p| p.as_str())
        .map(str::to_owned)
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(s: &str) -> HostPattern {
        s.parse().unwrap()
    }

    fn apply(text: &str, list: RuleList, r: &str) -> (String, RuleChange) {
        set_network_rule_in(text, list, &rule(r)).unwrap()
    }

    #[test]
    fn appends_preserving_comments_and_layout() {
        let text = "\
# project settings
[_.microkitchen]
cpus = 2 # enough

[_.microkitchen.network]
allow = [
  \"a.com\", # first
  \"b.com\",
]
";
        let (out, change) = apply(text, RuleList::Allow, "c.com");
        assert!(change.added && !change.removed_from_other);
        assert_eq!(
            out,
            text.replace("  \"b.com\",\n]", "  \"b.com\",\n  \"c.com\",\n]")
        );
    }

    #[test]
    fn appends_to_single_line_arrays() {
        let (out, _) = apply(
            "[_.microkitchen.network]\ndeny = [\"a.com\"]\n",
            RuleList::Deny,
            "203.0.113.7",
        );
        assert_eq!(
            out,
            "[_.microkitchen.network]\ndeny = [\"a.com\", \"203.0.113.7\"]\n"
        );
    }

    #[test]
    fn creates_the_section_when_missing() {
        let text = "[env]\nA = \"1\"\n";
        let (out, change) = apply(text, RuleList::Allow, "x.com");
        assert!(change.added);
        assert_eq!(
            out,
            "[env]\nA = \"1\"\n\n[_.microkitchen.network]\nallow = [\"x.com\"]\n"
        );
    }

    #[test]
    fn adds_network_to_an_existing_section() {
        let (out, _) = apply("[_.microkitchen]\ncpus = 2\n", RuleList::Deny, "*.evil.com");
        assert_eq!(
            out,
            "[_.microkitchen]\ncpus = 2\n\n[_.microkitchen.network]\ndeny = [\"*.evil.com\"]\n"
        );
    }

    #[test]
    fn writes_into_a_legacy_section() {
        let (out, _) = apply("[microkitchen]\ncpus = 2\n", RuleList::Allow, "x.com");
        assert_eq!(
            out,
            "[microkitchen]\ncpus = 2\n\n[microkitchen.network]\nallow = [\"x.com\"]\n"
        );
    }

    #[test]
    fn deduplicates_by_pattern() {
        let text = "[_.microkitchen.network]\nallow = [\"Example.com\"]\n";
        let (out, change) = apply(text, RuleList::Allow, "example.com");
        assert!(change.is_noop());
        assert_eq!(out, text);
    }

    #[test]
    fn moves_between_lists() {
        let text = "[_.microkitchen.network]\nallow = [\"a.com\"]\ndeny = [\"b.com\", \"c.com\"]\n";
        let (out, change) = apply(text, RuleList::Allow, "b.com");
        assert!(change.added && change.removed_from_other);
        assert_eq!(
            out,
            "[_.microkitchen.network]\nallow = [\"a.com\", \"b.com\"]\ndeny = [\"c.com\"]\n"
        );
    }

    #[test]
    fn refuses_non_array_lists() {
        let text = "[_.microkitchen.network]\nallow = \"a.com\"\n";
        assert!(set_network_rule_in(text, RuleList::Allow, &rule("b.com")).is_err());
    }

    #[test]
    fn removes_rules_keeping_the_rest() {
        let text = "\
[_.microkitchen.network]
allow = [
  \"a.com\", # first
  \"b.com\",
]
deny = [\"c.com\"]
";
        let (out, removed) = remove_network_rule_in(text, &rule("b.com")).unwrap();
        assert_eq!(removed, vec![RuleList::Allow]);
        assert_eq!(out, text.replace("  \"b.com\",\n", ""));

        let (out, removed) = remove_network_rule_in(text, &rule("C.com")).unwrap();
        assert_eq!(removed, vec![RuleList::Deny]);
        assert!(out.contains("deny = []"), "{out}");
    }

    #[test]
    fn removing_an_absent_rule_changes_nothing() {
        for text in [
            "[_.microkitchen.network]\nallow = [\"a.com\"]\n",
            "[env]\nA = \"1\"\n",
            "[microkitchen.network]\ndeny = [\"a.com\"]\n",
        ] {
            let (out, removed) = remove_network_rule_in(text, &rule("x.com")).unwrap();
            assert!(removed.is_empty());
            assert_eq!(out, text);
        }
        let (_, removed) = remove_network_rule_in(
            "[microkitchen.network]\ndeny = [\"a.com\"]\n",
            &rule("a.com"),
        )
        .unwrap();
        assert_eq!(removed, vec![RuleList::Deny], "legacy sections too");
    }

    #[test]
    fn global_rules_live_at_the_top_level() {
        let (out, change) =
            set_rule_in("", Scope::Global, RuleList::Allow, &rule("x.com")).unwrap();
        assert!(change.added);
        assert_eq!(out, "allow = [\"x.com\"]\n");
        let (out, change) =
            set_rule_in(&out, Scope::Global, RuleList::Deny, &rule("x.com")).unwrap();
        assert!(change.added && change.removed_from_other);
        assert_eq!(out, "allow = []\ndeny = [\"x.com\"]\n");
        let (out, removed) = remove_rule_in(&out, Scope::Global, &rule("x.com")).unwrap();
        assert_eq!(removed, vec![RuleList::Deny]);
        assert_eq!(out, "allow = []\ndeny = []\n");
    }

    #[test]
    fn writes_atomically_to_disk() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home"));
        let file = dir.path().join("mise.toml");
        std::fs::write(&file, "[env]\n").unwrap();
        let change = set_network_rule(&home, &file, RuleList::Deny, &rule("x.com")).unwrap();
        assert!(change.added);
        assert!(
            std::fs::read_to_string(&file)
                .unwrap()
                .contains("deny = [\"x.com\"]")
        );
        assert!(
            set_network_rule(&home, &file, RuleList::Deny, &rule("x.com"))
                .unwrap()
                .is_noop()
        );
        assert_eq!(
            remove_network_rule(&home, &file, &rule("x.com")).unwrap(),
            vec![RuleList::Deny]
        );
    }

    #[test]
    fn the_global_file_is_created_on_first_use() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path().join("home"));
        assert!(
            remove_global_rule(&home, &rule("x.com"))
                .unwrap()
                .is_empty()
        );
        assert!(!home.rules_file().exists(), "removing creates nothing");
        assert!(
            set_global_rule(&home, RuleList::Allow, &rule("x.com"))
                .unwrap()
                .added
        );
        assert_eq!(
            std::fs::read_to_string(home.rules_file()).unwrap(),
            "allow = [\"x.com\"]\n"
        );
    }
}
