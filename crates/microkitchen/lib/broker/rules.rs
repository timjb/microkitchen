//! Persistent rules come from the kitchen file, reloaded when it changes.
//!
//! Hand edits, `remodel` and approval answers therefore apply to the next
//! flow without restarting anything.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

use super::decision::Rules;
use crate::config::schema;
use crate::config::{Diagnostics, Source};

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

pub struct RuleSource {
    file: PathBuf,
    cache: Mutex<Cached>,
}

#[derive(Default)]
struct Cached {
    stamp: Option<(SystemTime, u64)>,
    rules: Arc<Rules>,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl RuleSource {
    pub fn new(file: impl Into<PathBuf>) -> Self {
        Self {
            file: file.into(),
            cache: Mutex::default(),
        }
    }

    /// The rules as of the file's current contents. If the file cannot be
    /// read or parsed, the last good rules stay in effect.
    pub fn current(&self) -> Arc<Rules> {
        let stamp = fs::metadata(&self.file)
            .ok()
            .map(|m| (m.modified().unwrap_or(SystemTime::UNIX_EPOCH), m.len()));
        let mut cache = self.cache.lock().unwrap();
        if stamp.is_some() && cache.stamp == stamp {
            return cache.rules.clone();
        }
        match fs::read_to_string(&self.file) {
            Ok(text) => match parse_rules(&self.file, &text) {
                Some(rules) => {
                    cache.rules = Arc::new(rules);
                    cache.stamp = stamp;
                }
                None => {
                    tracing::error!(file = %self.file.display(), "kitchen file is not valid TOML; keeping the previous rules")
                }
            },
            Err(error) => {
                tracing::error!(%error, file = %self.file.display(), "cannot read the kitchen file; keeping the previous rules")
            }
        }
        cache.rules.clone()
    }
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// The `allow`/`deny` lists of a kitchen file; invalid entries are skipped.
pub fn parse_rules(file: &Path, text: &str) -> Option<Rules> {
    let source = Source::new(file, text);
    let dir = file.parent().unwrap_or(Path::new("/"));
    let mut diagnostics = Diagnostics::default();
    let kitchen = schema::parse(&source, dir, &mut diagnostics)?;
    Some(Rules {
        allow: kitchen.config.network.allow,
        deny: kitchen.config.network.deny,
    })
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reloads_on_change_and_keeps_rules_on_errors() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("mise.toml");
        fs::write(&file, "[_.microkitchen.network]\nallow = [\"a.com\"]\n").unwrap();
        let source = RuleSource::new(&file);
        assert_eq!(source.current().allow.len(), 1);

        fs::write(&file, "[_.microkitchen.network]\nallow = [\"a.com\", \"b.com\"]\ndeny = [\"c.com\", \"bad host\"]\n").unwrap();
        let rules = source.current();
        assert_eq!((rules.allow.len(), rules.deny.len()), (2, 1));

        fs::write(&file, "[_.microkitchen.network\n").unwrap();
        assert_eq!(
            source.current().allow.len(),
            2,
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
}
