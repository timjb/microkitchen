//! Machine setup inside the guest: install mise and create chef as root,
//! then `mise bootstrap` as chef.
//!
//! The guest copy of the kitchen file is mise's system config (see
//! [`super::lifecycle::write_guest_config`] and [`super::build::GUEST_ENV`]),
//! so installed tools work from any directory, including mounts. Tools live in
//! [`GUEST_MISE_DATA_DIR`], owned by chef, who may add more with `mise use`.

use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};
use microsandbox::{ExecEvent, Sandbox};

use super::build::{GUEST_MISE_BIN, GUEST_MISE_DATA_DIR, MISE_CACHE_GUEST_PATH, ROOT};
use super::labels;
use crate::config::schema::CHEF;

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// One script of the bootstrap and the guest user it runs as.
pub struct Step {
    pub user: &'static str,
    pub script: String,
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// The guest scripts. `mise_version` pins the installed mise (`2026.9.6`);
/// it must already be validated (see [`crate::state::settings`]).
///
/// Root installs mise and sudo, lets the `sudo` group use it without a
/// password, and applies the accounts, which creates chef; chef then runs the
/// whole bootstrap (the accounts step again, a no-op unless something changed,
/// when mise elevates through sudo).
pub fn steps(mise_version: Option<&str>) -> Vec<Step> {
    let version = mise_version
        .map(|v| format!("MISE_VERSION=v{} ", v.trim_start_matches('v')))
        .unwrap_or_default();
    let root = format!(
        r#"set -eu
export MISE_YES=1
if ! command -v mise >/dev/null 2>&1; then
    echo "microkitchen: installing mise"
    curl -fsSL https://mise.run | {version}MISE_INSTALL_PATH={GUEST_MISE_BIN} sh
fi
if ! command -v sudo >/dev/null 2>&1; then
    echo "microkitchen: installing sudo"
    apt-get update -qq && apt-get install -y -qq sudo >/dev/null
fi
echo '%sudo ALL=(ALL:ALL) NOPASSWD: ALL' > /etc/sudoers.d/microkitchen
chmod 0440 /etc/sudoers.d/microkitchen
cd /root
mise --version
mise bootstrap accounts apply --yes
if ! id {CHEF} >/dev/null 2>&1; then
    echo "microkitchen: mise bootstrap did not create the {CHEF} user" >&2
    exit 1
fi
mkdir -p {GUEST_MISE_DATA_DIR} {MISE_CACHE_GUEST_PATH}
chown -R {CHEF}: {GUEST_MISE_DATA_DIR} {MISE_CACHE_GUEST_PATH}
"#
    );
    let chef = format!(
        r#"set -eu
export MISE_YES=1 USER={CHEF} LOGNAME={CHEF}
HOME="$(getent passwd {CHEF} | cut -d: -f6)"
export HOME
cd "$HOME"
mise bootstrap --yes
"#
    );
    vec![
        Step {
            user: ROOT,
            script: root,
        },
        Step {
            user: CHEF,
            script: chef,
        },
    ]
}

/// Run `steps` in the guest in order, streaming output to `log` and, when
/// `echo` is set, to this process's stdout/stderr. Stops at the first failure.
pub async fn run(sandbox: &Sandbox, steps: &[Step], log: &Path, echo: bool) -> Result<()> {
    if let Some(dir) = log.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut log_file = File::create(log).with_context(|| format!("creating {}", log.display()))?;
    for step in steps {
        run_step(sandbox, step, &mut log_file, log, echo).await?;
    }
    Ok(())
}

async fn run_step(
    sandbox: &Sandbox,
    step: &Step,
    log_file: &mut File,
    log: &Path,
    echo: bool,
) -> Result<()> {
    let mut handle = sandbox
        .shell_stream_with(step.script.as_str(), |e| e.user(step.user))
        .await
        .context("starting mise bootstrap in the sandbox")?;

    let mut exit_code = None;
    while let Some(event) = handle.recv().await {
        match event {
            ExecEvent::Stdout(bytes) => {
                log_file.write_all(&bytes)?;
                if echo {
                    let mut out = std::io::stdout();
                    out.write_all(&bytes)?;
                    out.flush()?;
                }
            }
            ExecEvent::Stderr(bytes) => {
                log_file.write_all(&bytes)?;
                if echo {
                    std::io::stderr().write_all(&bytes)?;
                }
            }
            ExecEvent::Exited { code } => {
                exit_code = Some(code);
                break;
            }
            ExecEvent::Failed(failure) => bail!("mise bootstrap could not start: {failure:?}"),
            _ => {}
        }
    }

    match exit_code {
        Some(0) => Ok(()),
        Some(code) => bail!(
            "mise bootstrap failed with exit code {code}; see {} and retry with `microkitchen bootstrap`",
            log.display()
        ),
        None => bail!("mise bootstrap ended without an exit status"),
    }
}

/// Record the outcome on the sandbox label. Best effort: microsandbox cannot
/// yet update labels of a running sandbox, so the change may only land at the
/// next start. `state.json` is the authoritative record.
pub async fn mark(sandbox: &Sandbox, bootstrapped: bool) {
    let value = bootstrapped.to_string();
    let live = sandbox
        .modify()
        .label(labels::BOOTSTRAPPED, value.as_str())
        .apply()
        .await;
    if let Err(error) = live {
        tracing::debug!(%error, "live label update refused; applying at next start");
        let deferred = sandbox
            .modify()
            .label(labels::BOOTSTRAPPED, value.as_str())
            .next_start()
            .apply()
            .await;
        if let Err(error) = deferred {
            tracing::warn!(%error, "could not record the bootstrap label");
        }
    }
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn root_script(mise_version: Option<&str>) -> String {
        let steps = steps(mise_version);
        assert_eq!(steps[0].user, ROOT);
        steps[0].script.clone()
    }

    #[test]
    fn pins_the_mise_version() {
        assert!(root_script(Some("2026.9.6")).contains(
            "curl -fsSL https://mise.run | MISE_VERSION=v2026.9.6 MISE_INSTALL_PATH=/usr/local/bin/mise sh"
        ));
        assert!(
            root_script(Some("v2026.9.6")).contains("MISE_VERSION=v2026.9.6 MISE_INSTALL_PATH")
        );
        assert!(root_script(None).contains("https://mise.run | MISE_INSTALL_PATH="));
    }

    #[test]
    fn root_creates_chef_and_chef_bootstraps() {
        let steps = steps(None);
        assert!(
            steps[0]
                .script
                .contains("mise bootstrap accounts apply --yes")
        );
        assert!(
            steps[0]
                .script
                .contains("%sudo ALL=(ALL:ALL) NOPASSWD: ALL")
        );
        assert_eq!(steps[1].user, CHEF);
        assert!(steps[1].script.ends_with("mise bootstrap --yes\n"));
    }
}
