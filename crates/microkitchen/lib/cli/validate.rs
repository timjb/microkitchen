//! `microkitchen validate`: check the configuration and resolve the
//! environment without touching any sandbox. Values are never printed.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::Result;
use serde::Serialize;

use super::{Context, print_diagnostics};
use crate::config::schema::KitchenConfig;
use crate::config::size::format_mib;
use crate::config::staging::GUEST_FILES_DIR;
use crate::config::{Diagnostics, Project, SectionLocation};
use crate::sandbox::naming::sandbox_name;

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

#[derive(Serialize)]
struct Report<'a> {
    valid: bool,
    kitchen_file: &'a Path,
    kitchen_dir: &'a Path,
    section: Option<SectionLocation>,
    loaded_files: &'a [PathBuf],
    sandbox_name: String,
    config: Option<&'a KitchenConfig>,
    /// Forwarded as plain environment variables.
    env: Vec<&'a str>,
    /// Forwarded as microsandbox secrets.
    secrets: Vec<&'a str>,
    /// Declared secrets that resolve to nothing and are left out.
    skipped_secrets: Vec<&'a str>,
    /// Host files copied into the sandbox, and where they land.
    staged: Vec<StagedEntry<'a>>,
    diagnostics: &'a Diagnostics,
}

/// One row of the staging manifest.
#[derive(Serialize)]
struct StagedEntry<'a> {
    /// The entry that referenced it: `dotfiles."~/.gitconfig"`.
    key: &'a str,
    host: &'a Path,
    guest: &'a str,
    /// The host path is not under the kitchen file's directory.
    outside: bool,
    /// Staged as mise's `dotfiles.root`, not by a `source`.
    dotfiles_root: bool,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl<'a> Report<'a> {
    fn new(project: &'a Project) -> Self {
        let discovery = &project.discovery;
        let config = project.kitchen.as_ref().map(|k| &k.config);
        let is_secret = |name: &str| config.is_some_and(|c| c.secrets.contains_key(name));

        let (mut env, mut secrets) = (Vec::new(), Vec::new());
        let mut skipped_secrets = Vec::new();
        if let Some(resolved) = &project.env {
            for name in resolved.values.keys() {
                if is_secret(name) {
                    secrets.push(name.as_str());
                } else {
                    env.push(name.as_str());
                }
            }
            if let Some(config) = config {
                skipped_secrets = config
                    .secrets
                    .keys()
                    .filter(|name| {
                        project.declarations.contains(name) && resolved.value(name).is_none()
                    })
                    .map(String::as_str)
                    .collect();
            }
        }

        let staged = project
            .staging
            .sources
            .iter()
            .map(|s| StagedEntry {
                key: &s.key,
                host: &s.host,
                guest: &s.guest,
                outside: s.outside,
                dotfiles_root: s.dotfiles_root,
            })
            .collect();

        Self {
            valid: !project.diagnostics.has_errors(),
            kitchen_file: &discovery.kitchen_file,
            kitchen_dir: &discovery.kitchen_dir,
            section: project.kitchen.as_ref().and_then(|k| k.location),
            loaded_files: &discovery.loaded_files,
            sandbox_name: sandbox_name(&discovery.kitchen_file, &discovery.kitchen_dir),
            config,
            env,
            secrets,
            skipped_secrets,
            staged,
            diagnostics: &project.diagnostics,
        }
    }

    fn print(&self) {
        let row = |label: &str, value: &str| println!("{label:<10} {value}");
        let section = match self.section {
            Some(section) => format!("[{}]", section.table_path()),
            None => "(no [_.microkitchen] table; defaults apply)".to_owned(),
        };
        row(
            "kitchen",
            &format!("{}  {section}", self.kitchen_file.display()),
        );
        row("sandbox", &self.sandbox_name);

        if let Some(config) = self.config {
            row(
                "resources",
                &format!(
                    "{} CPUs, {} memory, {} disk",
                    config.cpus,
                    format_mib(config.memory_mib),
                    format_mib(config.disk_mib)
                ),
            );
            let network = &config.network;
            let mut line = network.preset.to_string();
            for (label, items) in [
                ("allow", join(&network.allow)),
                ("deny", join(&network.deny)),
                ("ports", join(&network.ports)),
            ] {
                if !items.is_empty() {
                    line.push_str(&format!("; {label} {items}"));
                }
            }
            row("network", &line);
            for mount in &config.mounts {
                row("mount", &mount.to_string());
            }
            for name in &self.secrets {
                row(
                    "secret",
                    &format!("{name} → {}", join(&config.secrets[*name].allow)),
                );
            }
        }
        if !self.env.is_empty() {
            row("env", &self.env.join(", "));
        }
        if !self.skipped_secrets.is_empty() {
            row("skipped", &self.skipped_secrets.join(", "));
        }
        for entry in &self.staged {
            // Guest paths all share one prefix; showing it once keeps the
            // rows readable.
            let guest = entry
                .guest
                .strip_prefix(&format!("{GUEST_FILES_DIR}/"))
                .unwrap_or(entry.guest);
            let mut note = String::new();
            if entry.dotfiles_root {
                note.push_str("  (dotfiles root)");
            }
            if entry.outside {
                note.push_str("  (outside the project)");
            }
            row(
                "stage",
                &format!("{} → {guest}{note}", entry.host.display()),
            );
        }
        if !self.staged.is_empty() {
            row(
                "",
                &format!("staged under {GUEST_FILES_DIR} in the sandbox"),
            );
        }
    }
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

pub(super) fn run(ctx: &Context) -> Result<ExitCode> {
    let project = Project::load(&ctx.mise, &ctx.cwd)?;
    let report = Report::new(&project);

    if ctx.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        if !ctx.quiet {
            report.print();
        }
        print_diagnostics(&project.diagnostics);
        match project.diagnostics.error_count() {
            0 if !ctx.quiet => eprintln!("configuration is valid"),
            0 => {}
            1 => eprintln!("1 error"),
            n => eprintln!("{n} errors"),
        }
    }

    Ok(if report.valid {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    })
}

fn join<T: ToString>(items: &[T]) -> String {
    items
        .iter()
        .map(T::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}
