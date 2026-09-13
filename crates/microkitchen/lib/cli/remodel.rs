//! `microkitchen remodel`: apply kitchen-file changes to the existing sandbox
//! (implementation plan §9).
//!
//! Every change goes where it can: network rules are live through the
//! broker; resources, the disk, the environment and secrets go through the
//! SDK's `modify()`, live when the runtime allows it and otherwise at the next
//! start; mounts, ports and the network preset need a new sandbox.

use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::process::ExitCode;

use anyhow::{Context as _, Result, bail};
use microsandbox::sandbox::{
    ModificationDisposition, PlannedChange, SandboxHandle, SandboxModificationBuilder,
    SandboxModificationPlan,
};

use super::{Context, UpArgs, lifecycle as commands, print_diagnostics};
use crate::config::Project;
use crate::config::hostpat::HostPattern;
use crate::mise::render::render_guest_config;
use crate::sandbox::build::GUEST_PATH;
use crate::sandbox::labels;
use crate::sandbox::lifecycle::{self, is_active};
use crate::sandbox::plan::{SandboxPlan, config_hash};
use crate::sandbox::remodel::{self, Change, EnvPatch, Route};
use crate::state::sandbox::SandboxState;

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// What goes through `modify()`, in groups that apply live or at the next
/// start independently.
#[derive(Default)]
struct SdkChanges {
    cpus: Option<u8>,
    memory_mib: Option<u32>,
    disk_mib: Option<u32>,
    env: EnvPatch,
    secrets: Vec<SecretUpdate>,
    secrets_remove: Vec<String>,
}

/// A secret to add or update. `value` only when it is new or changed:
/// the SDK treats any value as a rotation.
#[derive(Clone)]
struct SecretUpdate {
    name: String,
    value: Option<String>,
    hosts: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Group {
    Resources,
    Disk,
    Env,
    Secrets,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl SdkChanges {
    fn groups(&self) -> Vec<Group> {
        let mut groups = Vec::new();
        if self.cpus.is_some() || self.memory_mib.is_some() {
            groups.push(Group::Resources);
        }
        if self.disk_mib.is_some() {
            groups.push(Group::Disk);
        }
        if !self.env.is_empty() {
            groups.push(Group::Env);
        }
        if !self.secrets.is_empty() || !self.secrets_remove.is_empty() {
            groups.push(Group::Secrets);
        }
        groups
    }

    fn builder(&self, handle: &SandboxHandle, groups: &[Group]) -> SandboxModificationBuilder {
        let mut builder = handle.modify();
        for group in groups {
            match group {
                Group::Resources => {
                    if let Some(cpus) = self.cpus {
                        builder = builder.cpus(cpus);
                    }
                    if let Some(mib) = self.memory_mib {
                        builder = builder.memory_mib(mib);
                    }
                }
                Group::Disk => {
                    if let Some(mib) = self.disk_mib {
                        builder = builder.root_disk_size_mib(mib);
                    }
                }
                Group::Env => {
                    for (key, value) in &self.env.set {
                        builder = builder.env(key, value);
                    }
                    for key in &self.env.remove {
                        builder = builder.remove_env(key);
                    }
                }
                Group::Secrets => {
                    for update in &self.secrets {
                        let update = update.clone();
                        builder = builder.secret(move |mut spec| {
                            spec = spec.env(update.name);
                            if let Some(value) = update.value {
                                spec = spec.value(value);
                            }
                            for host in update.hosts {
                                spec = spec.allow_host(host);
                            }
                            spec
                        });
                    }
                    for name in &self.secrets_remove {
                        builder = builder.remove_secret(name);
                    }
                }
            }
        }
        builder
    }
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

pub(super) async fn run(ctx: &Context, yes: bool, recreate: bool) -> Result<ExitCode> {
    let project = Project::load(&ctx.mise, &ctx.cwd)?;
    print_diagnostics(&project.diagnostics);
    if project.diagnostics.has_errors() {
        return Ok(ExitCode::FAILURE);
    }
    let plan = SandboxPlan::from_project(&project)?;
    let Some(handle) = lifecycle::find(&plan.kitchen_file).await? else {
        bail!(
            "there is no sandbox for {} yet; run `microkitchen up`",
            plan.kitchen_file.display()
        );
    };
    let name = handle.name().to_owned();
    let Some(mut state) = SandboxState::load(&ctx.home, &name)? else {
        bail!(
            "there is no record of how {name} was created; \
             recreate it with `microkitchen up --recreate`"
        );
    };

    let changes = remodel::diff(&state.applied, &plan.config);
    let sdk = sdk_changes(&handle, &state, &plan)?;
    let guest_changed = match &state.applied_text {
        Some(text) => render_guest_config(text)? != plan.guest_config,
        None => true,
    };
    let dry_run = if sdk.groups().is_empty() {
        None
    } else {
        Some(
            sdk.builder(&handle, &sdk.groups())
                .dry_run()
                .await
                .context("planning the changes")?,
        )
    };
    let pending_recreate: Vec<&Change> = changes
        .iter()
        .filter(|c| c.route == Route::Recreate)
        .collect();

    if changes.is_empty() && dry_run.is_none() && !guest_changed {
        say(
            ctx,
            format!(
                "nothing to remodel: {name} matches {}",
                plan.kitchen_file.display()
            ),
        );
        return Ok(ExitCode::SUCCESS);
    }

    show(
        ctx,
        &state,
        &plan,
        &changes,
        dry_run.as_ref(),
        guest_changed,
    );
    if let Some(dry_run) = &dry_run {
        if !dry_run.conflicts.is_empty() {
            for conflict in &dry_run.conflicts {
                eprintln!("cannot change {}: {}", conflict.field, conflict.message);
            }
            bail!("nothing was changed");
        }
        if dry_run
            .changes
            .iter()
            .any(|c| disposition(c) == ModificationDisposition::Unsupported)
        {
            bail!("{name} is starting or stopping; try again once it is running or stopped");
        }
    }

    if recreate && !pending_recreate.is_empty() {
        if !yes
            && !confirm(&format!(
                "Recreate {name} with the new configuration? Only the mise cache is kept."
            ))?
        {
            say(ctx, "nothing was changed");
            return Ok(ExitCode::FAILURE);
        }
        return commands::up(
            ctx,
            &UpArgs {
                no_shell: true,
                recreate: true,
            },
        )
        .await;
    }
    if !yes && !confirm("Apply these changes?")? {
        say(ctx, "nothing was changed");
        return Ok(ExitCode::FAILURE);
    }

    // Live where the SDK can, the rest recorded for the next start.
    let mut after_restart = Vec::new();
    if let Some(dry_run) = &dry_run {
        let (now, later): (Vec<Group>, Vec<Group>) = sdk.groups().into_iter().partition(|group| {
            !dry_run.changes.iter().any(|c| {
                group_of(c) == Some(*group)
                    && disposition(c) == ModificationDisposition::RequiresRestart
            })
        });
        if !now.is_empty() {
            sdk.builder(&handle, &now)
                .apply()
                .await
                .context("applying the changes")?;
        }
        if !later.is_empty() {
            sdk.builder(&handle, &later)
                .next_start()
                .apply()
                .await
                .context("recording the changes for the next start")?;
            after_restart = later;
        }
    }

    let running = is_active(handle.status_snapshot());
    state.applied = remodel::applied_without_recreate(&state.applied, &plan.config);
    state.config_hash = config_hash(&state.applied);
    state.applied_text = Some(plan.kitchen_text.clone());
    state.env_keys = plan.env.keys().cloned().collect();
    if guest_changed {
        if running {
            let sandbox = handle.connect().await?;
            lifecycle::write_guest_config(&sandbox, &plan.guest_config).await?;
        } else {
            state.guest_config_pending = true;
        }
    }
    state.save(&ctx.home)?;
    set_label(&handle, labels::CONFIG_HASH, &state.config_hash).await;

    say(ctx, format!("remodeled {name}"));
    if !after_restart.is_empty() {
        let what = after_restart
            .iter()
            .map(|g| format!("{g:?}").to_lowercase())
            .collect::<Vec<_>>()
            .join(", ");
        if running {
            say(
                ctx,
                format!("run `microkitchen restart` to apply the {what} changes"),
            );
        } else {
            say(ctx, format!("the {what} changes apply at the next start"));
        }
    }
    if !pending_recreate.is_empty() {
        let fields = pending_recreate
            .iter()
            .map(|c| c.field.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        say(
            ctx,
            format!(
                "not applied: {fields} need a new sandbox; run `microkitchen remodel --recreate` \
                 (only the mise cache is kept)"
            ),
        );
    }
    if guest_changed {
        say(
            ctx,
            "the guest's mise.toml is updated; run `microkitchen bootstrap` to install new tools",
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// The SDK-managed part of the change. Resources come from the recorded
/// configuration; the environment and secret values from the sandbox's own
/// stored configuration, since `state.json` never holds them.
fn sdk_changes(
    handle: &SandboxHandle,
    state: &SandboxState,
    plan: &SandboxPlan,
) -> Result<SdkChanges> {
    let config = handle
        .config()
        .context("reading the sandbox's configuration")?;
    let (applied, desired) = (&state.applied, &plan.config);
    let mut sdk = SdkChanges {
        cpus: (desired.cpus != applied.cpus).then_some(desired.cpus),
        memory_mib: (desired.memory_mib != applied.memory_mib).then_some(desired.memory_mib),
        disk_mib: (desired.disk_mib != applied.disk_mib).then_some(desired.disk_mib),
        ..SdkChanges::default()
    };

    let current_env: Vec<(String, String)> = config
        .spec
        .env
        .iter()
        .map(|e| (e.key.clone(), e.value.clone()))
        .collect();
    let mut desired_env = plan.env.clone();
    desired_env
        .entry("PATH".to_owned())
        .or_insert_with(|| GUEST_PATH.to_owned());
    sdk.env = remodel::env_patch(&current_env, &desired_env, &state.env_keys);

    let stored: BTreeMap<String, String> = config
        .spec
        .network
        .secrets
        .as_ref()
        .map(|s| {
            s.secrets
                .iter()
                .map(|e| (e.env_var.clone(), e.value.to_string()))
                .collect()
        })
        .unwrap_or_default();
    for (name, (value, secret)) in &plan.secrets {
        let value_changed = stored.get(name) != Some(value);
        let hosts_changed = applied.secrets.get(name).map(|s| &s.allow) != Some(&secret.allow);
        if value_changed || hosts_changed {
            sdk.secrets.push(SecretUpdate {
                name: name.clone(),
                value: value_changed.then(|| value.clone()),
                hosts: secret.allow.iter().filter_map(secret_host).collect(),
            });
        }
    }
    sdk.secrets_remove = stored
        .keys()
        .filter(|name| !plan.secrets.contains_key(*name))
        .cloned()
        .collect();
    Ok(sdk)
}

/// The form the sandbox builder gives a secret's allowed host.
fn secret_host(pattern: &HostPattern) -> Option<String> {
    match pattern {
        HostPattern::Exact(host) => Some(host.clone()),
        HostPattern::Suffix(_) => Some(pattern.to_string()),
        HostPattern::Address(_) | HostPattern::Network(_) => None,
    }
}

fn show(
    ctx: &Context,
    state: &SandboxState,
    plan: &SandboxPlan,
    changes: &[Change],
    dry_run: Option<&SandboxModificationPlan>,
    guest_changed: bool,
) {
    let file = plan
        .kitchen_file
        .file_name()
        .map_or_else(|| "mise.toml".into(), |n| n.to_string_lossy().into_owned());
    match &state.applied_text {
        Some(old) => {
            if let Some(diff) = remodel::text_diff(old, &plan.kitchen_text, &file) {
                println!("{diff}");
            }
        }
        None => say(
            ctx,
            "(this sandbox predates recorded kitchen files; showing the changes only)",
        ),
    }

    println!("changes:");
    for change in changes {
        let how = match change.route {
            Route::Broker => "applies now (the broker re-reads the kitchen file)",
            Route::Recreate => "needs a new sandbox (`remodel --recreate`)",
            // Shown from the SDK's plan below.
            Route::Sdk => continue,
        };
        println!(
            "  {:<22} {} -> {}   {how}",
            change.field, change.before, change.after
        );
    }
    if let Some(dry_run) = dry_run {
        for change in &dry_run.changes {
            match change {
                PlannedChange::Config(c) => println!(
                    "  {:<22} {} -> {}   {}",
                    c.field,
                    c.before.as_deref().unwrap_or("(none)"),
                    c.after.as_deref().unwrap_or("(none)"),
                    describe(c.disposition),
                ),
                PlannedChange::Secret(s) => println!(
                    "  {:<22} {}   {}",
                    format!("secret {}", s.name),
                    format!("{:?}", s.change).to_lowercase(),
                    describe(s.disposition),
                ),
            }
        }
        for warning in &dry_run.warnings {
            say(ctx, format!("note: {}: {}", warning.field, warning.message));
        }
    }
    if guest_changed {
        println!(
            "  {:<22} updated in the guest (new tools need `microkitchen bootstrap`)",
            "mise.toml"
        );
    }
}

fn describe(disposition: ModificationDisposition) -> &'static str {
    match disposition {
        ModificationDisposition::Live => "applies now",
        ModificationDisposition::NextStart => "at the next start",
        ModificationDisposition::RequiresRestart => "after `microkitchen restart`",
        ModificationDisposition::Unsupported => "not possible right now",
    }
}

fn disposition(change: &PlannedChange) -> ModificationDisposition {
    match change {
        PlannedChange::Config(c) => c.disposition,
        PlannedChange::Secret(s) => s.disposition,
    }
}

fn group_of(change: &PlannedChange) -> Option<Group> {
    match change {
        PlannedChange::Config(c) => match c.field.as_str() {
            "cpus" | "max_cpus" | "memory" | "max_memory" => Some(Group::Resources),
            "root_disk_size" => Some(Group::Disk),
            "env" => Some(Group::Env),
            _ => None,
        },
        PlannedChange::Secret(_) => Some(Group::Secrets),
    }
}

/// Labels cannot always change live; then they change at the next start.
async fn set_label(handle: &SandboxHandle, key: &str, value: &str) {
    if handle.modify().label(key, value).apply().await.is_ok() {
        return;
    }
    if let Err(error) = handle.modify().label(key, value).next_start().apply().await {
        tracing::warn!(%error, key, "could not update the sandbox label");
    }
}

fn confirm(question: &str) -> Result<bool> {
    if !std::io::stdin().is_terminal() {
        bail!("not running in a terminal, so nothing can be confirmed; pass --yes to apply");
    }
    eprint!("{question} [y/N] ");
    std::io::stderr().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

fn say(ctx: &Context, message: impl AsRef<str>) {
    if !ctx.quiet {
        eprintln!("{}", message.as_ref());
    }
}
