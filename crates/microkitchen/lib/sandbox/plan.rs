//! Everything needed to create a project's sandbox, derived from a valid [`Project`].

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};

use super::labels;
use super::naming::sandbox_name;
use crate::config::Project;
use crate::config::schema::{KitchenConfig, SecretConfig};
use crate::mise::render::render_guest_config;

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// A resolved description of the sandbox; secret values included, so never logged.
pub struct SandboxPlan {
    pub name: String,
    pub kitchen_file: PathBuf,
    pub config: KitchenConfig,
    pub config_hash: String,

    /// Plain environment variables.
    pub env: BTreeMap<String, String>,

    /// Secrets: value and allowed hosts.
    pub secrets: BTreeMap<String, (String, SecretConfig)>,

    /// Contents of `/root/kitchen/mise.toml` in the guest.
    pub guest_config: String,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl SandboxPlan {
    /// Build the plan; fails if the project did not validate.
    pub fn from_project(project: &Project) -> Result<Self> {
        if project.diagnostics.has_errors() {
            bail!(
                "the configuration is invalid; see the errors above or run `microkitchen validate`"
            );
        }
        let kitchen = project
            .kitchen
            .as_ref()
            .context("the kitchen file could not be parsed")?;
        let resolved = project
            .env
            .as_ref()
            .context("the environment could not be resolved")?;
        let discovery = &project.discovery;

        let config = kitchen.config.clone();
        let mut env = BTreeMap::new();
        let mut secrets = BTreeMap::new();
        for (name, value) in &resolved.values {
            match config.secrets.get(name) {
                Some(secret) => {
                    secrets.insert(name.clone(), (value.clone(), secret.clone()));
                }
                None => {
                    env.insert(name.clone(), value.clone());
                }
            }
        }

        let text = std::fs::read_to_string(&discovery.kitchen_file)
            .with_context(|| format!("reading {}", discovery.kitchen_file.display()))?;

        Ok(Self {
            name: sandbox_name(&discovery.kitchen_file, &discovery.kitchen_dir),
            kitchen_file: discovery.kitchen_file.clone(),
            config_hash: config_hash(&config),
            config,
            env,
            secrets,
            guest_config: render_guest_config(&text)?,
        })
    }

    /// Labels set at creation.
    pub fn labels(&self) -> Vec<(&'static str, String)> {
        vec![
            (labels::MANAGED, "true".to_owned()),
            (
                labels::CONFIG,
                self.kitchen_file.to_string_lossy().into_owned(),
            ),
            (labels::CONFIG_HASH, self.config_hash.clone()),
            (labels::VERSION, env!("CARGO_PKG_VERSION").to_owned()),
            (labels::BOOTSTRAPPED, "false".to_owned()),
        ]
    }
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// Short stable hash of the normalized configuration.
pub fn config_hash(config: &KitchenConfig) -> String {
    let json = serde_json::to_vec(config).expect("KitchenConfig serializes");
    let digest = Sha256::digest(&json);
    let bytes: &[u8] = digest.as_ref();
    hex::encode(&bytes[..8])
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_tracks_the_config() {
        let a = KitchenConfig::default();
        let mut b = KitchenConfig::default();
        assert_eq!(config_hash(&a), config_hash(&b));
        assert_eq!(config_hash(&a).len(), 16);
        b.cpus = 3;
        assert_ne!(config_hash(&a), config_hash(&b));
    }
}
