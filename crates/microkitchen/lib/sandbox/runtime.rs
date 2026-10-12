//! The microsandbox runtime (`msb` and libkrunfw): microkitchen keeps it, and
//! the rest of microsandbox's state, in its own home instead of the global
//! `~/.microsandbox`, and installs it there on first use.

use std::path::PathBuf;

use anyhow::{Context, Result};
use microsandbox::MicrosandboxError;
use microsandbox::setup::{self, InstallOptions, RuntimeOrigin};

use crate::state::Home;

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

/// microsandbox's home; set by the user, it overrides [`Home::microsandbox_dir`].
pub const MSB_HOME_ENV: &str = "MSB_HOME";

/// The runtime version [`install`] downloads: the microsandbox crate's.
pub const VERSION: &str = microsandbox_utils::PREBUILT_VERSION;

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// `$MSB_HOME` if set, else `home`'s `microsandbox` directory.
pub fn msb_home(home: &Home, env: impl Fn(&str) -> Option<String>) -> PathBuf {
    env(MSB_HOME_ENV)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| home.microsandbox_dir())
}

/// Point microsandbox, and the `msb` processes it spawns, at [`msb_home`].
///
/// # Safety
///
/// Changes the environment: call it while the process is single-threaded.
pub unsafe fn export_home(home: &Home) {
    let dir = msb_home(home, |name| std::env::var(name).ok());
    // SAFETY: guaranteed by the caller.
    unsafe { std::env::set_var(MSB_HOME_ENV, dir) };
}

/// What [`ensure`] has to do before a sandbox can start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Need {
    /// A runtime of [`VERSION`] is in the home, or one was chosen explicitly
    /// (`MSB_PATH`, microsandbox's config), which is left alone.
    Nothing,
    /// The home has no runtime.
    Install,
    /// The home has another version, or one whose version cannot be read,
    /// from an earlier microkitchen; it is replaced.
    Upgrade,
}

/// Inspect the runtime microsandbox would use.
pub fn check() -> Result<Need> {
    let config = microsandbox::config::config().context("reading the microsandbox config")?;
    let runtime = match setup::resolve_runtime(&config) {
        Ok(runtime) => runtime,
        Err(MicrosandboxError::RuntimeNotInstalled(_)) => return Ok(Need::Install),
        Err(error) => return Err(error).context("finding the microsandbox runtime"),
    };
    if runtime.origin != RuntimeOrigin::Home {
        return Ok(Need::Nothing);
    }
    let installed = setup::resolve_runtime_version(&runtime.msb_path)
        .ok()
        .flatten()
        .map(|v| v.to_string());
    Ok(compare(installed.as_deref()))
}

fn compare(installed: Option<&str>) -> Need {
    if installed == Some(VERSION) {
        Need::Nothing
    } else {
        Need::Upgrade
    }
}

/// Download the runtime matching the microsandbox crate ([`VERSION`]) into
/// the home, replacing what is there.
pub async fn install() -> Result<()> {
    let config = microsandbox::config::config().context("reading the microsandbox config")?;
    let options = InstallOptions {
        force: true,
        ..InstallOptions::default()
    };
    setup::install_runtime(&config, options)
        .await
        .with_context(|| {
            format!(
                "installing the microsandbox runtime into {}",
                config.home().display()
            )
        })?;
    Ok(())
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_the_microkitchen_home() {
        let home = Home::at("/h/.microkitchen");
        assert_eq!(
            msb_home(&home, |_| None),
            PathBuf::from("/h/.microkitchen/microsandbox")
        );
        assert_eq!(
            msb_home(&home, |_| Some(String::new())),
            PathBuf::from("/h/.microkitchen/microsandbox")
        );
    }

    #[test]
    fn msb_home_overrides() {
        let home = Home::at("/h/.microkitchen");
        let env = |name: &str| (name == MSB_HOME_ENV).then(|| "/elsewhere".to_owned());
        assert_eq!(msb_home(&home, env), PathBuf::from("/elsewhere"));
    }

    #[test]
    fn replaces_other_versions() {
        assert_eq!(compare(Some(VERSION)), Need::Nothing);
        assert_eq!(compare(Some("0.7.2")), Need::Upgrade);
        assert_eq!(compare(None), Need::Upgrade);
    }
}
