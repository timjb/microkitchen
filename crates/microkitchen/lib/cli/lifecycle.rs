//! `up`, `shell`, `exec`, `start`, `stop`, `restart`, `down`, `status`, `list`.

use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, bail};
use microsandbox::Sandbox;
use microsandbox::sandbox::{AttachOptionsBuilder, ExecOptionsBuilder, SandboxHandle};
use serde::Serialize;

use super::ui::{self, PullProgressDisplay, Spinner};
use super::{Context, UpArgs, print_diagnostics};
use crate::broker::client::BrokerClient;
use crate::broker::protocol::Mode;
use crate::config::Project;
use crate::config::discover::{Discovery, discover};
use crate::config::schema::CHEF;
use crate::config::schema::NetworkPreset;
use crate::mise::render::render_guest_config;
use crate::sandbox::bootstrap;
use crate::sandbox::build::{IMAGE, ROOT};
use crate::sandbox::labels;
use crate::sandbox::lifecycle::{self, is_active, label, status_name};
use crate::sandbox::naming::sandbox_name;
use crate::sandbox::plan::Egress;
use crate::sandbox::plan::SandboxPlan;
use crate::sandbox::staging;
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

/// Who `shell` and `exec` run as. chef's home and shell come from the guest,
/// since `[bootstrap.users.chef]` may change them.
struct Session {
    user: &'static str,
    home: String,
    shell: String,
}

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
// Methods
//--------------------------------------------------------------------------------------------------

impl Session {
    async fn lookup(sandbox: &Sandbox, root: bool) -> Result<Self> {
        let user = if root { ROOT } else { CHEF };
        let output = sandbox
            .exec_with("getent", |e| e.args(["passwd", user]).user(ROOT))
            .await
            .context("looking up the sandbox user")?;
        let entry = output.stdout().unwrap_or_default();
        let fields: Vec<&str> = entry.trim().split(':').collect();
        if !output.status().success || fields.len() < 7 {
            bail!("the sandbox has no {user} user yet; run `microkitchen bootstrap`");
        }
        Ok(Self {
            user,
            home: fields[5].to_owned(),
            shell: fields[6].to_owned(),
        })
    }

    fn attach(&self, options: AttachOptionsBuilder) -> AttachOptionsBuilder {
        options
            .user(self.user)
            .cwd(&self.home)
            .env("HOME", &self.home)
            .env("USER", self.user)
            .env("LOGNAME", self.user)
    }

    fn exec(&self, options: ExecOptionsBuilder) -> ExecOptionsBuilder {
        options
            .user(self.user)
            .cwd(&self.home)
            .env("HOME", &self.home)
            .env("USER", self.user)
            .env("LOGNAME", self.user)
    }
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

    let (sandbox, booted) = match lifecycle::find(&plan.kitchen_file).await? {
        Some(handle) if args.recreate => {
            remove(ctx, &handle).await?;
            retire(ctx, handle.name()).await;
            plan.egress = connect_egress(ctx, &plan.name, &plan.kitchen_file, preset, true).await?;
            (create(ctx, &plan).await?, true)
        }
        Some(handle) => {
            if label(&handle, labels::CONFIG_HASH).as_deref() != Some(plan.config_hash.as_str()) {
                say(
                    ctx,
                    "note: the configuration changed since this sandbox was created; \
                     run `microkitchen remodel` to apply the changes",
                );
            }
            let booted = !is_active(handle.status_snapshot());
            (start_mediated(ctx, &handle).await?, booted)
        }
        None => {
            plan.egress = connect_egress(ctx, &plan.name, &plan.kitchen_file, preset, true).await?;
            (create(ctx, &plan).await?, true)
        }
    };

    wait_for_docker(ctx, &sandbox, booted).await?;

    let bootstrapped = SandboxState::load(&ctx.home, &plan.name)?.is_some_and(|s| s.bootstrapped);
    if !bootstrapped {
        run_bootstrap(ctx, &sandbox, &plan.name).await?;
    }

    if args.no_shell {
        say(ctx, format!("{} is ready", plan.name));
        return Ok(ExitCode::SUCCESS);
    }
    attach_shell(&sandbox, false).await
}

pub(super) async fn shell(ctx: &Context, root: bool) -> Result<ExitCode> {
    let sandbox = start_mediated(ctx, &require(ctx).await?).await?;
    attach_shell(&sandbox, root).await
}

async fn attach_shell(sandbox: &Sandbox, root: bool) -> Result<ExitCode> {
    let session = Session::lookup(sandbox, root).await?;
    let code = sandbox
        .attach_with(&session.shell, |a| session.attach(a))
        .await?;
    Ok(exit_code(code))
}

pub(super) async fn exec(ctx: &Context, root: bool, command: &[String]) -> Result<ExitCode> {
    let (program, args) = command.split_first().expect("clap requires a command");
    let sandbox = start_mediated(ctx, &require(ctx).await?).await?;
    let session = Session::lookup(&sandbox, root).await?;

    if std::io::stdin().is_terminal() && std::io::stdout().is_terminal() {
        let code = sandbox
            .attach_with(program, |a| session.attach(a.args(args)))
            .await?;
        return Ok(exit_code(code));
    }
    // The output is the result: the spinner leaves no line behind.
    let spinner = Spinner::new(ctx.quiet, "Running", &command.join(" "));
    let output = sandbox
        .exec_with(program, |e| session.exec(e.args(args)))
        .await;
    spinner.finish_clear();
    let output = output?;
    std::io::stdout().write_all(output.stdout_bytes())?;
    std::io::stderr().write_all(output.stderr_bytes())?;
    Ok(exit_code(output.status().code))
}

pub(super) async fn start(ctx: &Context) -> Result<ExitCode> {
    let handle = require(ctx).await?;
    let booted = !is_active(handle.status_snapshot());
    if !booted {
        say(ctx, format!("{} is already running", handle.name()));
    }
    let sandbox = start_mediated(ctx, &handle).await?;
    wait_for_docker(ctx, &sandbox, booted).await?;
    Ok(ExitCode::SUCCESS)
}

pub(super) async fn stop(ctx: &Context) -> Result<ExitCode> {
    let handle = require(ctx).await?;
    if is_active(handle.status_snapshot()) {
        Spinner::new(ctx.quiet, "Stopping", handle.name())
            .run("Stopped", lifecycle::stop(&handle))
            .await?;
    } else {
        say(ctx, format!("{} is already stopped", handle.name()));
    }
    Ok(ExitCode::SUCCESS)
}

pub(super) async fn restart(ctx: &Context) -> Result<ExitCode> {
    let handle = require(ctx).await?;
    if is_active(handle.status_snapshot()) {
        Spinner::new(ctx.quiet, "Stopping", handle.name())
            .run("Stopped", lifecycle::stop(&handle))
            .await?;
    }
    let handle = handle.refresh().await?;
    let sandbox = start_mediated(ctx, &handle).await?;
    wait_for_docker(ctx, &sandbox, true).await?;
    Ok(ExitCode::SUCCESS)
}

pub(super) async fn down(ctx: &Context, purge: bool) -> Result<ExitCode> {
    let discovery = discover(&ctx.mise, &ctx.cwd)?;
    let name = name_of(&discovery);
    match lifecycle::find(&discovery.kitchen_file).await? {
        Some(handle) => {
            remove(ctx, &handle).await?;
            retire(ctx, handle.name()).await;
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
    let booted = !is_active(handle.status_snapshot());
    let sandbox = start_mediated(ctx, &handle).await?;
    wait_for_docker(ctx, &sandbox, booted).await?;
    restage(ctx, &sandbox, handle.name()).await?;
    run_bootstrap(ctx, &sandbox, handle.name()).await?;
    Ok(ExitCode::SUCCESS)
}

/// Copy the staged files in again, so editing a dotfile and re-running
/// `microkitchen bootstrap` applies it.
///
/// This command exists to retry a failed bootstrap, so a kitchen file that no
/// longer validates is reported and skipped rather than treated as fatal.
async fn restage(ctx: &Context, sandbox: &Sandbox, name: &str) -> Result<()> {
    let project = Project::load(&ctx.mise, &ctx.cwd)?;
    if project.diagnostics.has_errors() {
        print_diagnostics(&project.diagnostics);
        say(
            ctx,
            "the configuration has errors; staged files are unchanged",
        );
        return Ok(());
    }
    let plan = SandboxPlan::from_project(&project)?;
    let mut state = SandboxState::load(&ctx.home, name)?;
    let stale: Vec<String> = state
        .as_ref()
        .map(|state| {
            state
                .staged_paths
                .iter()
                .filter(|path| !plan.stage.roots.contains(path))
                .cloned()
                .collect()
        })
        .unwrap_or_default();

    if plan.stage.is_empty() && stale.is_empty() {
        return Ok(());
    }
    Spinner::new(ctx.quiet, "Staging", &format!("{} files", plan.stage.files))
        .run(
            "Staged",
            staging::apply(sandbox, &plan.stage, plan.config.user, &stale),
        )
        .await?;

    if let Some(state) = &mut state {
        state.staged_digest = plan.stage.digest.clone();
        state.staged_paths = plan.stage.roots.clone();
        state.save(&ctx.home)?;
    }
    Ok(())
}

/// Run `mise bootstrap` and record the result in `state.json` and the label.
pub(super) async fn run_bootstrap(ctx: &Context, sandbox: &Sandbox, name: &str) -> Result<()> {
    let settings = Settings::load(&ctx.home)?;
    let log = ctx.home.logs_dir().join(name).join("bootstrap.log");
    say(
        ctx,
        format!("running mise bootstrap (log: {})", log.display()),
    );

    // Setup installs from wherever it needs to; the broker mediates only afterwards.
    set_broker_mode(ctx, name, Mode::Open).await;
    let steps = bootstrap::steps(settings.mise_version.as_deref());
    let result = bootstrap::run(sandbox, &steps, &log, !ctx.quiet).await;
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
    let started = Instant::now();
    let mut display = PullProgressDisplay::new(
        ctx.quiet,
        IMAGE,
        &format!("{:<12} {}", "Creating", plan.name),
    );
    let result = lifecycle::create(plan, |event| display.handle_event(event)).await;
    display.finish();
    let sandbox = result?;
    if !ctx.quiet {
        ui::success("Created", &plan.name, started.elapsed());
    }
    SandboxState {
        name: plan.name.clone(),
        kitchen_file: plan.kitchen_file.clone(),
        config_hash: plan.config_hash.clone(),
        applied: plan.config.clone(),
        applied_text: Some(plan.kitchen_text.clone()),
        env_keys: plan.env.keys().cloned().collect(),
        guest_config_pending: false,
        staged_digest: plan.stage.digest.clone(),
        staged_paths: plan.stage.roots.clone(),
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
    let sandbox = if is_active(handle.status_snapshot()) {
        lifecycle::start(handle).await?
    } else {
        Spinner::new(ctx.quiet, "Starting", handle.name())
            .run("Started", lifecycle::start(handle))
            .await?
    };
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

/// Wait until dockerd answers. Only a sandbox that just `booted` shows it:
/// in one that was already running, Docker is up.
async fn wait_for_docker(ctx: &Context, sandbox: &Sandbox, booted: bool) -> Result<()> {
    Spinner::new(ctx.quiet || !booted, "Starting", "Docker")
        .run(
            "Started",
            lifecycle::wait_for_docker(sandbox, DOCKER_READY_TIMEOUT),
        )
        .await
}

async fn remove(ctx: &Context, handle: &SandboxHandle) -> Result<()> {
    Spinner::new(ctx.quiet, "Removing", handle.name())
        .run("Removed", lifecycle::remove(handle))
        .await
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
