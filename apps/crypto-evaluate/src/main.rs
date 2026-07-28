mod capacity;

use std::process::ExitCode;

use clap::{Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "crypto-evaluate")]
struct Arguments {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Capacity(capacity::CapacityArguments),
}

fn main() -> ExitCode {
    let arguments = Arguments::parse();
    let result = match arguments.command {
        Command::Capacity(arguments) => capacity::run(arguments),
    };
    match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("error[crypto-evaluate]: {error}");
            ExitCode::FAILURE
        }
    }
}
