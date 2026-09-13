//! Finding the kitchen file the way mise finds its config files.
//!
//! mise decides which files are loaded and in which order; microkitchen only
//! picks the kitchen file among them. `MISE_NO_ENV=1` keeps `mise config ls`
//! from failing on unset required variables, which are reported later with
//! the rest of the validation errors.

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use toml_edit::Document;

use super::SectionLocation;
use crate::mise::Mise;

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// The project's mise config files and the one microkitchen reads and edits.
#[derive(Debug, Clone)]
pub struct Discovery {
    /// TOML config files mise loads, lowest precedence first (global ones included).
    pub loaded_files: Vec<PathBuf>,

    /// The highest-precedence project file with a microkitchen table, or the
    /// highest-precedence project file if none has one. Dialog decisions are
    /// written here.
    pub kitchen_file: PathBuf,

    /// The project directory the kitchen file belongs to; mount sources are
    /// relative to it.
    pub kitchen_dir: PathBuf,

    pub section: Option<SectionLocation>,
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// Discover the project containing `cwd`.
pub fn discover(mise: &Mise, cwd: &Path) -> Result<Discovery> {
    // Highest precedence first, as printed by mise.
    let files: Vec<PathBuf> = mise
        .config_files(cwd)?
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "toml"))
        .collect();

    let global_dirs = global_config_dirs();
    let candidates: Vec<(&PathBuf, Option<SectionLocation>)> = files
        .iter()
        .filter(|p| !global_dirs.iter().any(|dir| p.starts_with(dir)))
        .map(|p| (p, section_in(p)))
        .collect();

    let (kitchen_file, section) = select_kitchen(&candidates).ok_or_else(|| {
        anyhow!(
            "no mise.toml found in {} or any parent directory",
            cwd.display()
        )
    })?;
    let kitchen_file = kitchen_file.clone();
    let kitchen_dir = project_dir(&kitchen_file);

    let mut loaded_files = files;
    loaded_files.reverse();

    Ok(Discovery {
        loaded_files,
        kitchen_file,
        kitchen_dir,
        section,
    })
}

/// Pick the kitchen file from project files ordered highest precedence first.
pub fn select_kitchen<P: Copy>(
    candidates: &[(P, Option<SectionLocation>)],
) -> Option<(P, Option<SectionLocation>)> {
    candidates
        .iter()
        .find(|(_, section)| section.is_some())
        .or_else(|| candidates.first())
        .copied()
}

/// The project directory of a config file: `mise.toml`, `.mise.toml`,
/// `.config/mise.toml`, `.config/mise/config.toml`, `mise/config.toml`,
/// `.mise/config.toml` and `conf.d/*.toml` variants all belong to the
/// directory that holds them.
pub fn project_dir(file: &Path) -> PathBuf {
    let mut dir = file.parent().unwrap_or(Path::new("/")).to_path_buf();
    let mut nested = file
        .file_name()
        .is_some_and(|n| n.to_string_lossy().starts_with("config"));
    if dir.file_name().is_some_and(|n| n == "conf.d") {
        dir.pop();
        nested = true;
    }
    if nested && dir.file_name().is_some_and(|n| n == "mise" || n == ".mise") {
        dir.pop();
    }
    if dir.file_name().is_some_and(|n| n == ".config") {
        dir.pop();
    }
    dir
}

/// Which microkitchen table a config file has, if any. Unreadable or invalid
/// files count as having none; mise has already rejected invalid ones.
fn section_in(file: &Path) -> Option<SectionLocation> {
    let text = std::fs::read_to_string(file).ok()?;
    let document = Document::parse(text).ok()?;
    if document
        .get("_")
        .and_then(|u| u.get("microkitchen"))
        .is_some()
    {
        Some(SectionLocation::Underscore)
    } else if document.get("microkitchen").is_some() {
        Some(SectionLocation::Legacy)
    } else {
        None
    }
}

/// Directories holding mise's global and system config; never kitchen files.
fn global_config_dirs() -> Vec<PathBuf> {
    let env = |name: &str| {
        std::env::var_os(name)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    let mut dirs = vec![PathBuf::from("/etc/mise")];
    if let Some(file) = env("MISE_GLOBAL_CONFIG_FILE") {
        dirs.push(file);
    }
    if let Some(file) = env("MISE_SYSTEM_CONFIG_FILE") {
        dirs.push(file);
    }
    if let Some(dir) = env("MISE_CONFIG_DIR") {
        dirs.push(dir);
    }
    if let Some(dir) = env("XDG_CONFIG_HOME") {
        dirs.push(dir.join("mise"));
    }
    if let Some(home) = env("HOME") {
        dirs.push(home.join(".config/mise"));
    }
    dirs
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_dirs() {
        for (file, dir) in [
            ("/p/mise.toml", "/p"),
            ("/p/mise.local.toml", "/p"),
            ("/p/.mise.toml", "/p"),
            ("/p/.config/mise.toml", "/p"),
            ("/p/.config/mise/config.toml", "/p"),
            ("/p/.config/mise/conf.d/extra.toml", "/p"),
            ("/p/mise/config.toml", "/p"),
            ("/p/.mise/config.local.toml", "/p"),
            ("/p/mise/mise.toml", "/p/mise"),
        ] {
            assert_eq!(project_dir(Path::new(file)), Path::new(dir), "{file}");
        }
    }

    #[test]
    fn selects_highest_file_with_section_else_nearest() {
        use SectionLocation::*;
        assert_eq!(
            select_kitchen(&[("a", None), ("b", Some(Legacy)), ("c", Some(Underscore))]),
            Some(("b", Some(Legacy)))
        );
        assert_eq!(
            select_kitchen(&[("a", None), ("b", None)]),
            Some(("a", None))
        );
        assert_eq!(select_kitchen::<&str>(&[]), None);
    }
}
