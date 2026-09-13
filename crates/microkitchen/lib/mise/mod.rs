//! Running the host's `mise` and interpreting what it reports.

pub mod declarations;
pub mod env;
pub mod render;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow};
use serde::Deserialize;

use self::env::MiseEnvEntry;

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

/// Overrides the mise executable (default: `mise` on `PATH`).
pub const MISE_PROGRAM_ENV: &str = "MICROKITCHEN_MISE";

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// Handle on the host's mise executable.
#[derive(Debug, Clone)]
pub struct Mise {
    program: OsString,
}

/// Output of a mise command that ran but failed: its cleaned-up stderr.
pub type MiseFailure = String;

#[derive(Deserialize)]
struct ConfigEntry {
    path: PathBuf,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl Mise {
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
        }
    }

    /// `$MICROKITCHEN_MISE`, else `mise`.
    pub fn from_env() -> Self {
        Self::new(std::env::var_os(MISE_PROGRAM_ENV).unwrap_or_else(|| "mise".into()))
    }

    /// Config files in use for `dir`, highest precedence first.
    pub fn config_files(&self, dir: &Path) -> Result<Vec<PathBuf>> {
        let mut command = self.command(dir);
        command
            .args(["config", "ls", "--json"])
            .env("MISE_NO_ENV", "1");
        let stdout = self
            .run(&mut command)?
            .map_err(|message| anyhow!("`mise config ls` failed: {message}"))?;
        let entries: Vec<ConfigEntry> =
            serde_json::from_slice(&stdout).context("parsing `mise config ls --json`")?;
        Ok(entries.into_iter().map(|e| e.path).collect())
    }

    /// `mise env --json-extended` in `dir` with auto-install off, so tools are
    /// never installed on the host just to print the environment.
    pub fn env(&self, dir: &Path) -> Result<Result<BTreeMap<String, MiseEnvEntry>, MiseFailure>> {
        let mut command = self.command(dir);
        command
            .args(["-q", "env", "--json-extended"])
            .env("MISE_AUTO_INSTALL", "false");
        match self.run(&mut command)? {
            Ok(stdout) => {
                Ok(Ok(serde_json::from_slice(&stdout)
                    .context("parsing `mise env --json-extended`")?))
            }
            Err(message) => Ok(Err(message)),
        }
    }

    fn command(&self, dir: &Path) -> Command {
        let mut command = Command::new(&self.program);
        command.arg("-C").arg(dir).stdin(Stdio::null());
        command
    }

    /// Run to completion: `Err` if mise could not be started, `Ok(Err)` if it failed.
    fn run(&self, command: &mut Command) -> Result<Result<Vec<u8>, MiseFailure>> {
        let output = command.output().map_err(|error| {
            if error.kind() == io::ErrorKind::NotFound {
                anyhow!(
                    "`{}` was not found; install mise (https://mise.jdx.dev) or set {MISE_PROGRAM_ENV}",
                    self.program.to_string_lossy()
                )
            } else {
                anyhow!(error).context(format!("running {}", self.program.to_string_lossy()))
            }
        })?;
        if output.status.success() {
            Ok(Ok(output.stdout))
        } else {
            Ok(Err(clean_stderr(&output.stderr)))
        }
    }
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// mise's error lines without the prefix and the version/verbose boilerplate.
fn clean_stderr(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let lines: Vec<&str> = text
        .lines()
        .map(|line| line.strip_prefix("mise ERROR ").unwrap_or(line).trim_end())
        .filter(|line| {
            !line.is_empty()
                && !line.starts_with("Version: ")
                && !line.starts_with("Run with --verbose")
        })
        .collect();
    if lines.is_empty() {
        "mise exited with an error and printed nothing".into()
    } else {
        lines.join("\n")
    }
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_mise_boilerplate() {
        let stderr = b"mise ERROR Required environment variable 'REQ' is not defined.\n\
mise ERROR Version: 2026.9.6 linux-x64 (2026-09-12)\n\
mise ERROR Run with --verbose or MISE_VERBOSE=1 for more information\n";
        assert_eq!(
            clean_stderr(stderr),
            "Required environment variable 'REQ' is not defined."
        );
        assert!(clean_stderr(b"").contains("printed nothing"));
    }

    #[test]
    fn missing_program_is_explained() {
        let mise = Mise::new("/nonexistent/mise");
        let error = mise.config_files(Path::new("/")).unwrap_err();
        assert!(error.to_string().contains("install mise"), "{error}");
    }
}
