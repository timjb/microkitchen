//! Machine setup inside the guest: install mise, then `mise bootstrap`.
//!
//! The guest copy of the kitchen file is linked as mise's global config
//! (see [`super::lifecycle::write_guest_config`]), so bootstrap runs from
//! `/root` and installed tools work from any directory, including mounts.

use std::fs::{self, File};
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result, bail};
use microsandbox::{ExecEvent, Sandbox};

use super::labels;

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// The guest script. `mise_version` pins the installed mise (`2026.9.6`);
/// it must already be validated (see [`crate::state::settings`]).
pub fn script(mise_version: Option<&str>) -> String {
    let version = mise_version
        .map(|v| format!("MISE_VERSION=v{} ", v.trim_start_matches('v')))
        .unwrap_or_default();
    format!(
        r#"set -eu
export MISE_YES=1
if ! command -v mise >/dev/null 2>&1; then
    echo "microkitchen: installing mise"
    curl -fsSL https://mise.run | {version}sh
fi
cd /root
mise --version
mise bootstrap --yes
"#
    )
}

/// Run `script` in the guest, streaming output to `log` and, when `echo` is
/// set, to this process's stdout/stderr.
pub async fn run(sandbox: &Sandbox, script: &str, log: &Path, echo: bool) -> Result<()> {
    if let Some(dir) = log.parent() {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    }
    let mut log_file = File::create(log).with_context(|| format!("creating {}", log.display()))?;

    let mut handle = sandbox
        .shell_stream(script)
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

    #[test]
    fn pins_the_mise_version() {
        assert!(
            script(Some("2026.9.6"))
                .contains("curl -fsSL https://mise.run | MISE_VERSION=v2026.9.6 sh")
        );
        assert!(script(Some("v2026.9.6")).contains("MISE_VERSION=v2026.9.6 sh"));
        assert!(script(None).contains("https://mise.run | sh"));
    }
}
