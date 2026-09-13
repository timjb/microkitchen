//! `microkitchen net …`: rule edits and the headless approval surface.

use std::process::ExitCode;

use anyhow::{Context as _, Result};

use super::Context;
use crate::broker::client::BrokerClient;
use crate::broker::decision::Rules;
use crate::broker::protocol::{Answer, Mode, PendingApproval};
use crate::broker::rules::{BUILTIN_ALLOW, parse_global_rules, parse_rules};
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

/// `net rules`: the persistent rules the broker applies to the project's
/// sandbox (temporary allows are held by the broker and not listed).
pub(super) fn rules(ctx: &Context) -> Result<ExitCode> {
    let discovery = discover(&ctx.mise, &ctx.cwd)?;
    let file = &discovery.kitchen_file;
    let text =
        std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    let rules = parse_rules(file, &text)
        .with_context(|| format!("{} is not valid TOML", file.display()))?;
    let builtin: Vec<HostPattern> = BUILTIN_ALLOW
        .iter()
        .map(|host| host.parse().expect("built-in patterns are valid"))
        .collect();
    let global_file = ctx.home.rules_file();
    let global = match std::fs::read_to_string(&global_file) {
        Ok(text) => parse_global_rules(&text)
            .with_context(|| format!("{} is not valid", global_file.display()))?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Rules::default(),
        Err(error) => {
            return Err(error).with_context(|| format!("reading {}", global_file.display()));
        }
    };
    let names = |list: &[HostPattern]| list.iter().map(ToString::to_string).collect::<Vec<_>>();

    if ctx.json {
        let kitchen_allow: Vec<HostPattern> = rules
            .allow
            .iter()
            .filter(|r| !builtin.contains(r))
            .cloned()
            .collect();
        let report = serde_json::json!({
            "kitchen_file": file,
            "allow": names(&kitchen_allow),
            "deny": names(&rules.deny),
            "builtin_allow": names(&builtin),
            "global_file": global_file,
            "global_allow": names(&global.allow),
            "global_deny": names(&global.deny),
        });
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(ExitCode::SUCCESS);
    }
    let print = |title: String, rules: &Rules| {
        println!("{title}");
        for (key, list) in [("allow", &rules.allow), ("deny", &rules.deny)] {
            let entries: Vec<String> = list
                .iter()
                .map(|rule| {
                    if key == "allow" && builtin.contains(rule) {
                        format!("{rule} (built in)")
                    } else {
                        rule.to_string()
                    }
                })
                .collect();
            let entries = if entries.is_empty() {
                "(none)".to_owned()
            } else {
                entries.join(", ")
            };
            println!("  {key:<6} {entries}");
        }
    };
    println!("deny wins over allow within a file; the kitchen file is consulted first");
    print(format!("{}:", file.display()), &rules);
    print(
        format!("{} (every sandbox):", global_file.display()),
        &global,
    );
    Ok(ExitCode::SUCCESS)
}

/// `net revoke <rule> [--global]`: take a rule out of the allow and deny
/// lists of the kitchen file (or `rules.toml`). The broker applies the change
/// to the next flow.
pub(super) fn revoke(ctx: &Context, rule: &str, global: bool) -> Result<ExitCode> {
    let rule: HostPattern = rule
        .parse()
        .with_context(|| format!("invalid rule {rule:?}"))?;
    let (removed, path) = if global {
        (
            edit::remove_global_rule(&ctx.home, &rule)?,
            ctx.home.rules_file(),
        )
    } else {
        let discovery = discover(&ctx.mise, &ctx.cwd)?;
        let removed = edit::remove_network_rule(&ctx.home, &discovery.kitchen_file, &rule)?;
        (removed, discovery.kitchen_file)
    };
    let file = path.display();
    if removed.is_empty() {
        let builtin = BUILTIN_ALLOW
            .iter()
            .any(|host| host.parse::<HostPattern>().is_ok_and(|b| b == rule));
        if builtin {
            eprintln!("`{rule}` is allowed built in; block it with `microkitchen net deny {rule}`");
        } else {
            eprintln!("`{rule}` is in neither `allow` nor `deny` in {file}");
        }
        return Ok(ExitCode::FAILURE);
    }
    if !ctx.quiet {
        for list in removed {
            println!("removed `{rule}` from `{}` in {file}", list.key());
        }
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
    let rule: HostPattern = rule
        .parse()
        .with_context(|| format!("invalid rule {rule:?}"))?;
    let (change, path) = if global {
        let change = edit::set_global_rule(&ctx.home, list, &rule)?;
        (change, ctx.home.rules_file())
    } else {
        let discovery = discover(&ctx.mise, &ctx.cwd)?;
        let change = edit::set_network_rule(&ctx.home, &discovery.kitchen_file, list, &rule)?;
        (change, discovery.kitchen_file)
    };

    if !ctx.quiet {
        let file = path.display();
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
