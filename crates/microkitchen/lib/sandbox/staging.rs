//! Copying the host files a [`StagePlan`] names into the guest.
//!
//! The plan says *which* host paths the kitchen file references and where each
//! lands (see [`crate::config::staging`]); this module expands them against the
//! filesystem, streams them in as a tar archive, and removes what a previous
//! stage left behind.
//!
//! Everything lands under [`GUEST_FILES_DIR`], never beside
//! `/opt/kitchen/mise.toml`, so a stage can never damage mise's system config.
//! Ownership travels in the tar headers rather than a `chown` pass: entries
//! carry chef's numeric uid and gid, and root's extract honours them. Numeric,
//! because chef does not exist as a *name* until `mise bootstrap accounts
//! apply` has run.
//!
//! The archive is built on disk and streamed in, rather than held in memory and
//! sent as one blob. The SDK's `stdin_bytes` puts everything in a single
//! protocol frame, and those are capped at 4 MiB
//! (`microsandbox_protocol::codec::MAX_FRAME_SIZE`) — fine for a `mise.toml`
//! and hopeless for a dotfiles tree.
//!
//! Transferring the archive with `fs().copy_from_host` instead was measured and
//! is just as correct and no faster: the tar is opaque, so both preserve modes,
//! ownership and symlinks exactly. It would still need this same `tar -x` exec
//! to unpack, plus a temporary copy inside the guest to clean up and room for
//! it on the guest's disk, so streaming to `tar -xf -` is the simpler of the
//! two. (Copying a *tree* with the fs API is a different matter: it has no
//! recursion, drops permissions, and silently dereferences symlinks.)

use std::collections::BTreeSet;
use std::fs;
use std::io::{BufReader, BufWriter, Read, Seek, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, bail};
use microsandbox::{ExecEvent, Sandbox};
use sha2::{Digest, Sha256};

use super::build::ROOT;
use crate::config::schema::GuestUser;
use crate::config::staging::{GUEST_FILES_DIR, StagePlan, StagedSource};

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

/// Bytes per `ExecStdin` frame, comfortably under the protocol's 4 MiB cap.
const CHUNK: usize = 1024 * 1024;

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// What a [`StagePlan`] expands to against the host filesystem right now.
pub struct StageArchive {
    /// Guest paths this stage owns, so the next one can remove what is gone.
    pub roots: Vec<String>,
    /// Digest of the contents, for `remodel`'s change detection.
    pub digest: String,
    pub files: usize,
    pub bytes: u64,
    entries: Vec<Entry>,
}

/// One archive member. Paths are relative to [`GUEST_FILES_DIR`], which is the
/// directory the guest extracts into.
struct Entry {
    path: String,
    kind: Kind,
    mode: u32,
    /// Where to read the bytes from, for [`Kind::File`].
    host: PathBuf,
    size: u64,
    /// Content hash, or the link target for [`Kind::Symlink`].
    fingerprint: String,
}

enum Kind {
    File,
    Dir,
    Symlink,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl StageArchive {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Write the archive as an uncompressed tar stream owned by `user`.
    pub fn write_tar(&self, user: GuestUser, out: impl Write) -> Result<()> {
        let mut builder = tar::Builder::new(out);
        builder.follow_symlinks(false);
        for entry in &self.entries {
            let mut header = tar::Header::new_gnu();
            header.set_mode(entry.mode);
            header.set_uid(u64::from(user.uid));
            header.set_gid(u64::from(user.gid));
            // Zeroed so the same sources produce the same archive.
            header.set_mtime(0);
            match entry.kind {
                Kind::Dir => {
                    header.set_entry_type(tar::EntryType::Directory);
                    header.set_size(0);
                    builder
                        .append_data(&mut header, &entry.path, std::io::empty())
                        .with_context(|| format!("adding {} to the archive", entry.path))?;
                }
                Kind::Symlink => {
                    header.set_entry_type(tar::EntryType::Symlink);
                    header.set_size(0);
                    builder
                        .append_link(&mut header, &entry.path, &entry.fingerprint)
                        .with_context(|| format!("adding {} to the archive", entry.path))?;
                }
                Kind::File => {
                    let file = fs::File::open(&entry.host)
                        .with_context(|| format!("reading {}", entry.host.display()))?;
                    header.set_entry_type(tar::EntryType::Regular);
                    header.set_size(entry.size);
                    builder
                        .append_data(&mut header, &entry.path, file)
                        .with_context(|| format!("adding {} to the archive", entry.path))?;
                }
            }
        }
        builder.finish().context("finishing the archive")?;
        Ok(())
    }
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// Expand `plan` against the host filesystem, applying `exclude` and
/// `manifest = "git"` here so filtered files never leave the host.
pub fn collect(plan: &StagePlan) -> Result<StageArchive> {
    let mut entries: Vec<Entry> = Vec::new();
    let mut roots: BTreeSet<String> = BTreeSet::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();

    for source in &plan.sources {
        let relative = source
            .guest
            .strip_prefix(&format!("{GUEST_FILES_DIR}/"))
            .unwrap_or(&source.guest)
            .to_owned();
        roots.insert(source.guest.clone());

        // The source itself may be a symlink the user pointed at deliberately,
        // so resolve this one; links *inside* a staged tree stay links.
        let meta = match fs::metadata(&source.host) {
            Ok(meta) => meta,
            // validate::check reports a missing source at its line; a stage
            // that runs anyway simply leaves it out.
            Err(_) => continue,
        };

        if meta.is_dir() {
            let tracked = if source.git_manifest {
                Some(git_manifest(&source.host)?)
            } else {
                None
            };
            collect_dir(source, &relative, tracked.as_ref(), &mut entries, &mut seen)?;
        } else {
            push(
                &mut entries,
                &mut seen,
                Entry {
                    path: relative,
                    kind: Kind::File,
                    mode: meta.permissions().mode() & 0o7777,
                    size: meta.len(),
                    fingerprint: hash_file(&source.host)?,
                    host: source.host.clone(),
                },
            );
        }
    }

    entries.sort_by(|a, b| a.path.cmp(&b.path));
    let digest = digest(&entries);
    let files = entries
        .iter()
        .filter(|e| matches!(e.kind, Kind::File))
        .count();
    let bytes = entries.iter().map(|e| e.size).sum();

    Ok(StageArchive {
        roots: roots.into_iter().collect(),
        digest,
        files,
        bytes,
        entries,
    })
}

/// Copy `archive` into the guest and delete `stale` paths a previous stage left.
pub async fn apply(
    sandbox: &Sandbox,
    archive: &StageArchive,
    user: GuestUser,
    stale: &[String],
) -> Result<()> {
    let removals: String = stale
        .iter()
        .filter(|path| removable(path))
        .map(|path| format!("rm -rf -- '{path}'\n"))
        .collect();

    if archive.is_empty() && removals.is_empty() {
        return Ok(());
    }

    let script = format!(
        "set -eu\n\
         command -v tar >/dev/null 2>&1 || {{ echo 'microkitchen: tar is missing in the sandbox' >&2; exit 1; }}\n\
         mkdir -p {GUEST_FILES_DIR}\n\
         chown {}:{} {GUEST_FILES_DIR}\n\
         {removals}\
         tar -xf - -C {GUEST_FILES_DIR} --same-owner --no-overwrite-dir\n",
        user.uid, user.gid,
    );

    let mut handle = sandbox
        .shell_stream_with(script.as_str(), |e| e.user(ROOT).stdin_pipe())
        .await
        .context("starting the staging copy in the sandbox")?;
    let sink = handle
        .take_stdin()
        .context("the staging copy has no stdin")?;

    // Built on disk rather than in a `Vec`, so host memory stays flat whatever
    // the tree's size, and streamed in so the guest needs no temporary copy.
    let mut tar = tempfile::NamedTempFile::new().context("creating the staging archive")?;
    archive.write_tar(user, BufWriter::new(tar.as_file_mut()))?;
    tar.as_file_mut()
        .rewind()
        .context("rewinding the staging archive")?;

    let mut reader = BufReader::new(tar.as_file());
    let mut buffer = vec![0u8; CHUNK];
    loop {
        let read = reader
            .read(&mut buffer)
            .context("reading the staging archive")?;
        if read == 0 {
            break;
        }
        sink.write(&buffer[..read])
            .await
            .context("sending the staged files to the sandbox")?;
    }
    sink.close()
        .await
        .context("closing the staging copy's stdin")?;

    let mut stderr = Vec::new();
    let mut exit = None;
    while let Some(event) = handle.recv().await {
        match event {
            ExecEvent::Stderr(bytes) => stderr.extend_from_slice(&bytes),
            ExecEvent::Exited { code } => {
                exit = Some(code);
                break;
            }
            ExecEvent::Failed(failure) => bail!("the staging copy could not start: {failure:?}"),
            _ => {}
        }
    }

    match exit {
        Some(0) => Ok(()),
        Some(code) => bail!(
            "staging files into the sandbox failed with exit code {code}: {}",
            String::from_utf8_lossy(&stderr).trim()
        ),
        None => bail!("the staging copy ended without an exit status"),
    }
}

/// Whether a guest path recorded by an earlier stage may be deleted. These
/// strings come from `state.json`, so they are checked rather than trusted.
fn removable(path: &str) -> bool {
    let prefix = format!("{GUEST_FILES_DIR}/");
    path.starts_with(&prefix)
        && path.len() > prefix.len()
        && !path.split('/').any(|component| component == "..")
}

/// Walk a directory source, honouring `exclude` and git's index.
fn collect_dir(
    source: &StagedSource,
    relative: &str,
    tracked: Option<&BTreeSet<PathBuf>>,
    entries: &mut Vec<Entry>,
    seen: &mut BTreeSet<String>,
) -> Result<()> {
    // A pattern holding `/` is anchored to the source root, so a leading one
    // is only a marker (`/mise.toml`) and must go before matching a relative
    // path. A pattern without `/` matches any single component anywhere.
    let patterns: Vec<(glob::Pattern, bool)> = source
        .exclude
        .iter()
        .filter_map(|p| {
            let anchored = p.contains('/');
            glob::Pattern::new(p.trim_start_matches('/'))
                .ok()
                .map(|pattern| (pattern, anchored))
        })
        .collect();

    let walk = walkdir::WalkDir::new(&source.host)
        .follow_links(false)
        .into_iter()
        .filter_entry(|entry| {
            entry.path() == source.host
                || !excluded(
                    entry
                        .path()
                        .strip_prefix(&source.host)
                        .unwrap_or(entry.path()),
                    &patterns,
                )
        });

    for entry in walk {
        let entry = entry.with_context(|| format!("walking {}", source.host.display()))?;
        let Ok(rest) = entry.path().strip_prefix(&source.host) else {
            continue;
        };
        if rest.as_os_str().is_empty() {
            entries.push(Entry {
                path: relative.to_owned(),
                kind: Kind::Dir,
                mode: 0o755,
                host: entry.path().to_owned(),
                size: 0,
                fingerprint: String::new(),
            });
            continue;
        }
        if let Some(tracked) = tracked
            && !entry.file_type().is_dir()
            && !tracked.contains(rest)
        {
            continue;
        }

        let path = format!("{relative}/{}", rest.to_string_lossy());
        let meta = entry
            .metadata()
            .with_context(|| format!("reading {}", entry.path().display()))?;
        let mode = meta.permissions().mode() & 0o7777;

        if entry.file_type().is_dir() {
            push(
                entries,
                seen,
                Entry {
                    path,
                    kind: Kind::Dir,
                    mode,
                    host: entry.path().to_owned(),
                    size: 0,
                    fingerprint: String::new(),
                },
            );
        } else if entry.file_type().is_symlink() {
            let target = fs::read_link(entry.path())
                .with_context(|| format!("reading the link {}", entry.path().display()))?;
            push(
                entries,
                seen,
                Entry {
                    path,
                    kind: Kind::Symlink,
                    mode,
                    host: entry.path().to_owned(),
                    size: 0,
                    fingerprint: target.to_string_lossy().into_owned(),
                },
            );
        } else if entry.file_type().is_file() {
            push(
                entries,
                seen,
                Entry {
                    path,
                    kind: Kind::File,
                    mode,
                    size: meta.len(),
                    fingerprint: hash_file(entry.path())?,
                    host: entry.path().to_owned(),
                },
            );
        }
        // Sockets, fifos and devices are skipped: nothing sensible to copy.
    }
    Ok(())
}

/// Whether `relative` is excluded. A pattern holding `/` is anchored to the
/// source root; one without matches any single path component, as mise's is.
fn excluded(relative: &Path, patterns: &[(glob::Pattern, bool)]) -> bool {
    if relative.as_os_str().is_empty() {
        return false;
    }
    patterns.iter().any(|(pattern, anchored)| {
        if *anchored {
            pattern.matches_path(relative)
        } else {
            relative
                .components()
                .any(|c| pattern.matches(&c.as_os_str().to_string_lossy()))
        }
    })
}

/// Paths in git's index, relative to `dir`.
fn git_manifest(dir: &Path) -> Result<BTreeSet<PathBuf>> {
    let output = Command::new("git")
        .args(["ls-files", "-z"])
        .current_dir(dir)
        .output()
        .with_context(|| format!("running git ls-files in {}", dir.display()))?;
    if !output.status.success() {
        bail!(
            "`manifest = \"git\"` needs a git repository at {}: {}",
            dir.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output
        .stdout
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| PathBuf::from(String::from_utf8_lossy(s).into_owned()))
        .collect())
}

/// Add an entry unless its guest path is already taken, which happens when two
/// sources overlap.
fn push(entries: &mut Vec<Entry>, seen: &mut BTreeSet<String>, entry: Entry) {
    if seen.insert(entry.path.clone()) {
        entries.push(entry);
    }
}

fn hash_file(path: &Path) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    Ok(hex::encode(Sha256::digest(&bytes)))
}

/// Digest of the staged contents: path, kind, permissions and content of each
/// entry. Host paths, mtimes and ownership are left out — ownership comes from
/// `KitchenConfig::user`, which is hashed with the rest of the configuration.
fn digest(entries: &[Entry]) -> String {
    if entries.is_empty() {
        return String::new();
    }
    let mut hasher = Sha256::new();
    for entry in entries {
        let kind = match entry.kind {
            Kind::File => "f",
            Kind::Dir => "d",
            Kind::Symlink => "l",
        };
        hasher.update(entry.path.as_bytes());
        hasher.update([0]);
        hasher.update(kind.as_bytes());
        hasher.update([0]);
        hasher.update(format!("{:04o}", entry.mode).as_bytes());
        hasher.update([0]);
        hasher.update(entry.fingerprint.as_bytes());
        hasher.update(*b"\n");
    }
    let out = hasher.finalize();
    let bytes: &[u8] = out.as_ref();
    hex::encode(&bytes[..8])
}

//--------------------------------------------------------------------------------------------------
// Tests
//--------------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::os::unix::fs::symlink;

    use super::*;
    use crate::config::staging::StagedSource;

    const USER: GuestUser = GuestUser {
        uid: 1001,
        gid: 1001,
    };

    fn source(host: &Path, guest: &str) -> StagedSource {
        StagedSource {
            host: host.to_owned(),
            guest: guest.to_owned(),
            key: "dotfiles.\"~/.x\"".to_owned(),
            span: None,
            exclude: Vec::new(),
            git_manifest: false,
            outside: false,
            dotfiles_root: false,
        }
    }

    /// A tree with a 0600 file, an executable, a nested dir and a symlink.
    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("dots");
        fs::create_dir_all(root.join("nvim")).unwrap();
        fs::write(root.join("gitconfig"), "[user]\n").unwrap();
        fs::set_permissions(root.join("gitconfig"), fs::Permissions::from_mode(0o600)).unwrap();
        fs::write(root.join("setup.sh"), "#!/bin/sh\n").unwrap();
        fs::set_permissions(root.join("setup.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(root.join("nvim/init.lua"), "-- lua\n").unwrap();
        fs::write(root.join("notes.md"), "# notes\n").unwrap();
        symlink("init.lua", root.join("nvim/link.lua")).unwrap();
        dir
    }

    fn plan_of(sources: Vec<StagedSource>) -> StagePlan {
        StagePlan {
            sources,
            rewrites: Vec::new(),
        }
    }

    /// Members of the written archive: path to (mode, uid, gid, link target).
    fn members(archive: &StageArchive) -> BTreeMap<String, (u32, u64, u64, Option<String>)> {
        let mut buffer = Vec::new();
        archive.write_tar(USER, &mut buffer).unwrap();
        let mut out = BTreeMap::new();
        let mut reader = tar::Archive::new(buffer.as_slice());
        for entry in reader.entries().unwrap() {
            let entry = entry.unwrap();
            let header = entry.header();
            let link = entry
                .link_name()
                .unwrap()
                .map(|p| p.to_string_lossy().into_owned());
            out.insert(
                entry.path().unwrap().to_string_lossy().into_owned(),
                (
                    header.mode().unwrap(),
                    header.uid().unwrap(),
                    header.gid().unwrap(),
                    link,
                ),
            );
        }
        out
    }

    #[test]
    fn archive_preserves_modes_symlinks_and_ownership() {
        let dir = tree();
        let plan = plan_of(vec![source(
            &dir.path().join("dots"),
            "/opt/kitchen/files/project/dots",
        )]);
        let archive = collect(&plan).unwrap();
        let members = members(&archive);

        // Paths are relative to the extraction directory and never escape it.
        assert!(
            members
                .keys()
                .all(|p| !p.starts_with('/') && !p.contains("..")),
            "{:?}",
            members.keys().collect::<Vec<_>>()
        );

        let file = &members["project/dots/gitconfig"];
        assert_eq!(file.0, 0o600, "mode");
        assert_eq!((file.1, file.2), (1001, 1001), "ownership");
        assert_eq!(members["project/dots/setup.sh"].0, 0o755);
        assert_eq!(
            members["project/dots/nvim/link.lua"].3.as_deref(),
            Some("init.lua"),
            "symlinks stay symlinks"
        );
        assert_eq!(archive.files, 4);
    }

    #[test]
    fn excluded_paths_never_leave_the_host() {
        let dir = tree();
        let mut source = source(&dir.path().join("dots"), "/opt/kitchen/files/project/dots");
        source.exclude = vec!["*.md".to_owned(), "nvim".to_owned()];
        let archive = collect(&plan_of(vec![source])).unwrap();
        let members = members(&archive);

        assert!(members.contains_key("project/dots/gitconfig"));
        assert!(
            !members.contains_key("project/dots/notes.md"),
            "{members:?}"
        );
        // Excluding a directory excludes everything beneath it.
        assert!(
            !members.keys().any(|p| p.contains("nvim")),
            "{:?}",
            members.keys().collect::<Vec<_>>()
        );
    }

    #[test]
    fn anchored_excludes_only_match_at_the_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("dots");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("mise.toml"), "a\n").unwrap();
        fs::write(root.join("sub/mise.toml"), "b\n").unwrap();

        let mut anchored = source(&root, "/opt/kitchen/files/project/dots");
        anchored.exclude = vec!["/mise.toml".to_owned()];
        let kept = members(&collect(&plan_of(vec![anchored])).unwrap());
        assert!(kept.contains_key("project/dots/sub/mise.toml"), "{kept:?}");
        assert!(!kept.contains_key("project/dots/mise.toml"), "{kept:?}");

        let mut bare = source(&root, "/opt/kitchen/files/project/dots");
        bare.exclude = vec!["mise.toml".to_owned()];
        let dropped = members(&collect(&plan_of(vec![bare])).unwrap());
        assert!(
            !dropped.contains_key("project/dots/sub/mise.toml"),
            "{dropped:?}"
        );
    }

    #[test]
    fn a_single_file_source_is_archived_alone() {
        let dir = tree();
        let plan = plan_of(vec![source(
            &dir.path().join("dots/gitconfig"),
            "/opt/kitchen/files/ab12cd34/gitconfig",
        )]);
        let archive = collect(&plan).unwrap();
        let members = members(&archive);
        assert_eq!(members.len(), 1);
        assert!(members.contains_key("ab12cd34/gitconfig"), "{members:?}");
    }

    #[test]
    fn digest_tracks_contents_and_modes_but_not_time() {
        let dir = tree();
        let host = dir.path().join("dots");
        let plan = plan_of(vec![source(&host, "/opt/kitchen/files/project/dots")]);

        let before = collect(&plan).unwrap().digest;
        assert_eq!(before.len(), 16);
        // Re-collecting the untouched tree gives the same digest.
        assert_eq!(collect(&plan).unwrap().digest, before);

        fs::write(host.join("gitconfig"), "[user]\n\tname = t\n").unwrap();
        let after_content = collect(&plan).unwrap().digest;
        assert_ne!(after_content, before, "contents");

        fs::set_permissions(host.join("gitconfig"), fs::Permissions::from_mode(0o644)).unwrap();
        assert_ne!(collect(&plan).unwrap().digest, after_content, "mode");
    }

    #[test]
    fn an_empty_plan_has_an_empty_digest() {
        let archive = collect(&StagePlan::default()).unwrap();
        assert!(archive.is_empty());
        assert_eq!(archive.digest, "");
        assert_eq!(archive.roots, Vec::<String>::new());
    }

    #[test]
    fn git_manifest_limits_the_archive() {
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let dir = tree();
        let root = dir.path().join("dots");
        let git = |args: &[&str]| {
            Command::new("git")
                .args(args)
                .current_dir(&root)
                .output()
                .unwrap()
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@example.com"]);
        git(&["config", "user.name", "t"]);
        git(&["add", "gitconfig"]);

        let mut source = source(&root, "/opt/kitchen/files/project/dots");
        source.git_manifest = true;
        let members = members(&collect(&plan_of(vec![source])).unwrap());
        assert!(
            members.contains_key("project/dots/gitconfig"),
            "{members:?}"
        );
        assert!(
            !members.contains_key("project/dots/notes.md"),
            "{members:?}"
        );
    }

    #[test]
    fn removals_stay_inside_the_staged_area() {
        for allowed in [
            "/opt/kitchen/files/project/dots",
            "/opt/kitchen/files/ab12cd34/gitconfig",
        ] {
            assert!(removable(allowed), "{allowed}");
        }
        for refused in [
            "/opt/kitchen/files",
            "/opt/kitchen/files/",
            "/opt/kitchen/mise.toml",
            "/opt/kitchen",
            "/etc/passwd",
            "/opt/kitchen/files/../../etc",
            "",
        ] {
            assert!(!removable(refused), "{refused}");
        }
    }
}
