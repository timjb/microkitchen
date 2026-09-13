//! `up`, `shell`, `exec`, `start`, `stop`, `restart`, `down`, `status`, `list`.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Result, bail};
use microsandbox::Sandbox;
use microsandbox::sandbox::SandboxHandle;
use serde::Serialize;

use super::{Context, UpArgs, print_diagnostics};
use crate::broker::client::BrokerClient;
use crate::broker::protocol::Mode;
use crate::config::Project;
use crate::config::discover::{Discovery, discover};
use crate::config::schema::NetworkPreset;
use crate::mise::render::render_guest_config;
use crate::sandbox::bootstrap;
use crate::sandbox::build::IMAGE;
use crate::sandbox::labels;
use crate::sandbox::lifecycle::{self, is_active, label, status_name};
use crate::sandbox::naming::sandbox_name;
use crate::sandbox::plan::Egress;
use crate::sandbox::plan::SandboxPlan;
use crate::state::sandbox::SandboxState;
use crate::state::secret;
use crate::state::settings::Settings;

//--------------------------------------------------------------------------------------------------
// Constants
//--------------------------------------------------------------------------------------------------

/// How long `up`/`start` wait for dockerd.
const DOCKER_READY_TIMEOUT: Duration = Duration::from_secs(90);

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

#[derive(Serialize)]
struct StatusReport {
    name: String,
    kitchen_file: PathBuf,
    exists: bool,
    status: Option<String>,
    /// Whether the sandbox was created from the current configuration.
    up_to_date: Option<bool>,
}

#[derive(Serialize)]
struct ListEntry {
    name: String,
    status: String,
    kitchen_file: Option<String>,
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

pub(super) async fn up(ctx: &Context, args: &UpArgs) -> Result<ExitCode> {
    let project = Project::load(&ctx.mise, &ctx.cwd)?;
    print_diagnostics(&project.diagnostics);
    if project.diagnostics.has_errors() {
        return Ok(ExitCode::FAILURE);
    }
    let mut plan = SandboxPlan::from_project(&project)?;
    let preset = plan.config.network.preset;

    let sandbox = match lifecycle::find(&plan.kitchen_file).await? {
        Some(handle) if args.recreate => {
            say(ctx, format!("removing {}", handle.name()));
            lifecycle::remove(&handle).await?;
            retire(ctx, handle.name()).await;
            plan.egress = connect_egress(ctx, &plan.name, &plan.kitchen_file, preset, true).await?;
            create(ctx, &plan).await?
        }
        Some(handle) => {
            if label(&handle, labels::CONFIG_HASH).as_deref() != Some(plan.config_hash.as_str()) {
                say(
                    ctx,
                    "note: the configuration changed since this sandbox was created; \
                     run `microkitchen remodel` to apply the changes",
                );
            }
            if !is_active(handle.status_snapshot()) {
                say(ctx, format!("starting {}", handle.name()));
            }
            start_mediated(ctx, &handle).await?
        }
        None => {
            plan.egress = connect_egress(ctx, &plan.name, &plan.kitchen_file, preset, true).await?;
            create(ctx, &plan).await?
        }
    };

    say(ctx, "waiting for Docker");
    lifecycle::wait_for_docker(&sandbox, DOCKER_READY_TIMEOUT).await?;

    let bootstrapped = SandboxState::load(&ctx.home, &plan.name)?.is_some_and(|s| s.bootstrapped);
    if !bootstrapped {
        run_bootstrap(ctx, &sandbox, &plan.name).await?;
    }

    if args.no_shell {
        say(ctx, format!("{} is ready", plan.name));
        return Ok(ExitCode::SUCCESS);
    }
    Ok(exit_code(sandbox.attach_shell().await?))
}

pub(super) async fn shell(ctx: &Context) -> Result<ExitCode> {
    let sandbox = start_mediated(ctx, &require(ctx).await?).await?;
    Ok(exit_code(sandbox.attach_shell().await?))
}

pub(super) async fn exec(ctx: &Context, command: &[String]) -> Result<ExitCode> {
    let (program, args) = command.split_first().expect("clap requires a command");
    let sandbox = start_mediated(ctx, &require(ctx).await?).await?;

    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        return Ok(exit_code(sandbox.attach(program, args).await?));
    }
    let output = sandbox.exec(program, args).await?;
    std::io::stdout().write_all(output.stdout_bytes())?;
    std::io::stderr().write_all(output.stderr_bytes())?;
    Ok(exit_code(output.status().code))
}

pub(super) async fn start(ctx: &Context) -> Result<ExitCode> {
    let handle = require(ctx).await?;
    let sandbox = start_mediated(ctx, &handle).await?;
    lifecycle::wait_for_docker(&sandbox, DOCKER_READY_TIMEOUT).await?;
    say(ctx, format!("{} is running", handle.name()));
    Ok(ExitCode::SUCCESS)
}

pub(super) async fn stop(ctx: &Context) -> Result<ExitCode> {
    let handle = require(ctx).await?;
    lifecycle::stop(&handle).await?;
    say(ctx, format!("{} is stopped", handle.name()));
    Ok(ExitCode::SUCCESS)
}

pub(super) async fn restart(ctx: &Context) -> Result<ExitCode> {
    let handle = require(ctx).await?;
    lifecycle::stop(&handle).await?;
    let handle = handle.refresh().await?;
    let sandbox = start_mediated(ctx, &handle).await?;
    lifecycle::wait_for_docker(&sandbox, DOCKER_READY_TIMEOUT).await?;
    say(ctx, format!("{} restarted", handle.name()));
    Ok(ExitCode::SUCCESS)
}

pub(super) async fn down(ctx: &Context, purge: bool) -> Result<ExitCode> {
    let discovery = discover(&ctx.mise, &ctx.cwd)?;
    let name = name_of(&discovery);
    match lifecycle::find(&discovery.kitchen_file).await? {
        Some(handle) => {
            lifecycle::remove(&handle).await?;
            retire(ctx, handle.name()).await;
            say(ctx, format!("removed {}", handle.name()));
        }
        None => say(
            ctx,
            format!("no sandbox for {}", discovery.kitchen_file.display()),
        ),
    }
    if purge {
        SandboxState::purge(&ctx.home, &name)?;
    }
    Ok(ExitCode::SUCCESS)
}

pub(super) async fn status(ctx: &Context) -> Result<ExitCode> {
    let discovery = discover(&ctx.mise, &ctx.cwd)?;
    let handle = lifecycle::find(&discovery.kitchen_file).await?;

    // The current config hash needs a valid project; status still works without one.
    let current_hash = Project::load(&ctx.mise, &ctx.cwd)
        .ok()
        .and_then(|project| SandboxPlan::from_project(&project).ok())
        .map(|plan| plan.config_hash);

    let report = StatusReport {
        name: handle
            .as_ref()
            .map_or_else(|| name_of(&discovery), |h| h.name().to_owned()),
        kitchen_file: discovery.kitchen_file.clone(),
        exists: handle.is_some(),
        status: handle.as_ref().map(|h| status_name(h.status_snapshot())),
        up_to_date: handle
            .as_ref()
            .zip(current_hash.as_ref())
            .map(|(h, hash)| label(h, labels::CONFIG_HASH).as_deref() == Some(hash.as_str())),
    };

    if ctx.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!("{:<10} {}", "sandbox", report.name);
        println!("{:<10} {}", "kitchen", report.kitchen_file.display());
        println!(
            "{:<10} {}",
            "status",
            report.status.as_deref().unwrap_or("not created")
        );
        match report.up_to_date {
            Some(false) => println!(
                "{:<10} configuration changed; run `microkitchen remodel`",
                "config"
            ),
            Some(true) => println!("{:<10} up to date", "config"),
            None => {}
        }
    }
    Ok(ExitCode::SUCCESS)
}

pub(super) async fn list(ctx: &Context) -> Result<ExitCode> {
    let entries: Vec<ListEntry> = lifecycle::list_managed()
        .await?
        .iter()
        .map(|handle| ListEntry {
            name: handle.name().to_owned(),
            status: status_name(handle.status_snapshot()),
            kitchen_file: label(handle, labels::CONFIG),
        })
        .collect();

    if ctx.json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
    } else if entries.is_empty() {
        say(ctx, "no sandboxes");
    } else {
        for entry in &entries {
            println!(
                "{:<40} {:<10} {}",
                entry.name,
                entry.status,
                entry.kitchen_file.as_deref().unwrap_or("-")
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

pub(super) async fn bootstrap(ctx: &Context) -> Result<ExitCode> {
    let handle = require(ctx).await?;
    let sandbox = start_mediated(ctx, &handle).await?;
    lifecycle::wait_for_docker(&sandbox, DOCKER_READY_TIMEOUT).await?;
    run_bootstrap(ctx, &sandbox, handle.name()).await?;
    Ok(ExitCode::SUCCESS)
}

/// Run `mise bootstrap` and record the result in `state.json` and the label.
async fn run_bootstrap(ctx: &Context, sandbox: &Sandbox, name: &str) -> Result<()> {
    let settings = Settings::load(&ctx.home)?;
    let log = ctx.home.logs_dir().join(name).join("bootstrap.log");
    say(
        ctx,
        format!("running mise bootstrap (log: {})", log.display()),
    );

    // Setup installs from wherever it needs to; the broker mediates only afterwards.
    set_broker_mode(ctx, name, Mode::Open).await;
    let script = bootstrap::script(settings.mise_version.as_deref());
    let result = bootstrap::run(sandbox, &script, &log, !ctx.quiet).await;
    let succeeded = result.is_ok();
    set_broker_mode(ctx, name, Mode::Enforce).await;

    bootstrap::mark(sandbox, succeeded).await;
    if let Some(mut state) = SandboxState::load(&ctx.home, name)? {
        state.bootstrapped = succeeded;
        state.save(&ctx.home)?;
    }
    result
}

async fn create(ctx: &Context, plan: &SandboxPlan) -> Result<Sandbox> {
    say(
        ctx,
        format!(
            "creating {} from {IMAGE} (the first start pulls the image)",
            plan.name
        ),
    );
    let sandbox = lifecycle::create(plan).await?;
    SandboxState {
        name: plan.name.clone(),
        kitchen_file: plan.kitchen_file.clone(),
        config_hash: plan.config_hash.clone(),
        applied: plan.config.clone(),
        applied_text: Some(plan.kitchen_text.clone()),
        env_keys: plan.env.keys().cloned().collect(),
        guest_config_pending: false,
        bootstrapped: false,
        resolver_port: plan.egress.as_ref().map(|e| e.resolver_port),
        proxy_port: plan.egress.as_ref().map(|e| e.proxy_port),
    }
    .save(&ctx.home)?;
    Ok(sandbox)
}

/// Start (or connect to) a sandbox after registering it with the broker.
/// Registering on every start also restores egress after a broker restart.
pub(super) async fn start_mediated(ctx: &Context, handle: &SandboxHandle) -> Result<Sandbox> {
    let state = SandboxState::load(&ctx.home, handle.name())?;
    if let Some(state) = &state {
        let preset = state.applied.network.preset;
        connect_egress(ctx, handle.name(), &state.kitchen_file, preset, false).await?;
    }
    let sandbox = lifecycle::start(handle).await?;
    if let Some(mut state) = state
        && state.guest_config_pending
    {
        // `remodel` changed the kitchen file while the sandbox was stopped.
        let text = std::fs::read_to_string(&state.kitchen_file)?;
        lifecycle::write_guest_config(&sandbox, &render_guest_config(&text)?).await?;
        state.guest_config_pending = false;
        state.save(&ctx.home)?;
    }
    Ok(sandbox)
}

/// Register the sandbox with the broker (starting the broker if needed). A
/// `fresh` sandbox gets new endpoints; an existing one keeps the ones it was
/// created with. `None` when the sandbox has no network.
async fn connect_egress(
    ctx: &Context,
    name: &str,
    kitchen_file: &Path,
    preset: NetworkPreset,
    fresh: bool,
) -> Result<Option<Egress>> {
    if preset == NetworkPreset::None {
        return Ok(None);
    }
    let state = if fresh {
        None
    } else {
        SandboxState::load(&ctx.home, name)?
    };
    if state.as_ref().is_some_and(|s| s.proxy_port.is_none()) {
        say(
            ctx,
            format!(
                "note: {name} was created without the egress broker; \
                 recreate it with `microkitchen up --recreate` to mediate its traffic"
            ),
        );
        return Ok(None);
    }
    let secret_env = secret::env_var(name);
    if std::env::var_os(&secret_env).is_none() {
        bail!("the proxy secret for {name} was not exported (internal error)");
    }

    let bootstrapped = state.as_ref().is_some_and(|s| s.bootstrapped);
    let mode = if bootstrapped {
        Mode::Enforce
    } else {
        Mode::Open
    };
    let client = BrokerClient::ensure_running(&ctx.home).await?;
    let registration = client
        .register(
            name,
            kitchen_file,
            mode,
            state.as_ref().and_then(|s| s.resolver_port),
            state.as_ref().and_then(|s| s.proxy_port),
        )
        .await?;
    Ok(Some(Egress {
        resolver_port: registration.resolver_port,
        proxy_port: registration.proxy_port,
        secret_env,
    }))
}

async fn set_broker_mode(ctx: &Context, name: &str, mode: Mode) {
    let client = BrokerClient::new(&ctx.home);
    if client.is_running().await
        && let Err(error) = client.set_mode(name, mode).await
    {
        tracing::debug!(%error, "could not change the broker mode");
    }
}

async fn retire(ctx: &Context, name: &str) {
    let client = BrokerClient::new(&ctx.home);
    if client.is_running().await {
        let _ = client.retire(name).await;
    }
}

/// The project's sandbox, which must exist.
async fn require(ctx: &Context) -> Result<SandboxHandle> {
    let discovery = discover(&ctx.mise, &ctx.cwd)?;
    match lifecycle::find(&discovery.kitchen_file).await? {
        Some(handle) => Ok(handle),
        None => bail!(
            "there is no sandbox for {} yet; run `microkitchen up`",
            discovery.kitchen_file.display()
        ),
    }
}

fn name_of(discovery: &Discovery) -> String {
    sandbox_name(&discovery.kitchen_file, &discovery.kitchen_dir)
}

fn say(ctx: &Context, message: impl AsRef<str>) {
    if !ctx.quiet {
        eprintln!("{}", message.as_ref());
    }
}

fn exit_code(code: i32) -> ExitCode {
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}
