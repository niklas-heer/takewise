//! `takewise`: record narration over a film; the command line for agents.

mod calibrate;
mod cli;
mod cli_install;
mod film;
mod output;
mod window;

use std::process::ExitCode;

use clap::Parser as _;

fn main() -> ExitCode {
    let json = std::env::args().any(|arg| arg == "--json");
    let args = match cli::Cli::try_parse() {
        Ok(args) => args,
        Err(err) if !err.use_stderr() => {
            // --help and --version
            let _ = err.print();
            return ExitCode::SUCCESS;
        }
        Err(err) if json => {
            output::print_error(true, "usage", &err.render().to_string());
            return ExitCode::from(2);
        }
        Err(err) => {
            let _ = err.print();
            return ExitCode::from(2);
        }
    };
    let json = args.json;
    match cli::run(args) {
        Ok(outcome) => outcome.print(json),
        Err(err) => {
            output::print_error(json, err.code(), &err.to_string());
            ExitCode::from(2)
        }
    }
}
