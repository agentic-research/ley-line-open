//! Thin binary wrapper for ley-line (open edition).
//!
//! All real logic lives in `leyline_cli_lib`. This binary just parses args
//! and dispatches to the library.

use anyhow::Result;
use clap::{CommandFactory, FromArgMatches, Parser};
use leyline_cli_lib::Commands;

#[derive(Parser)]
#[command(
    name = "leyline",
    about = "Pre-bake source code into a .db for mache"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[tokio::main]
async fn main() -> Result<()> {
    let version: &'static str = Box::leak(
        format!("{} ({})", env!("CARGO_PKG_VERSION"), leyline_cli_lib::EDITION).into_boxed_str(),
    );
    let matches = Cli::command().version(version).get_matches();
    let cli = Cli::from_arg_matches(&matches)?;
    leyline_cli_lib::run(cli.command).await
}
