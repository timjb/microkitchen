//! Per-sandbox state: `~/.microkitchen/sandboxes/<name>/state.json`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::{Home, write_atomic};
use crate::config::schema::KitchenConfig;

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxState {
    pub name: String,
    pub kitchen_file: PathBuf,
    pub config_hash: String,

    /// The configuration the sandbox currently runs with; `remodel` diffs against it.
    pub applied: KitchenConfig,

    /// The kitchen file's text as last applied, for `remodel`'s text diff.
    /// Absent for sandboxes created before milestone 6.
    #[serde(default)]
    pub applied_text: Option<String>,

    /// Environment variables microkitchen set from the kitchen file. Only
    /// these may be removed by `remodel`: the image sets its own.
    #[serde(default)]
    pub env_keys: Vec<String>,

    /// The guest's `mise.toml` is out of date (changed while the sandbox was
    /// stopped); written at the next start.
    #[serde(default)]
    pub guest_config_pending: bool,

    #[serde(default)]
    pub bootstrapped: bool,

    /// Broker endpoints, fixed for the sandbox's lifetime (milestone 4).
    #[serde(default)]
    pub resolver_port: Option<u16>,
    #[serde(default)]
    pub proxy_port: Option<u16>,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl SandboxState {
    pub fn path(home: &Home, name: &str) -> PathBuf {
        home.sandbox_dir(name).join("state.json")
    }

    pub fn load(home: &Home, name: &str) -> Result<Option<Self>> {
        let path = Self::path(home, name);
        match fs::read(&path) {
            Ok(bytes) => Ok(Some(
                serde_json::from_slice(&bytes)
                    .with_context(|| format!("parsing {}", path.display()))?,
            )),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn save(&self, home: &Home) -> Result<()> {
        home.ensure()?;
        let path = Self::path(home, &self.name);
        let dir = path.parent().expect("state file has a directory");
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let json = serde_json::to_vec_pretty(self)?;
        write_atomic(&path, &json)
    }

    /// Delete the sandbox's state and logs.
    pub fn purge(home: &Home, name: &str) -> Result<()> {
        for dir in [home.sandbox_dir(name), home.logs_dir().join(name)] {
            remove_dir_if_exists(&dir)?;
        }
        Ok(())
    }
}

fn remove_dir_if_exists(dir: &Path) -> Result<()> {
    match fs::remove_dir_all(dir) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("removing {}", dir.display())),
    }
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_and_purges() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path());
        assert_eq!(SandboxState::load(&home, "mk-a").unwrap(), None);

        let state = SandboxState {
            name: "mk-a".into(),
            kitchen_file: "/p/mise.toml".into(),
            config_hash: "abc".into(),
            applied: KitchenConfig::default(),
            applied_text: Some("[tools]\n".into()),
            env_keys: vec!["GREETING".into()],
            guest_config_pending: false,
            bootstrapped: false,
            resolver_port: None,
            proxy_port: None,
        };
        state.save(&home).unwrap();
        assert_eq!(SandboxState::load(&home, "mk-a").unwrap(), Some(state));

        SandboxState::purge(&home, "mk-a").unwrap();
        assert_eq!(SandboxState::load(&home, "mk-a").unwrap(), None);
        SandboxState::purge(&home, "mk-a").unwrap();
    }
}
