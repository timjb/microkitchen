//! Stable sandbox names: `mk-<dir-slug>-<8 hex of sha256(kitchen file path)>`.

use std::path::Path;

use sha2::{Digest, Sha256};

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

pub const NAME_PREFIX: &str = "mk-";

/// Keeps names well below microsandbox's 128-byte limit.
const MAX_SLUG_LEN: usize = 48;

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// The sandbox name for a kitchen file, stable for as long as the file stays put.
pub fn sandbox_name(kitchen_file: &Path, kitchen_dir: &Path) -> String {
    let digest = Sha256::digest(kitchen_file.as_os_str().as_encoded_bytes());
    let bytes: &[u8] = digest.as_ref();
    let dir_name = kitchen_dir
        .file_name()
        .map(|n| n.to_string_lossy())
        .unwrap_or_default();
    format!(
        "{NAME_PREFIX}{}-{}",
        slug(&dir_name),
        hex::encode(&bytes[..4])
    )
}

/// Lowercase ASCII letters and digits, other runs collapsed into one `-`.
fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars() {
        if out.len() >= MAX_SLUG_LEN {
            break;
        }
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    let out = out.trim_end_matches('-');
    if out.is_empty() {
        "kitchen".to_owned()
    } else {
        out.to_owned()
    }
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_stable_and_distinct() {
        let a = sandbox_name(
            Path::new("/home/u/My Project/mise.toml"),
            Path::new("/home/u/My Project"),
        );
        assert!(a.starts_with("mk-my-project-"), "{a}");
        assert_eq!(a.len(), "mk-my-project-".len() + 8);
        assert_eq!(
            a,
            sandbox_name(
                Path::new("/home/u/My Project/mise.toml"),
                Path::new("/home/u/My Project")
            )
        );
        let b = sandbox_name(
            Path::new("/home/u/My Project/mise.local.toml"),
            Path::new("/home/u/My Project"),
        );
        assert_ne!(a, b);
    }

    #[test]
    fn slugs() {
        assert_eq!(slug("--Hello, World!--"), "hello-world");
        assert_eq!(slug("日本"), "kitchen");
        assert_eq!(slug(""), "kitchen");
        assert_eq!(slug(&"a".repeat(200)).len(), MAX_SLUG_LEN);
    }
}
