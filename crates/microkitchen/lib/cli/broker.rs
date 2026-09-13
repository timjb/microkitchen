//! `microkitchen broker start|stop|status|run`.

use std::process::ExitCode;
use std::time::Duration;

use anyhow::Result;

use super::{BrokerCommand, Context};
use crate::broker::client::BrokerClient;
use crate::broker::daemon;

//--------------------------------------------------------------------------------------------------
// Functions
//--------------------------------------------------------------------------------------------------

pub(super) async fn run(ctx: &Context, command: BrokerCommand) -> Result<ExitCode> {
    match command {
        BrokerCommand::Run => daemon::run(ctx.home.clone()).await?,
        BrokerCommand::Start => {
            BrokerClient::ensure_running(&ctx.home).await?;
            say(ctx, "the egress broker is running");
        }
        BrokerCommand::Stop => {
            let client = BrokerClient::new(&ctx.home);
            if client.is_running().await {
                client.shutdown().await?;
                for _ in 0..50 {
                    if !client.is_running().await {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                say(ctx, "the egress broker stopped");
            } else {
                say(ctx, "the egress broker is not running");
            }
        }
        BrokerCommand::Status => {
            let client = BrokerClient::new(&ctx.home);
            let running = client.is_running().await;
            let sandboxes = if running {
                client.list().await?
            } else {
                Vec::new()
            };
            if ctx.json {
                let status = serde_json::json!({ "running": running, "sandboxes": sandboxes });
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else if !running {
                println!("the egress broker is not running");
            } else {
                println!(
                    "the egress broker is running (log: {})",
                    client.log_file().display()
                );
                for s in &sandboxes {
                    println!(
                        "  {:<40} {:<8} dns 127.0.0.1:{:<6} proxy 127.0.0.1:{:<6} {} bindings",
                        s.name,
                        format!("{:?}", s.mode).to_lowercase(),
                        s.resolver_port,
                        s.proxy_port,
                        s.bindings
                    );
                }
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn say(ctx: &Context, message: &str) {
    if !ctx.quiet {
        eprintln!("{message}");
    }
}
