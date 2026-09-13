//! Persistent rules, reloaded when their file changes: each kitchen file's
//! `[_.microkitchen.network]`, and `~/.microkitchen/rules.toml` for every
//! sandbox (consulted after the kitchen's own rules).
//!
//! Hand edits, `remodel` and approval answers therefore apply to the next
//! flow without restarting anything.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use serde::Deserialize;

use super::decision::Rules;
use crate::config::hostpat::HostPattern;
use crate::config::schema;
use crate::config::{Diagnostics, Source};

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

/// Allowed in every kitchen: the image's own time sync would otherwise prompt
/// every few seconds. A kitchen `deny` still wins (deny rules come first).
pub const BUILTIN_ALLOW: &[&str] = &["ntp.ubuntu.com"];

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

pub struct RuleSource {
    file: PathBuf,
    kind: Kind,
    cache: Mutex<Cached>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Kitchen,
    Global,
}

struct Cached {
    stamp: Option<(SystemTime, u64)>,
    rules: Arc<Rules>,
}

/// `rules.toml`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GlobalFile {
    #[serde(default)]
    allow: Vec<String>,
    #[serde(default)]
    deny: Vec<String>,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl RuleSource {
    /// A kitchen file's rules plus [`BUILTIN_ALLOW`].
    pub fn new(file: impl Into<PathBuf>) -> Self {
        Self::with_kind(file.into(), Kind::Kitchen)
    }

    /// `~/.microkitchen/rules.toml`; a missing file means no rules.
    pub fn global(file: impl Into<PathBuf>) -> Self {
        Self::with_kind(file.into(), Kind::Global)
    }

    fn with_kind(file: PathBuf, kind: Kind) -> Self {
        let rules = match kind {
            // Before the kitchen file is first read, only the built-ins apply.
            Kind::Kitchen => Rules {
                allow: builtin_allow().collect(),
                deny: Vec::new(),
            },
            Kind::Global => Rules::default(),
        };
        Self {
            file,
            kind,
            cache: Mutex::new(Cached {
                stamp: None,
                rules: Arc::new(rules),
            }),
        }
    }

    /// The rules as of the file's current contents. If the file cannot be
    /// read or parsed, the last good rules stay in effect.
    pub fn current(&self) -> Arc<Rules> {
        let metadata = fs::metadata(&self.file);
        let mut cache = self.cache.lock().unwrap();
        if self.kind == Kind::Global
            && metadata
                .as_ref()
                .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
        {
            cache.rules = Arc::new(Rules::default());
            cache.stamp = None;
            return cache.rules.clone();
        }
        let stamp = metadata
            .ok()
            .map(|m| (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len()));
        if stamp.is_some() && cache.stamp == stamp {
            return cache.rules.clone();
        }
        let parsed = fs::read_to_string(&self.file).map(|text| match self.kind {
            Kind::Kitchen => parse_rules(&self.file, &text),
            Kind::Global => parse_global_rules(&text),
        });
        match parsed {
            Ok(Some(rules)) => {
                cache.rules = Arc::new(rules);
                cache.stamp = stamp;
            }
            Ok(None) => {
                tracing::error!(file = %self.file.display(), "rules file is not valid; keeping the previous rules")
            }
            Err(error) => {
                tracing::error!(%error, file = %self.file.display(), "cannot read the rules file; keeping the previous rules")
            }
        }
        cache.rules.clone()
    }
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// The `allow`/`deny` lists of a kitchen file, plus [`BUILTIN_ALLOW`];
/// invalid entries are skipped.
pub fn parse_rules(file: &Path, text: &str) -> Option<Rules> {
    let source = Source::new(file, text);
    let dir = file.parent().unwrap_or(Path::new("/"));
    let mut diagnostics = Diagnostics::default();
    let kitchen = schema::parse(&source, dir, &mut diagnostics)?;
    let mut allow = kitchen.config.network.allow;
    allow.extend(builtin_allow());
    Some(Rules {
        allow,
        deny: kitchen.config.network.deny,
    })
}

/// The top-level `allow`/`deny` lists of `rules.toml`; `None` if the file is
/// not valid, invalid entries are skipped with a warning.
pub fn parse_global_rules(text: &str) -> Option<Rules> {
    let file: GlobalFile = toml::from_str(text).ok()?;
    let patterns = |list: Vec<String>| {
        list.into_iter()
            .filter_map(|entry| match entry.parse::<HostPattern>() {
                Ok(pattern) => Some(pattern),
                Err(error) => {
                    tracing::warn!(entry, %error, "ignoring an invalid rule in rules.toml");
                    None
                }
            })
            .collect()
    };
    Some(Rules {
        allow: patterns(file.allow),
        deny: patterns(file.deny),
    })
}

fn builtin_allow() -> impl Iterator<Item = HostPattern> {
    BUILTIN_ALLOW
        .iter()
        .map(|host| host.parse().expect("built-in patterns are valid"))
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const BUILTINS: usize = BUILTIN_ALLOW.len();

    #[test]
    fn reloads_on_change_and_keeps_rules_on_errors() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("mise.toml");
        fs::write(&file, "[_.microkitchen.network]\nallow = [\"a.com\"]\n").unwrap();
        let source = RuleSource::new(&file);
        assert_eq!(source.current().allow.len(), 1 + BUILTINS);

        fs::write(&file, "[_.microkitchen.network]\nallow = [\"a.com\", \"b.com\"]\ndeny = [\"c.com\", \"bad host\"]\n").unwrap();
        let rules = source.current();
        assert_eq!((rules.allow.len(), rules.deny.len()), (2 + BUILTINS, 1));

        fs::write(&file, "[_.microkitchen.network\n").unwrap();
        assert_eq!(
            source.current().allow.len(),
            2 + BUILTINS,
            "broken files keep the last good rules"
        );
    }

    #[test]
    fn legacy_tables_count() {
        let rules = parse_rules(
            Path::new("/p/mise.toml"),
            "[microkitchen.network]\ndeny = [\"x.com\"]\n",
        )
        .unwrap();
        assert_eq!(rules.deny.len(), 1);
    }

    #[test]
    fn builtins_apply_with_and_without_a_kitchen_file() {
        let ntp: HostPattern = "ntp.ubuntu.com".parse().unwrap();
        let rules = parse_rules(Path::new("/p/mise.toml"), "").unwrap();
        assert!(rules.allow.contains(&ntp));

        let missing = RuleSource::new("/nonexistent/mise.toml");
        assert!(missing.current().allow.contains(&ntp));
    }

    #[test]
    fn global_rules_have_no_builtins_and_may_be_missing() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("rules.toml");
        let source = RuleSource::global(&file);
        assert_eq!(*source.current(), Rules::default());

        fs::write(
            &file,
            "allow = [\"*.npmjs.org\", \"bad host\"]\ndeny = [\"x.com\"]\n",
        )
        .unwrap();
        let rules = source.current();
        assert_eq!((rules.allow.len(), rules.deny.len()), (1, 1));

        fs::write(&file, "allow = \"not a list\"\n").unwrap();
        assert_eq!(
            source.current().allow.len(),
            1,
            "broken files keep the last good rules"
        );

        fs::remove_file(&file).unwrap();
        assert_eq!(
            *source.current(),
            Rules::default(),
            "deleting the file drops the rules"
        );
        assert!(parse_global_rules("[network]\nallow = []\n").is_none());
    }
}
