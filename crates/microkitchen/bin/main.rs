use std::process::ExitCode;

use clap::Parser;
use microkitchen::cli::{self, Cli};

fn main() -> ExitCode {
    let args = Cli::parse();
    cli::init_logging(args.verbose, args.quiet);
    match cli::run(args) {
        Ok(code) => code,
        Err(error) => {
            cli::print_error(&error);
            ExitCode::FAILURE
        }
    }
}
