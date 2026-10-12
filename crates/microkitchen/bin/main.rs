use std::process::ExitCode;

use clap::Parser;
use microkitchen::cli::{self, Cli};

fn main() -> ExitCode {
    let args = Cli::parse();
    // The broker daemon logs its activity by default; commands only warnings.
    let verbose = if args.is_broker_daemon() {
        args.verbose.max(1)
    } else {
        args.verbose
    };
    cli::init_logging(verbose, args.quiet);

    // Before any thread exists: microsandbox reads its home and the proxy
    // password from the environment.
    if let Err(error) =
        cli::export_microsandbox_home(&args).and_then(|()| cli::export_proxy_secret(&args))
    {
        cli::print_error(&error);
        return ExitCode::FAILURE;
    }

    let runtime = match tokio::runtime::Runtime::new() {
        Ok(runtime) => runtime,
        Err(error) => {
            cli::print_error(&anyhow::Error::from(error));
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(cli::run(args)) {
        Ok(code) => code,
        Err(error) => {
            cli::print_error(&error);
            ExitCode::FAILURE
        }
    }
}
