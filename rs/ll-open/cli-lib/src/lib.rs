//! Shared CLI library for ley-line (open edition).
//!
//! Exports a `Commands` enum that can be used standalone or flattened into
//! a wrapper enum by downstream binaries (e.g. the private `leyline` binary
//! that adds `daemon`, `embed`, `send`, etc.).

pub mod cmd_parse;

use std::path::PathBuf;

use anyhow::Result;
use clap::Subcommand;

/// Edition tag for this build of the CLI library.
pub const EDITION: &str = "open";

/// Subcommands provided by ley-line open.
#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Parse a source directory into a .db with nodes + _ast + _source tables.
    Parse {
        /// Source directory to parse.
        source: PathBuf,

        /// Output database path.
        #[arg(short, long, default_value = "output.db")]
        output: PathBuf,

        /// Only parse files matching this language (go, python, etc.).
        /// If omitted, all recognized languages are parsed.
        #[arg(short, long)]
        lang: Option<String>,
    },
}

/// Dispatch a command to its implementation.
pub async fn run(cmd: Commands) -> Result<()> {
    match cmd {
        Commands::Parse {
            source,
            output,
            lang,
        } => cmd_parse::cmd_parse(&source, &output, lang.as_deref()),
    }
}
