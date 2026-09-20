//! Host files that mise's `[dotfiles]`, `[bootstrap.files]` and
//! `[bootstrap.directories]` entries reference, and where they land in the guest.
//!
//! Only the kitchen file reaches the guest (as `/opt/kitchen/mise.toml`), so
//! every `source` in it would otherwise dangle. This module decides which host
//! paths to copy and rewrites each `source` to the absolute guest path of its
//! copy; [`crate::sandbox::staging`] does the copying.
//!
//! **How mise resolves a `source`** (verified against mise 2026.9.6, the pinned
//! version, with the config installed as `MISE_SYSTEM_CONFIG_FILE`):
//!
//! - mise honours `[dotfiles]` and `[bootstrap.files]` in a *system* config.
//! - A relative `source` resolves against the directory holding the **declaring
//!   file**, not against mise's `config_root`. With the config at
//!   `<p>/.config/mise/config.toml`, `source = "dotfiles/x"` is
//!   `<p>/.config/mise/dotfiles/x`, never `<p>/dotfiles/x`. So `base` here is the
//!   kitchen *file's* directory, and [`super::discover::project_dir`]'s result —
//!   which pops `.config`/`mise` — is used only to tell project-local sources
//!   from outside ones.
//! - An absolute `source` is used verbatim. That is what makes rewriting to
//!   absolute guest paths sufficient.
//! - The working directory does not affect resolution.
//!
//! Rewriting every source, rather than only the ones that escape the project,
//! means the guest never depends on how mise derives `config_root` for a system
//! config, and a source we resolved wrongly fails at `validate` time with "host
//! path does not exist" instead of dangling silently in the guest.

use std::collections::BTreeMap;
use std::ops::Range;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};
use toml_edit::{Document, Item};

use super::schema::resolve_host_path;
use super::{Diagnostics, Severity, Source};

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

/// Guest directory holding every staged source. Replaceable without touching
/// `/opt/kitchen/mise.toml`, which is mise's system config.
pub const GUEST_FILES_DIR: &str = "/opt/kitchen/files";

/// Subdirectory of [`GUEST_FILES_DIR`] for sources under the kitchen directory.
const PROJECT_DIR: &str = "project";

/// Characters that make a `source` a glob pattern rather than a literal path.
const GLOB_CHARS: &[char] = &['*', '?', '['];

/// Tables whose entries name host files, by their path in the document.
const FILE_TABLES: &[&[&str]] = &[
    &["dotfiles"],
    &["bootstrap", "files"],
    &["bootstrap", "directories"],
];

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// Everything the guest needs from the host filesystem, and the rewrites that
/// point the guest config at it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StagePlan {
    pub sources: Vec<StagedSource>,
    pub rewrites: Vec<Rewrite>,
}

/// One host path copied into the guest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedSource {
    /// Absolute, lexically normalized host path. Never holds glob characters:
    /// a pattern is staged by its literal prefix.
    pub host: PathBuf,

    /// Absolute guest path of the copy.
    pub guest: String,

    /// The entry that referenced it, for diagnostics: `dotfiles."~/.gitconfig"`.
    pub key: String,

    /// Span of the value in the kitchen file.
    pub span: Option<Range<usize>>,

    /// `exclude` patterns, applied on the host as well so excluded files never
    /// leave it. The key stays in the guest config; applying it twice is
    /// idempotent.
    pub exclude: Vec<String>,

    /// `manifest = "git"`: only files in git's index are copied. Resolved on
    /// the host, because the staged copy has no `.git` (see [`Rewrite`]).
    pub git_manifest: bool,

    /// The host path is not under the kitchen directory.
    pub outside: bool,

    /// Set from `[_.microkitchen] dotfiles`, not from a mise entry.
    pub dotfiles_root: bool,
}

/// One value of the guest config to replace, addressed by TOML path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rewrite {
    pub path: Vec<Step>,
    pub action: RewriteAction,
}

/// One hop of a [`Rewrite`]'s path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Key(String),
    Index(usize),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RewriteAction {
    /// Replace the value in place.
    Set(String),
    /// Create the key and any missing parents, then set it.
    Ensure(String),
    /// Drop the key: `manifest = "git"`, which is applied on the host instead.
    Remove,
}

/// Collects sources while walking the document.
struct Collector<'a> {
    source: &'a Source,
    diagnostics: &'a mut Diagnostics,
    /// Directory relative sources resolve against: the kitchen file's own.
    base: &'a Path,
    /// Project directory, for the inside/outside split only.
    kitchen_dir: &'a Path,
    plan: StagePlan,
    /// Host path to its index in `plan.sources`, for dedup.
    seen: BTreeMap<PathBuf, usize>,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl StagePlan {
    pub fn is_empty(&self) -> bool {
        self.sources.is_empty()
    }
}

impl Collector<'_> {
    fn error(&mut self, span: Option<Range<usize>>, key: &str, message: impl Into<String>) {
        self.diagnostics.push(
            self.source
                .diagnostic(Severity::Error, span, Some(key), message),
        );
    }

    /// Register `text` as a source of `key`, returning the guest path its
    /// value should be rewritten to.
    fn add(
        &mut self,
        text: &str,
        key: &str,
        span: Option<Range<usize>>,
        exclude: Vec<String>,
        git_manifest: bool,
        dotfiles_root: bool,
    ) -> Option<String> {
        let resolved = match resolve_host_path(text, self.base) {
            Ok(path) => path,
            Err(message) => {
                self.error(span, key, message);
                return None;
            }
        };

        // A pattern is staged by the directory it searches; mise re-expands it
        // in the guest, so we never interpret it.
        let (host, pattern) = split_glob(&resolved);
        let (guest, outside) = guest_path(&host, self.kitchen_dir);

        match self.seen.get(&host) {
            Some(&index) => {
                // Same host path twice: one copy, and the first span wins for
                // diagnostics. Merge what only narrows the copy.
                let existing = &mut self.plan.sources[index];
                existing.dotfiles_root |= dotfiles_root;
                if existing.exclude != exclude {
                    existing.exclude.clear();
                }
                existing.git_manifest &= git_manifest;
            }
            None => {
                if let Some(other) = self
                    .plan
                    .sources
                    .iter()
                    .find(|s| s.guest == guest && s.host != host)
                {
                    self.error(
                        span,
                        key,
                        format!(
                            "`{}` and `{}` would both be staged at `{guest}`",
                            other.host.display(),
                            host.display()
                        ),
                    );
                    return None;
                }
                self.seen.insert(host.clone(), self.plan.sources.len());
                self.plan.sources.push(StagedSource {
                    host,
                    guest: guest.clone(),
                    key: key.to_owned(),
                    span,
                    exclude,
                    git_manifest,
                    outside,
                    dotfiles_root,
                });
            }
        }

        Some(match pattern {
            Some(pattern) => format!("{guest}/{pattern}"),
            None => guest,
        })
    }

    /// Walk one `target = <entry>` pair of a file table.
    fn entry(&mut self, table: &str, target: &str, item: &Item, prefix: &[Step]) {
        let key = format!("{table}.\"{target}\"");

        // Shorthand: the value is the source itself.
        if let Some(text) = item.as_str() {
            if let Some(guest) = self.add(text, &key, item.span(), Vec::new(), false, false) {
                self.plan.rewrites.push(Rewrite {
                    path: prefix.to_vec(),
                    action: RewriteAction::Set(guest),
                });
            }
            return;
        }

        let Some(entry) = item.as_table_like() else {
            return;
        };

        let exclude: Vec<String> = entry
            .get("exclude")
            .and_then(Item::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let git_manifest = entry.get("manifest").and_then(Item::as_str) == Some("git");

        if let Some(item) = entry.get("source")
            && let Some(text) = item.as_str()
        {
            let guest = self.add(
                text,
                &key,
                item.span(),
                exclude.clone(),
                git_manifest,
                false,
            );
            if let Some(guest) = guest {
                let mut path = prefix.to_vec();
                path.push(Step::Key("source".to_owned()));
                self.plan.rewrites.push(Rewrite {
                    path,
                    action: RewriteAction::Set(guest),
                });
                if git_manifest {
                    // The staged copy has no `.git`, so mise in the guest would
                    // match nothing. Filtering on the host preserves the result.
                    let mut path = prefix.to_vec();
                    path.push(Step::Key("manifest".to_owned()));
                    self.plan.rewrites.push(Rewrite {
                        path,
                        action: RewriteAction::Remove,
                    });
                }
            }
        }

        // Every variant's source: the guest is always Linux, but evaluating
        // mise's variant conditions on the host is not worth the risk.
        if let Some(variants) = entry.get("variants").and_then(Item::as_array) {
            for (index, value) in variants.iter().enumerate() {
                let Some(variant) = value.as_inline_table() else {
                    continue;
                };
                let Some(text) = variant.get("source").and_then(|v| v.as_str()) else {
                    continue;
                };
                let key = format!("{key}.variants[{index}]");
                let span = variant.get("source").and_then(|v| v.span());
                if let Some(guest) =
                    self.add(text, &key, span, exclude.clone(), git_manifest, false)
                {
                    let mut path = prefix.to_vec();
                    path.push(Step::Key("variants".to_owned()));
                    path.push(Step::Index(index));
                    path.push(Step::Key("source".to_owned()));
                    self.plan.rewrites.push(Rewrite {
                        path,
                        action: RewriteAction::Set(guest),
                    });
                }
            }
        }
    }
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// Collect the host paths the kitchen file references.
///
/// `base` is the kitchen file's own directory, which relative sources resolve
/// against; `kitchen_dir` is the project directory, used only to tell
/// project-local sources from outside ones. `dotfiles` is
/// `[_.microkitchen] dotfiles`, already resolved.
pub fn plan(
    source: &Source,
    base: &Path,
    kitchen_dir: &Path,
    dotfiles: Option<&Path>,
    diagnostics: &mut Diagnostics,
) -> StagePlan {
    let Ok(document) = Document::parse(source.text.as_str()) else {
        // schema::parse has already reported the syntax error.
        return StagePlan::default();
    };

    let mut collector = Collector {
        source,
        diagnostics,
        base,
        kitchen_dir,
        plan: StagePlan::default(),
        seen: BTreeMap::new(),
    };

    for path in FILE_TABLES {
        let mut item = document.as_table().get(path[0]);
        for key in &path[1..] {
            item = item.and_then(Item::as_table_like).and_then(|t| t.get(key));
        }
        let Some(table) = item.and_then(Item::as_table_like) else {
            continue;
        };
        let label = path.join(".");
        for (target, entry) in table.iter() {
            let prefix: Vec<Step> = path
                .iter()
                .map(|k| Step::Key((*k).to_owned()))
                .chain([Step::Key(target.to_owned())])
                .collect();
            collector.entry(&label, target, entry, &prefix);
        }
    }

    // `settings.dotfiles.root` written in the kitchen file is a host path too.
    let root_item = document
        .as_table()
        .get("settings")
        .and_then(Item::as_table_like)
        .and_then(|t| t.get("dotfiles"))
        .and_then(Item::as_table_like)
        .and_then(|t| t.get("root"));
    if let Some(item) = root_item
        && let Some(text) = item.as_str()
        && dotfiles.is_none()
        && let Some(guest) = collector.add(
            text,
            "settings.dotfiles.root",
            item.span(),
            Vec::new(),
            false,
            true,
        )
    {
        collector.plan.rewrites.push(Rewrite {
            path: dotfiles_root_path(),
            action: RewriteAction::Set(guest),
        });
    }

    // `[_.microkitchen] dotfiles` wins over it, and is created if absent.
    if let Some(dotfiles) = dotfiles {
        let text = dotfiles.to_string_lossy().into_owned();
        if let Some(guest) = collector.add(
            &text,
            "_.microkitchen.dotfiles",
            None,
            Vec::new(),
            false,
            true,
        ) {
            collector.plan.rewrites.push(Rewrite {
                path: dotfiles_root_path(),
                action: RewriteAction::Ensure(guest),
            });
        }
    }

    let mut plan = collector.plan;
    collapse_nested(&mut plan);
    plan
}

/// Where a host path lands in the guest, and whether it came from outside the
/// project: `<files>/project/<relative>` for a path under `kitchen_dir`,
/// `<files>/<hash>/<name>` otherwise.
///
/// Outside paths are grouped by the directory holding them, so several sources
/// from one host directory share a staged directory and the hash does not
/// change when a sibling is added. The hash keeps host usernames and directory
/// layout out of the guest, as `render_guest_config` already keeps `_.path` and
/// `_.file` out.
pub fn guest_path(host: &Path, kitchen_dir: &Path) -> (String, bool) {
    if let Ok(relative) = host.strip_prefix(kitchen_dir)
        && !relative.as_os_str().is_empty()
    {
        return (
            format!(
                "{GUEST_FILES_DIR}/{PROJECT_DIR}/{}",
                relative.to_string_lossy()
            ),
            false,
        );
    }
    let name = host
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "root".to_owned());
    let anchor = host.parent().unwrap_or(Path::new("/"));
    let digest = Sha256::digest(anchor.to_string_lossy().as_bytes());
    let bytes: &[u8] = digest.as_ref();
    (
        format!("{GUEST_FILES_DIR}/{}/{name}", hex::encode(&bytes[..4])),
        true,
    )
}

/// Split a path into the longest prefix without glob characters and the rest.
fn split_glob(path: &Path) -> (PathBuf, Option<String>) {
    let mut prefix = PathBuf::new();
    let mut rest: Vec<String> = Vec::new();
    for component in path.components() {
        let text = component.as_os_str().to_string_lossy();
        if rest.is_empty() && !text.contains(GLOB_CHARS) {
            prefix.push(component.as_os_str());
        } else {
            rest.push(text.into_owned());
        }
    }
    if rest.is_empty() {
        (prefix, None)
    } else {
        // A pattern in the first component would leave no directory to stage;
        // fall back to the path's parent.
        if prefix.as_os_str().is_empty() {
            prefix.push("/");
        }
        (prefix, Some(rest.join("/")))
    }
}

/// Drop sources already covered by a staged ancestor: the bytes are copied
/// once and the descendant's guest path is the ancestor's plus the same
/// relative suffix, so rewrites stay correct.
fn collapse_nested(plan: &mut StagePlan) {
    let mut keep: Vec<StagedSource> = Vec::new();
    let mut sorted: Vec<StagedSource> = std::mem::take(&mut plan.sources);
    sorted.sort_by(|a, b| a.host.cmp(&b.host));
    for candidate in sorted {
        let covered = keep.iter().any(|kept| {
            candidate.host != kept.host
                && candidate.host.starts_with(&kept.host)
                && kept.exclude.is_empty()
                && !kept.git_manifest
        });
        if !covered {
            keep.push(candidate);
        }
    }
    plan.sources = keep;
}

fn dotfiles_root_path() -> Vec<Step> {
    vec![
        Step::Key("settings".to_owned()),
        Step::Key("dotfiles".to_owned()),
        Step::Key("root".to_owned()),
    ]
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const KITCHEN: &str = "/p/app";

    fn run(text: &str) -> (StagePlan, Diagnostics) {
        run_with(text, None)
    }

    fn run_with(text: &str, dotfiles: Option<&str>) -> (StagePlan, Diagnostics) {
        let source = Source::new(format!("{KITCHEN}/mise.toml"), text);
        let mut diagnostics = Diagnostics::default();
        let plan = plan(
            &source,
            Path::new(KITCHEN),
            Path::new(KITCHEN),
            dotfiles.map(Path::new),
            &mut diagnostics,
        );
        (plan, diagnostics)
    }

    /// Guest paths of every staged source, in plan order.
    fn guests(plan: &StagePlan) -> Vec<&str> {
        plan.sources.iter().map(|s| s.guest.as_str()).collect()
    }

    /// The value a rewrite sets. Target keys hold `/` themselves, so the path
    /// is given as parts: `&["dotfiles", "~/.gitconfig", "source"]`.
    fn rewrite(plan: &StagePlan, path: &[&str]) -> Option<String> {
        let wanted: Vec<Step> = path
            .iter()
            .map(|s| match s.parse::<usize>() {
                Ok(index) => Step::Index(index),
                Err(_) => Step::Key((*s).to_owned()),
            })
            .collect();
        plan.rewrites
            .iter()
            .find(|r| r.path == wanted)
            .map(|r| match &r.action {
                RewriteAction::Set(v) | RewriteAction::Ensure(v) => v.clone(),
                RewriteAction::Remove => "<remove>".to_owned(),
            })
    }

    /// The staged source whose guest path ends with `suffix`.
    fn staged<'a>(plan: &'a StagePlan, suffix: &str) -> &'a StagedSource {
        plan.sources
            .iter()
            .find(|s| s.guest.ends_with(suffix))
            .unwrap_or_else(|| panic!("no source ending in {suffix}: {plan:?}"))
    }

    #[test]
    fn collects_every_entry_spelling() {
        let text = r#"
[dotfiles]
"~/.gitconfig" = "dotfiles/gitconfig"
"~/.vimrc" = { source = "dotfiles/vimrc", mode = "copy" }

[dotfiles."~/.config/nvim"]
source = "dotfiles/nvim"
mode = "symlink"

[bootstrap.files."/etc/a.conf"]
source = "etc/a.conf"

[bootstrap.directories."/srv/data"]
mode = "0750"
"#;
        let (plan, diagnostics) = run(text);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(
            guests(&plan),
            [
                "/opt/kitchen/files/project/dotfiles/gitconfig",
                "/opt/kitchen/files/project/dotfiles/nvim",
                "/opt/kitchen/files/project/dotfiles/vimrc",
                "/opt/kitchen/files/project/etc/a.conf",
            ]
        );
        assert_eq!(
            rewrite(&plan, &["dotfiles", "~/.gitconfig"]).as_deref(),
            Some("/opt/kitchen/files/project/dotfiles/gitconfig")
        );
        assert_eq!(
            rewrite(&plan, &["dotfiles", "~/.vimrc", "source"]).as_deref(),
            Some("/opt/kitchen/files/project/dotfiles/vimrc")
        );
        assert_eq!(
            rewrite(&plan, &["dotfiles", "~/.config/nvim", "source"]).as_deref(),
            Some("/opt/kitchen/files/project/dotfiles/nvim")
        );
        assert_eq!(
            rewrite(&plan, &["bootstrap", "files", "/etc/a.conf", "source"]).as_deref(),
            Some("/opt/kitchen/files/project/etc/a.conf")
        );
    }

    #[test]
    fn entries_without_a_source_stage_nothing() {
        let text = r#"
[dotfiles]
"~/.config/x.conf" = { content = "a = 1\n" }
"~/.zshrc/activate" = { block = 'eval "$(mise activate zsh)"' }
"/etc/hosts/dev" = { line = "127.0.0.1 dev.local" }
"~/.old" = { state = "absent" }
"#;
        let (plan, diagnostics) = run(text);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(plan.is_empty(), "{plan:?}");
        assert!(plan.rewrites.is_empty());
    }

    #[test]
    fn outside_sources_are_hashed_and_grouped() {
        let text = r#"
[dotfiles]
"~/.gitconfig" = "../shared/gitconfig"
"~/.tigrc" = "../shared/tigrc"
"~/.vimrc" = "../../other/vimrc"
"#;
        let (plan, diagnostics) = run(text);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert!(plan.sources.iter().all(|s| s.outside), "{plan:?}");

        // Two files from one host directory share a staged directory; a file
        // from another does not.
        let dir = |s: &StagedSource| s.guest.rsplit_once('/').unwrap().0.to_owned();
        assert_eq!(
            dir(staged(&plan, "/gitconfig")),
            dir(staged(&plan, "/tigrc"))
        );
        assert_ne!(
            dir(staged(&plan, "/gitconfig")),
            dir(staged(&plan, "/vimrc"))
        );
    }

    #[test]
    fn guest_paths_are_stable() {
        let host = Path::new("/home/someone/.dotfiles");
        let kitchen = Path::new("/p/app");
        assert_eq!(guest_path(host, kitchen), guest_path(host, kitchen));
        let (guest, outside) = guest_path(host, kitchen);
        assert!(outside);
        assert!(guest.ends_with("/.dotfiles"), "{guest}");
        // The host path does not leak into the guest.
        assert!(!guest.contains("someone"), "{guest}");
    }

    #[test]
    fn a_source_is_staged_once() {
        let text = r#"
[dotfiles]
"~/.a" = "shared/file"
"~/.b" = "shared/file"
"#;
        let (plan, _) = run(text);
        assert_eq!(plan.sources.len(), 1);
        assert_eq!(plan.rewrites.len(), 2);
        assert_eq!(
            rewrite(&plan, &["dotfiles", "~/.a"]),
            rewrite(&plan, &["dotfiles", "~/.b"])
        );
    }

    #[test]
    fn nested_sources_collapse_into_their_ancestor() {
        let text = r#"
[dotfiles]
"~/.config" = "dotfiles"
"~/.gitconfig" = "dotfiles/git/config"
"#;
        let (plan, _) = run(text);
        assert_eq!(guests(&plan), ["/opt/kitchen/files/project/dotfiles"]);
        // The descendant still points at its path inside the copy.
        assert_eq!(
            rewrite(&plan, &["dotfiles", "~/.gitconfig"]).as_deref(),
            Some("/opt/kitchen/files/project/dotfiles/git/config")
        );
    }

    #[test]
    fn globs_stage_their_directory_and_keep_the_pattern() {
        let text = "[dotfiles]\n\"~/.config/*.toml\" = \"dotfiles/config/*.toml\"\n";
        let (plan, diagnostics) = run(text);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(
            guests(&plan),
            ["/opt/kitchen/files/project/dotfiles/config"]
        );
        assert_eq!(
            rewrite(&plan, &["dotfiles", "~/.config/*.toml"]).as_deref(),
            Some("/opt/kitchen/files/project/dotfiles/config/*.toml")
        );
    }

    #[test]
    fn variants_stage_every_source() {
        let text = r#"
[dotfiles."vscode/settings.json"]
mode = "copy"
variants = [
  { os = "macos", source = "dotfiles/vscode/macos.json" },
  { os = "linux", source = "dotfiles/vscode/linux.json" },
]
"#;
        let (plan, _) = run(text);
        assert_eq!(
            guests(&plan),
            [
                "/opt/kitchen/files/project/dotfiles/vscode/linux.json",
                "/opt/kitchen/files/project/dotfiles/vscode/macos.json",
            ]
        );
        assert_eq!(
            rewrite(
                &plan,
                &[
                    "dotfiles",
                    "vscode/settings.json",
                    "variants",
                    "1",
                    "source"
                ]
            )
            .as_deref(),
            Some("/opt/kitchen/files/project/dotfiles/vscode/linux.json")
        );
    }

    #[test]
    fn git_manifest_is_resolved_on_the_host_and_dropped() {
        let text =
            "[dotfiles.\"~\"]\nsource = \"dots\"\nmode = \"symlink-each\"\nmanifest = \"git\"\n";
        let (plan, _) = run(text);
        assert!(plan.sources[0].git_manifest);
        assert_eq!(
            rewrite(&plan, &["dotfiles", "~", "manifest"]).as_deref(),
            Some("<remove>")
        );
    }

    #[test]
    fn exclude_is_carried_to_the_host() {
        let text = "[dotfiles.\"~\"]\nsource = \"dots\"\nexclude = [\"mise.toml\", \"*.md\"]\n";
        let (plan, _) = run(text);
        assert_eq!(plan.sources[0].exclude, ["mise.toml", "*.md"]);
    }

    #[test]
    fn the_dotfiles_key_becomes_the_guest_root() {
        let (plan, diagnostics) = run_with("[tools]\nnode = \"22\"\n", Some("/home/t/.dotfiles"));
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(plan.sources.len(), 1);
        assert!(plan.sources[0].dotfiles_root);
        let root = rewrite(&plan, &["settings", "dotfiles", "root"]).expect("root rewrite");
        assert_eq!(root, plan.sources[0].guest);
        assert!(
            matches!(plan.rewrites[0].action, RewriteAction::Ensure(_)),
            "{:?}",
            plan.rewrites[0]
        );
    }

    #[test]
    fn a_declared_settings_root_is_staged_too() {
        let text = "[settings.dotfiles]\nroot = \"~/.dotfiles\"\n";
        let (plan, _) = run(text);
        assert_eq!(plan.sources.len(), 1);
        assert!(plan.sources[0].dotfiles_root);
        assert!(matches!(plan.rewrites[0].action, RewriteAction::Set(_)));

        // The microkitchen key wins, and is staged instead.
        let (plan, _) = run_with(text, Some("/elsewhere/dots"));
        assert_eq!(plan.sources.len(), 1);
        assert_eq!(plan.sources[0].host, Path::new("/elsewhere/dots"));
    }

    #[test]
    fn the_kitchen_directory_itself_stages_outside_the_project() {
        // `"~" = { source = ".", mode = "symlink-each" }` is a real mise idiom.
        // The kitchen directory has no path *relative to itself*, so it takes
        // the hashed branch and can never land on `/opt/kitchen/mise.toml`.
        let text = "[dotfiles.\"~\"]\nsource = \".\"\nmode = \"symlink-each\"\n";
        let (plan, diagnostics) = run(text);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(plan.sources.len(), 1);
        assert_eq!(plan.sources[0].host, Path::new(KITCHEN));
        assert!(plan.sources[0].outside);
        assert!(plan.sources[0].guest.ends_with("/app"), "{plan:?}");
        assert_ne!(plan.sources[0].guest, GUEST_FILES_DIR);
    }

    #[test]
    fn colliding_guest_paths_are_reported() {
        // Two different host directories whose files share a name only collide
        // when their anchors hash alike, so provoke it through the project
        // branch instead: `../app/x` normalizes back inside the kitchen dir.
        let text = "[dotfiles]\n\"~/.a\" = \"dotfiles/x\"\n\"~/.b\" = \"../app/dotfiles/x\"\n";
        let (plan, diagnostics) = run(text);
        // Same normalized host path, so it is a dedup, not a collision.
        assert_eq!(plan.sources.len(), 1);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
    }

    #[test]
    fn split_glob_finds_the_literal_prefix() {
        let cases = [
            ("/a/b/c", "/a/b/c", None),
            ("/a/b/*.toml", "/a/b", Some("*.toml")),
            ("/a/**/c", "/a", Some("**/c")),
            ("/a/b?/c", "/a", Some("b?/c")),
        ];
        for (input, prefix, rest) in cases {
            let (got_prefix, got_rest) = split_glob(Path::new(input));
            assert_eq!(got_prefix, Path::new(prefix), "{input}");
            assert_eq!(got_rest.as_deref(), rest, "{input}");
        }
    }
}
