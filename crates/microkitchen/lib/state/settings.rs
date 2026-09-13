//! User settings for microkitchen itself: `~/.microkitchen/config.toml`.
//!
//! ```toml
//! mise_version = "2026.9.6"      # mise installed in guests (default: latest)
//!
//! [approval]
//! dialog = "auto"                # "auto", "zenity", "kdialog", "osascript" or "none"
//! headless = "deny"              # no dialog available: "deny" or "queue" for `net decide`
//! timeout_secs = 60              # unanswered approvals deny the flow
//! max_prompts = 20               # more prompts than this within window_secs
//! window_secs = 600              #   switch a sandbox to deny-all (`net resume`)
//!
//! [broker]
//! port_range = [40000, 49999]    # per-sandbox resolver and proxy ports
//! upstream_dns = ["1.1.1.1"]     # default: the host's /etc/resolv.conf
//! ```

use std::fs;
use std::time::Duration;

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
    pub approval: ApprovalSettings,
    pub broker: BrokerSettings,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ApprovalSettings {
    /// Which desktop dialog shows approvals.
    pub dialog: DialogSetting,
    /// What happens when no dialog can be shown. Explicit, never inferred.
    pub headless: HeadlessFallback,
    /// Seconds before an unanswered approval denies the flow (not remembered).
    pub timeout_secs: u64,
    /// More prompts than this within `window_secs` switch a sandbox to deny-all.
    pub max_prompts: u32,
    pub window_secs: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DialogSetting {
    /// osascript on macOS; zenity or kdialog when a display is available.
    #[default]
    Auto,
    Zenity,
    Kdialog,
    Osascript,
    /// Never show dialogs; use the headless fallback.
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HeadlessFallback {
    /// Deny the flow.
    #[default]
    Deny,
    /// Hold it for `microkitchen net pending` / `net decide`.
    Queue,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct BrokerSettings {
    /// Inclusive range for per-sandbox loopback ports.
    pub port_range: (u16, u16),
    /// Upstream resolvers (`ip` or `ip:port`); empty means `/etc/resolv.conf`.
    pub upstream_dns: Vec<String>,
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
        let approval = &settings.approval;
        if approval.max_prompts == 0 || approval.window_secs == 0 || approval.timeout_secs == 0 {
            bail!("approval.max_prompts, window_secs and timeout_secs must be at least 1");
        }
        let (low, high) = settings.broker.port_range;
        if low < 1024 || low > high {
            bail!("broker.port_range must be [low, high] with 1024 <= low <= high");
        }
        Ok(settings)
    }
}

impl ApprovalSettings {
    pub fn timeout(&self) -> Duration {
        Duration::from_secs(self.timeout_secs)
    }

    pub fn window(&self) -> Duration {
        Duration::from_secs(self.window_secs)
    }
}

//--------------------------------------------------------------------------------------------------
// Trait Implementations
//--------------------------------------------------------------------------------------------------

impl Default for ApprovalSettings {
    fn default() -> Self {
        Self {
            dialog: DialogSetting::Auto,
            headless: HeadlessFallback::Deny,
            timeout_secs: 60,
            max_prompts: 20,
            window_secs: 600,
        }
    }
}

impl Default for BrokerSettings {
    fn default() -> Self {
        Self {
            port_range: (40000, 49999),
            upstream_dns: Vec::new(),
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
    fn approval_and_broker_sections() {
        let s = Settings::parse(
            "[approval]\nheadless = \"queue\"\ntimeout_secs = 5\n\n[broker]\nport_range = [50000, 50100]\nupstream_dns = [\"1.1.1.1\"]\n",
        )
        .unwrap();
        assert_eq!(s.approval.headless, HeadlessFallback::Queue);
        assert_eq!(s.approval.timeout(), Duration::from_secs(5));
        assert_eq!(s.broker.port_range, (50000, 50100));
        assert!(Settings::parse("[broker]\nport_range = [80, 90]\n").is_err());
        assert!(Settings::parse("[approval]\nheadless = \"maybe\"\n").is_err());
        assert_eq!(
            Settings::default().approval.headless,
            HeadlessFallback::Deny
        );
    }

    #[test]
    fn dialogs_and_rate_limit() {
        let s =
            Settings::parse("[approval]\ndialog = \"none\"\nmax_prompts = 3\nwindow_secs = 30\n")
                .unwrap();
        assert_eq!(s.approval.dialog, DialogSetting::None);
        assert_eq!(s.approval.max_prompts, 3);
        assert_eq!(s.approval.window(), Duration::from_secs(30));
        let defaults = Settings::default().approval;
        assert_eq!(
            (defaults.dialog, defaults.max_prompts, defaults.window_secs),
            (DialogSetting::Auto, 20, 600)
        );
        assert!(Settings::parse("[approval]\ndialog = \"gtk\"\n").is_err());
        assert!(Settings::parse("[approval]\nmax_prompts = 0\n").is_err());
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
