//! Core library for the `mvault` command-line tool.

use std::io::IsTerminal as _;

use clap::Parser as _;

pub mod auth;
pub mod cli;
pub mod download;
pub mod error;
mod fsutil;
pub mod hash;
pub mod mirror;
pub mod source;
pub mod store;
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod testutil;
pub mod upstream;

/// Runs the application with the arguments of the process.
///
/// # Errors
///
/// Returns the error of the command that was run.
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let cli = cli::Cli::parse();
    let mut err = std::io::stderr();
    let live = err.is_terminal();
    let mut context = cli::Context {
        env: &|name| std::env::var(name).ok(),
        base_url: &|host| format!("https://{host}"),
        http: download::Client::default(),
        out: &mut std::io::stdout(),
        err: &mut err,
        live,
    };
    Ok(cli::execute(cli, &mut context)?)
}
