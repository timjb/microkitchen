use std::process::ExitCode;

use clap::Parser;
use microkitchen::cli::{self, Cli};

#[tokio::main]
async fn main() -> ExitCode {
    let args = Cli::parse();
    cli::init_logging(args.verbose, args.quiet);
    match cli::run(args).await {
        Ok(code) => code,
        Err(error) => {
            cli::print_error(&error);
            ExitCode::FAILURE
        }
    }
}
