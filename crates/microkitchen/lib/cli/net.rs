//! `microkitchen net …`: rule edits. Approvals arrive with the broker.

use std::process::ExitCode;

use anyhow::{Context as _, Result, bail};

use super::Context;
use crate::config::discover::discover;
use crate::config::edit::{self, RuleList};
use crate::config::hostpat::HostPattern;

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

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
