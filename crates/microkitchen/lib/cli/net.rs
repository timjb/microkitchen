//! `microkitchen net …`: rule edits and the headless approval surface.

use std::process::ExitCode;

use anyhow::{Context as _, Result, bail};

use super::Context;
use crate::broker::client::BrokerClient;
use crate::broker::protocol::{Answer, Mode, PendingApproval};
use crate::config::discover::discover;
use crate::config::edit::{self, RuleList};
use crate::config::hostpat::HostPattern;
use crate::sandbox::naming::sandbox_name;

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// `net pending`: approvals waiting for an answer, across all sandboxes.
pub(super) async fn pending(ctx: &Context) -> Result<ExitCode> {
    let client = BrokerClient::new(&ctx.home);
    let pending = if client.is_running().await {
        client.pending().await?
    } else {
        Vec::new()
    };
    if ctx.json {
        println!("{}", serde_json::to_string_pretty(&pending)?);
    } else if pending.is_empty() {
        if !ctx.quiet {
            eprintln!("no pending approvals");
        }
    } else {
        for approval in &pending {
            print_pending(approval);
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `net decide <id> allow|deny|temp`.
pub(super) async fn decide(ctx: &Context, id: u64, answer: Answer) -> Result<ExitCode> {
    BrokerClient::new(&ctx.home).decide(id, answer).await?;
    if !ctx.quiet {
        eprintln!("answered approval {id}");
    }
    Ok(ExitCode::SUCCESS)
}

/// `net mode open|enforce` for the project's sandbox.
pub(super) async fn mode(ctx: &Context, mode: Mode) -> Result<ExitCode> {
    let name = project_sandbox(ctx)?;
    BrokerClient::new(&ctx.home).set_mode(&name, mode).await?;
    if !ctx.quiet {
        eprintln!(
            "{name} is now in {} mode",
            format!("{mode:?}").to_lowercase()
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// `net bindings`: names the project's sandbox resolved, by address.
pub(super) async fn bindings(ctx: &Context) -> Result<ExitCode> {
    let name = project_sandbox(ctx)?;
    let bindings = BrokerClient::new(&ctx.home).bindings(&name).await?;
    if ctx.json {
        println!("{}", serde_json::to_string_pretty(&bindings)?);
    } else {
        for binding in &bindings {
            println!(
                "{:<40} {}  (expires in {}s)",
                binding.address,
                binding.names.join(", "),
                binding.expires_in_secs
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

/// `net temp <host>`: allow a host or address for a few minutes, not persisted.
pub(super) async fn temp(ctx: &Context, host: &str) -> Result<ExitCode> {
    let name = project_sandbox(ctx)?;
    BrokerClient::new(&ctx.home).grant(&name, host).await?;
    if !ctx.quiet {
        eprintln!("allowed {host} temporarily for {name}");
    }
    Ok(ExitCode::SUCCESS)
}

/// `net resume`: leave deny-all after the approval rate limit tripped.
pub(super) async fn resume(ctx: &Context) -> Result<ExitCode> {
    let name = project_sandbox(ctx)?;
    let was_limited = BrokerClient::new(&ctx.home).resume(&name).await?;
    if !ctx.quiet {
        if was_limited {
            eprintln!("{name} may ask for network approvals again");
        } else {
            eprintln!("{name} was not rate limited");
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn print_pending(approval: &PendingApproval) {
    let transport = format!("{:?}", approval.transport).to_lowercase();
    let names = if approval.unresolved {
        "this sandbox never resolved this address".to_owned()
    } else {
        approval.names.join(", ")
    };
    let origin = approval
        .origin
        .as_ref()
        .map(|o| format!("  {o}"))
        .unwrap_or_default();
    println!(
        "{:>4}  {}  {}:{} ({transport})  {names}{origin}  [{}s]",
        approval.id, approval.sandbox, approval.address, approval.port, approval.age_secs
    );
}

fn project_sandbox(ctx: &Context) -> Result<String> {
    let discovery = discover(&ctx.mise, &ctx.cwd)?;
    Ok(sandbox_name(
        &discovery.kitchen_file,
        &discovery.kitchen_dir,
    ))
}

/// `net allow|deny <rule>`: put the rule into the kitchen file's list,
/// taking it out of the opposite list.
pub(super) fn set_rule(
    ctx: &Context,
    list: RuleList,
    rule: &str,
    global: bool,
) -> Result<ExitCode> {
    if global {
        bail!("--global rules need the egress broker, which is not implemented yet");
    }
    let rule: HostPattern = rule
        .parse()
        .with_context(|| format!("invalid rule {rule:?}"))?;
    let discovery = discover(&ctx.mise, &ctx.cwd)?;
    let change = edit::set_network_rule(&ctx.home, &discovery.kitchen_file, list, &rule)?;

    if !ctx.quiet {
        let file = discovery.kitchen_file.display();
        if change.removed_from_other {
            println!("removed `{rule}` from `{}` in {file}", list.other().key());
        }
        if change.added {
            println!("added `{rule}` to `{}` in {file}", list.key());
        } else {
            println!("`{rule}` is already in `{}` in {file}", list.key());
        }
    }
    Ok(ExitCode::SUCCESS)
}
