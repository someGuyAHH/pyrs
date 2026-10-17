mod bundle;
mod cli;
mod commands;
mod envs;
mod hash;
mod installer;
mod interpreter;
mod manifest;
mod ui;
mod wheel;

use clap::Parser;

use crate::cli::{Cli, Command};

fn main() {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Build(args) => commands::build::run(args, cli.quiet).map(|()| 0),
        Command::Run(args) => commands::run::run(args, cli.quiet),
        Command::Inspect(args) => commands::inspect::run(args).map(|()| 0),
    };
    // Exit only after `run` has returned, so the temporary workspace has been cleaned up.
    match result {
        Ok(code) => std::process::exit(code),
        Err(err) => {
            eprintln!("pyrs: error: {err:#}");
            std::process::exit(2);
        }
    }
}
