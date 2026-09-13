//! Command-line interface. One module per subcommand.

mod broker;
mod lifecycle;
mod net;
mod remodel;
mod validate;

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use anyhow::{Context as _, Result, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use owo_colors::OwoColorize;
use tracing_subscriber::EnvFilter;

use crate::broker::protocol::{Answer, Mode};
use crate::config::discover::discover;
use crate::config::edit::RuleList;
use crate::config::{Diagnostics, Severity};
use crate::mise::Mise;
use crate::sandbox::naming::sandbox_name;
use crate::state::{HOME_ENV, Home, secret};

//--------------------------------------------------------------------------------------------------
// Types
//--------------------------------------------------------------------------------------------------

/// Launch microsandbox VMs from a mise.toml.
#[derive(Debug, Parser)]
#[command(name = "microkitchen", version, propagate_version = true)]
pub struct Cli {
    /// Run as if started in DIR.
    #[arg(short = 'C', long = "cd", value_name = "DIR", global = true)]
    pub dir: Option<PathBuf>,

    /// State directory [default: ~/.microkitchen].
    #[arg(long, env = HOME_ENV, value_name = "DIR", global = true)]
    pub home: Option<PathBuf>,

    /// More log output (repeatable).
    #[arg(short, long, action = clap::ArgAction::Count, global = true)]
    pub verbose: u8,

    /// Only print errors.
    #[arg(short, long, global = true)]
    pub quiet: bool,

    /// Machine-readable output where supported.
    #[arg(long, global = true)]
    pub json: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create or start the sandbox, bootstrap it and attach a shell (the default).
    Up(UpArgs),
    /// Attach an interactive shell.
    Shell,
    /// Run a command in the sandbox.
    Exec {
        #[arg(
            trailing_var_arg = true,
            allow_hyphen_values = true,
            required = true,
            value_name = "CMD"
        )]
        command: Vec<String>,
    },
    /// Stop the sandbox.
    Stop,
    /// Start a stopped sandbox.
    Start,
    /// Stop and start the sandbox.
    Restart,
    /// Stop and remove the sandbox.
    Down {
        /// Also delete state and logs (never the shared mise cache).
        #[arg(long)]
        purge: bool,
    },
    /// Show the sandbox and broker state.
    Status,
    /// List sandboxes created by microkitchen.
    List,
    /// Show logs.
    Logs {
        #[arg(long, group = "source")]
        bootstrap: bool,
        #[arg(long, group = "source")]
        broker: bool,
        #[arg(long, group = "source")]
        sandbox: bool,
    },
    /// Run `mise bootstrap` in the sandbox again.
    Bootstrap,
    /// Validate the configuration and resolve the environment without creating anything.
    Validate,
    /// Apply configuration changes to the existing sandbox.
    Remodel {
        /// Do not ask for confirmation.
        #[arg(short, long)]
        yes: bool,
        /// Recreate the sandbox when a change requires it.
        #[arg(long)]
        recreate: bool,
    },
    /// Network approvals and rules.
    #[command(subcommand)]
    Net(NetCommand),
    /// Control the egress broker daemon.
    #[command(subcommand)]
    Broker(BrokerCommand),
}

#[derive(Debug, Default, Args)]
pub struct UpArgs {
    /// Do not attach a shell.
    #[arg(long)]
    pub no_shell: bool,
    /// Remove and recreate the sandbox (the mise cache is kept).
    #[arg(long)]
    pub recreate: bool,
}

#[derive(Debug, Subcommand)]
pub enum NetCommand {
    /// List approvals waiting for a decision.
    Pending,
    /// Answer a pending approval.
    Decide { id: u64, verdict: Verdict },
    /// Add a rule to the kitchen file's allow list.
    Allow {
        /// Host, `*.suffix`, IP address or CIDR range.
        rule: String,
        /// Write to ~/.microkitchen/rules.toml instead, for every sandbox.
        #[arg(long)]
        global: bool,
    },
    /// Add a rule to the kitchen file's deny list.
    Deny {
        /// Host, `*.suffix`, IP address or CIDR range.
        rule: String,
        /// Write to ~/.microkitchen/rules.toml instead, for every sandbox.
        #[arg(long)]
        global: bool,
    },
    /// Allow a host for five minutes.
    Temp { host: String },
    /// Show the rules in effect.
    Rules,
    /// Remove a rule.
    Revoke { rule: String },
    /// Switch the sandbox between open and enforce mode.
    Mode { mode: BrokerMode },
    /// Leave deny-all after the approval rate limit tripped.
    Resume,
    /// Show the names the sandbox resolved.
    Bindings,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Verdict {
    Allow,
    Deny,
    Temp,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum BrokerMode {
    Open,
    Enforce,
}

#[derive(Debug, Subcommand)]
pub enum BrokerCommand {
    Start,
    Stop,
    Status,
    /// Run the daemon in the foreground.
    #[command(hide = true)]
    Run,
}

/// What every subcommand gets.
pub struct Context {
    pub cwd: PathBuf,
    pub home: Home,
    pub json: bool,
    pub quiet: bool,
    pub mise: Mise,
}

//--------------------------------------------------------------------------------------------------
// Methods
//--------------------------------------------------------------------------------------------------

impl Cli {
    /// `microkitchen broker run`.
    pub fn is_broker_daemon(&self) -> bool {
        matches!(self.command, Some(Command::Broker(BrokerCommand::Run)))
    }
}

impl From<Verdict> for Answer {
    fn from(verdict: Verdict) -> Self {
        match verdict {
            Verdict::Allow => Self::Allow,
            Verdict::Deny => Self::Deny,
            Verdict::Temp => Self::Temp,
        }
    }
}

impl From<BrokerMode> for Mode {
    fn from(mode: BrokerMode) -> Self {
        match mode {
            BrokerMode::Open => Self::Open,
            BrokerMode::Enforce => Self::Enforce,
        }
    }
}

impl Command {
    fn name(&self) -> &'static str {
        match self {
            Self::Up(_) => "up",
            Self::Shell => "shell",
            Self::Exec { .. } => "exec",
            Self::Stop => "stop",
            Self::Start => "start",
            Self::Restart => "restart",
            Self::Down { .. } => "down",
            Self::Status => "status",
            Self::List => "list",
            Self::Logs { .. } => "logs",
            Self::Bootstrap => "bootstrap",
            Self::Validate => "validate",
            Self::Remodel { .. } => "remodel",
            Self::Net(_) => "net",
            Self::Broker(_) => "broker",
        }
    }
}

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// Log to stderr; `MICROKITCHEN_LOG` (an `EnvFilter` directive) overrides `-v`/`-q`.
pub fn init_logging(verbose: u8, quiet: bool) {
    let default = match (quiet, verbose) {
        (true, _) => "error",
        (false, 0) => "warn",
        (false, 1) => "info",
        (false, 2) => "debug",
        (false, _) => "trace",
    };
    let filter =
        EnvFilter::try_from_env("MICROKITCHEN_LOG").unwrap_or_else(|_| EnvFilter::new(default));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .without_time()
        .try_init();
}

pub async fn run(cli: Cli) -> Result<ExitCode> {
    let cwd = resolve_cwd(cli.dir.as_deref())?;
    let ctx = Context {
        cwd,
        home: Home::resolve(cli.home)?,
        json: cli.json,
        quiet: cli.quiet,
        mise: Mise::from_env(),
    };

    match cli.command.unwrap_or(Command::Up(UpArgs::default())) {
        Command::Up(args) => lifecycle::up(&ctx, &args).await,
        Command::Shell => lifecycle::shell(&ctx).await,
        Command::Exec { command } => lifecycle::exec(&ctx, &command).await,
        Command::Start => lifecycle::start(&ctx).await,
        Command::Stop => lifecycle::stop(&ctx).await,
        Command::Restart => lifecycle::restart(&ctx).await,
        Command::Down { purge } => lifecycle::down(&ctx, purge).await,
        Command::Status => lifecycle::status(&ctx).await,
        Command::List => lifecycle::list(&ctx).await,
        Command::Bootstrap => lifecycle::bootstrap(&ctx).await,
        Command::Validate => validate::run(&ctx),
        Command::Net(NetCommand::Allow { rule, global }) => {
            net::set_rule(&ctx, RuleList::Allow, &rule, global)
        }
        Command::Net(NetCommand::Deny { rule, global }) => {
            net::set_rule(&ctx, RuleList::Deny, &rule, global)
        }
        Command::Net(NetCommand::Pending) => net::pending(&ctx).await,
        Command::Net(NetCommand::Decide { id, verdict }) => {
            net::decide(&ctx, id, verdict.into()).await
        }
        Command::Net(NetCommand::Mode { mode }) => net::mode(&ctx, mode.into()).await,
        Command::Net(NetCommand::Bindings) => net::bindings(&ctx).await,
        Command::Net(NetCommand::Temp { host }) => net::temp(&ctx, &host).await,
        Command::Net(NetCommand::Resume) => net::resume(&ctx).await,
        Command::Remodel { yes, recreate } => remodel::run(&ctx, yes, recreate).await,
        Command::Broker(command) => broker::run(&ctx, command).await,
        other => bail!("`microkitchen {}` is not implemented yet", other.name()),
    }
}

/// Export the project's proxy password for microsandbox, which reads it from
/// the host environment whenever it starts a sandbox. Must run before the
/// async runtime exists: changing the environment is only sound while the
/// process is single-threaded.
pub fn export_proxy_secret(cli: &Cli) -> Result<()> {
    let may_start_sandbox = matches!(
        cli.command,
        None | Some(
            Command::Up(_)
                | Command::Start
                | Command::Restart
                | Command::Shell
                | Command::Exec { .. }
                | Command::Bootstrap
                | Command::Remodel { .. }
        )
    );
    if !may_start_sandbox {
        return Ok(());
    }
    let cwd = resolve_cwd(cli.dir.as_deref())?;
    // Problems finding the project are reported by the command itself.
    let Ok(discovery) = discover(&Mise::from_env(), &cwd) else {
        return Ok(());
    };
    let home = Home::resolve(cli.home.clone())?;
    let name = sandbox_name(&discovery.kitchen_file, &discovery.kitchen_dir);
    let value = secret::load_or_create(&home, &name)?;
    // SAFETY: main calls this before creating the tokio runtime; no other
    // threads exist yet.
    unsafe { std::env::set_var(secret::env_var(&name), value) };
    Ok(())
}

fn resolve_cwd(dir: Option<&Path>) -> Result<PathBuf> {
    match dir {
        Some(dir) => {
            std::path::absolute(dir).with_context(|| format!("resolving {}", dir.display()))
        }
        None => std::env::current_dir().context("reading the current directory"),
    }
}

pub fn print_error(error: &anyhow::Error) {
    eprintln!("{} {error:#}", label(Severity::Error));
}

fn print_diagnostics(diagnostics: &Diagnostics) {
    for diagnostic in diagnostics.iter() {
        eprintln!(
            "{} {}",
            label(diagnostic.severity),
            diagnostic.located_message()
        );
    }
}

fn label(severity: Severity) -> String {
    let text = format!("{severity}:");
    if !use_color() {
        return text;
    }
    match severity {
        Severity::Error => text.red().bold().to_string(),
        Severity::Warning => text.yellow().bold().to_string(),
        Severity::Notice => text.cyan().bold().to_string(),
    }
}

fn use_color() -> bool {
    std::env::var_os("NO_COLOR").is_none() && std::io::stderr().is_terminal()
}
