//! Shared CLI library for ley-line (open edition).
//!
//! Exports a `Commands` enum that can be used standalone or flattened into
//! a wrapper enum by downstream binaries (e.g. the private `leyline` binary
//! that adds `daemon`, `embed`, `send`, etc.).

pub mod cmd_inspect;
pub mod cmd_load;
pub mod cmd_parse;
pub mod cmd_splice;

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

    /// Load a .db file into a ley-line arena.
    Load {
        /// Path to the .db file to load.
        #[arg(long)]
        db: PathBuf,

        /// Path to the controller (.ctrl) file.
        #[arg(long)]
        control: PathBuf,
    },

    /// Inspect the arena's active SQLite buffer — look up a node or run SQL.
    Inspect {
        /// Node ID to look up (positional).
        id: String,

        /// Path to the arena file.
        #[arg(long, default_value = "./leyline.arena")]
        arena: PathBuf,

        /// Path to the controller (.ctrl) file. If omitted, uses arena path directly.
        #[arg(long)]
        control_path: Option<PathBuf>,

        /// Arbitrary SQL query. If provided, runs this instead of node lookup.
        #[arg(long)]
        query: Option<String>,
    },

    /// Edit an AST node's text in a .db file (splice + reproject).
    Splice {
        /// Path to the .db file.
        #[arg(long)]
        db: PathBuf,

        /// Node ID to splice (e.g. "main.go/function_declaration/block").
        #[arg(long)]
        node: String,

        /// New text to replace the node's content.
        #[arg(long)]
        text: String,
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
        Commands::Inspect {
            id,
            arena,
            control_path,
            query,
        } => cmd_inspect::cmd_inspect(
            &id,
            &arena,
            control_path.as_deref(),
            query.as_deref(),
        ),
        Commands::Load { db, control } => cmd_load::cmd_load(&db, &control),
        Commands::Splice { db, node, text } => cmd_splice::cmd_splice(&db, &node, &text),
    }
}
