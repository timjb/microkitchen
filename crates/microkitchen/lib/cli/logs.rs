//! `microkitchen logs [--bootstrap | --broker | --sandbox]`.

use std::io::Write;
use std::path::Path;
use std::process::ExitCode;

use anyhow::{Context as _, Result, bail};
use microsandbox::sandbox::LogOptions;

use super::Context;
use crate::config::discover::discover;
use crate::sandbox::lifecycle;
use crate::sandbox::naming::sandbox_name;

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

/// The bootstrap log of the project's sandbox by default.
pub(super) async fn run(ctx: &Context, broker: bool, sandbox: bool) -> Result<ExitCode> {
    if broker {
        let code = print_file(
            &ctx.home.logs_dir().join("broker.log"),
            "the egress broker has not written a log yet",
        )?;
        if !ctx.quiet {
            eprintln!(
                "(every verdict is also in {})",
                ctx.home.broker_dir().join("audit.log").display()
            );
        }
        return Ok(code);
    }

    let discovery = discover(&ctx.mise, &ctx.cwd)?;
    if sandbox {
        let Some(handle) = lifecycle::find(&discovery.kitchen_file).await? else {
            bail!(
                "there is no sandbox for {} yet; run `microkitchen up`",
                discovery.kitchen_file.display()
            );
        };
        let entries = handle
            .logs(&LogOptions::default())
            .await
            .with_context(|| format!("reading the logs of {}", handle.name()))?;
        let mut out = std::io::stdout().lock();
        for entry in entries {
            out.write_all(&entry.data)?;
        }
        return Ok(ExitCode::SUCCESS);
    }

    let name = sandbox_name(&discovery.kitchen_file, &discovery.kitchen_dir);
    print_file(
        &ctx.home.logs_dir().join(&name).join("bootstrap.log"),
        &format!("{name} has not been bootstrapped yet; run `microkitchen up`"),
    )
}

fn print_file(path: &Path, missing: &str) -> Result<ExitCode> {
    match std::fs::read(path) {
        Ok(bytes) => {
            std::io::stdout().write_all(&bytes)?;
            Ok(ExitCode::SUCCESS)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            eprintln!("{missing}");
            Ok(ExitCode::FAILURE)
        }
        Err(error) => Err(error).with_context(|| format!("reading {}", path.display())),
    }
}
