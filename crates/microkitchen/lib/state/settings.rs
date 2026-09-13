//! User settings for microkitchen itself: `~/.microkitchen/config.toml`.

use std::fs;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use super::Home;

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// Every key is optional; a missing file means defaults.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// mise version installed in the guest (`"2026.9.6"`); latest when unset.
    pub mise_version: Option<String>,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl Settings {
    pub fn load(home: &Home) -> Result<Self> {
        let path = home.config_file();
        let text = match fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => return Err(error).with_context(|| format!("reading {}", path.display())),
        };
        Self::parse(&text).with_context(|| format!("in {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let settings: Self = toml::from_str(text)?;
        if let Some(version) = &settings.mise_version {
            let digits = version.trim_start_matches('v');
            if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
                bail!("mise_version must look like \"2026.9.6\", got {version:?}");
            }
        }
        Ok(settings)
    }
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_validates() {
        assert_eq!(Settings::parse("").unwrap(), Settings::default());
        assert_eq!(
            Settings::parse("mise_version = \"2026.9.6\"\n")
                .unwrap()
                .mise_version
                .as_deref(),
            Some("2026.9.6")
        );
        assert!(Settings::parse("mise_version = \"1; rm -rf /\"\n").is_err());
        assert!(Settings::parse("unknown = 1\n").is_err());
    }

    #[test]
    fn missing_file_means_defaults() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            Settings::load(&Home::at(dir.path())).unwrap(),
            Settings::default()
        );
    }
}
