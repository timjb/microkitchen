//! The per-sandbox SOCKS5 password: `~/.microkitchen/sandboxes/<name>/proxy-secret`.
//!
//! microsandbox reads the password from a host environment variable at every
//! sandbox start, so microkitchen exports it before starting a sandbox; the
//! broker reads the same file to verify the handshake, and to re-register
//! sandboxes after it restarts.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use anyhow::{Context, Result, anyhow};
use sha2::{Digest, Sha256};

use super::Home;

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

pub fn path(home: &Home, name: &str) -> PathBuf {
    home.sandbox_dir(name).join("proxy-secret")
}

/// The host environment variable microsandbox reads the password from.
pub fn env_var(name: &str) -> String {
    let digest = Sha256::digest(name.as_bytes());
    let bytes: &[u8] = digest.as_ref();
    format!("MK_PROXY_SECRET_{}", hex::encode_upper(&bytes[..4]))
}

pub fn load(home: &Home, name: &str) -> Result<Option<String>> {
    let path = path(home, name);
    match fs::read_to_string(&path) {
        Ok(secret) => Ok(Some(secret.trim().to_owned())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}

/// The sandbox's secret, generated (256 random bits, mode 0600) on first use.
pub fn load_or_create(home: &Home, name: &str) -> Result<String> {
    if let Some(secret) = load(home, name)? {
        return Ok(secret);
    }
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| anyhow!("generating a proxy secret: {e}"))?;
    let secret = hex::encode(bytes);

    home.ensure()?;
    let path = path(home, name);
    let dir = path.parent().expect("secret file has a directory");
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    match options.open(&path) {
        Ok(mut file) => {
            file.write_all(secret.as_bytes())
                .with_context(|| format!("writing {}", path.display()))?;
            Ok(secret)
        }
        // Another microkitchen process won the race; use its secret.
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            load(home, name)?.context("proxy secret disappeared")
        }
        Err(error) => Err(error).with_context(|| format!("creating {}", path.display())),
    }
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_once_and_privately() {
        let dir = tempfile::tempdir().unwrap();
        let home = Home::at(dir.path());
        assert_eq!(load(&home, "mk-a").unwrap(), None);
        let secret = load_or_create(&home, "mk-a").unwrap();
        assert_eq!(secret.len(), 64);
        assert_eq!(load_or_create(&home, "mk-a").unwrap(), secret);
        assert_ne!(load_or_create(&home, "mk-b").unwrap(), secret);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(path(&home, "mk-a"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn env_var_names_are_stable_and_distinct() {
        assert_eq!(env_var("mk-a"), env_var("mk-a"));
        assert_ne!(env_var("mk-a"), env_var("mk-b"));
        assert!(env_var("mk-a").starts_with("MK_PROXY_SECRET_"));
    }
}
