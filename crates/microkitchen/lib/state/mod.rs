//! Paths under `~/.microkitchen` and small filesystem helpers.

pub mod sandbox;

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use tempfile::NamedTempFile;

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

/// Overrides the state directory (also `--home`).
pub const HOME_ENV: &str = "MICROKITCHEN_HOME";

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// The microkitchen state directory, `~/.microkitchen` by default.
#[derive(Debug, Clone)]
pub struct Home {
    root: PathBuf,
}

/// Exclusive lock serializing config writers (CLI and broker); released on drop.
#[derive(Debug)]
pub struct WriteLock {
    _file: File,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl Home {
    /// `explicit`, else `$MICROKITCHEN_HOME`, else `$HOME/.microkitchen`.
    pub fn resolve(explicit: Option<PathBuf>) -> Result<Self> {
        let env = |name| std::env::var_os(name).filter(|v| !v.is_empty());
        let root = match explicit.or_else(|| env(HOME_ENV).map(PathBuf::from)) {
            Some(root) => root,
            None => {
                let home = env("HOME").context("HOME is not set; pass --home")?;
                PathBuf::from(home).join(".microkitchen")
            }
        };
        Ok(Self::at(std::path::absolute(&root).unwrap_or(root)))
    }

    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `config.toml`: user settings for microkitchen itself.
    pub fn config_file(&self) -> PathBuf {
        self.root.join("config.toml")
    }

    pub fn broker_dir(&self) -> PathBuf {
        self.root.join("broker")
    }

    pub fn sandbox_dir(&self, name: &str) -> PathBuf {
        self.root.join("sandboxes").join(name)
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }

    /// Create the state directory, private to the user, if it does not exist.
    pub fn ensure(&self) -> Result<()> {
        if self.root.is_dir() {
            return Ok(());
        }
        fs::create_dir_all(&self.root)
            .with_context(|| format!("creating {}", self.root.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&self.root, fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    /// Block until no other process is editing config files.
    pub fn lock_writers(&self) -> Result<WriteLock> {
        self.ensure()?;
        let path = self.root.join("write.lock");
        let file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .with_context(|| format!("opening {}", path.display()))?;
        file.lock()
            .with_context(|| format!("locking {}", path.display()))?;
        Ok(WriteLock { _file: file })
    }
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// Replace `path` with `contents` atomically, keeping its permissions.
pub fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let mut temp = NamedTempFile::new_in(dir)
        .with_context(|| format!("creating a temporary file in {}", dir.display()))?;
    temp.write_all(contents)?;
    if let Ok(metadata) = fs::metadata(path) {
        temp.as_file().set_permissions(metadata.permissions())?;
    }
    temp.as_file().sync_all()?;
    temp.persist(path)
        .map_err(|e| e.error)
        .with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_home_wins() {
        let home = Home::resolve(Some("/x/state".into())).unwrap();
        assert_eq!(home.root(), Path::new("/x/state"));
        assert_eq!(
            home.sandbox_dir("mk-a"),
            Path::new("/x/state/sandboxes/mk-a")
        );
    }

    #[cfg(unix)]
    #[test]
    fn atomic_write_keeps_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f");
        fs::write(&file, "old").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o640)).unwrap();
        write_atomic(&file, b"new").unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), "new");
        assert_eq!(
            fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }
}
