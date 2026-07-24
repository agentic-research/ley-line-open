//! Shared CLI library for ley-line (open edition).
//!
//! Exports a `Commands` enum that can be used standalone or flattened into
//! a wrapper enum by downstream binaries (e.g. the private `leyline` binary
//! that adds `daemon`, `embed`, `send`, etc.).

#[cfg(feature = "cdc")]
pub mod cmd_cdc;
pub mod cmd_daemon;
#[cfg(feature = "lsp")]
pub mod cmd_doctor;
pub mod cmd_inspect;
pub mod cmd_load;
#[cfg(feature = "lsp")]
pub mod cmd_lsp;
pub mod cmd_parse;
pub mod cmd_serve;
pub mod cmd_splice;
pub mod daemon;
pub mod topology_pass;
pub mod walk;

use std::path::PathBuf;

use anyhow::Result;
use clap::Subcommand;

/// Edition tag for this build of the CLI library.
pub const EDITION: &str = "open";

/// CDC storage-administration subcommands.
#[derive(Debug, Subcommand)]
pub enum CdcCommands {
    /// Populate or resume the private chunk-backed content index.
    Enable {
        /// SQLite projection to activate in place.
        #[arg(long)]
        db: PathBuf,

        /// Authoritative rows loaded into memory per query page.
        #[arg(long, default_value_t = 256)]
        batch_size: usize,

        /// Emit the activation report as one JSON object.
        #[arg(long)]
        json: bool,
    },

    /// Delete chunks unreachable from every committed manifest.
    Gc {
        /// Existing SQLite projection to collect in place.
        #[arg(long)]
        db: PathBuf,

        /// Report unreachable rows and bytes without deleting them.
        #[arg(long)]
        dry_run: bool,

        /// Emit the GC report as one JSON object.
        #[arg(long)]
        json: bool,
    },
}

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

    /// Administer content-defined chunk storage.
    Cdc {
        #[command(subcommand)]
        command: CdcCommands,
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

    /// Spawn a language server, collect symbols + diagnostics, and write a .db.
    #[cfg(feature = "lsp")]
    Lsp {
        /// LSP server command (e.g. "gopls", "pyright-langserver").
        #[arg(long)]
        server: String,

        /// Arguments passed to the LSP server.
        #[arg(long, num_args = 0.., allow_hyphen_values = true)]
        server_args: Vec<String>,

        /// Source file to analyse.
        #[arg(long)]
        input: PathBuf,

        /// Output .db path.
        #[arg(long)]
        output: PathBuf,

        /// Existing .db to merge LSP data into (enables merge mode).
        #[arg(long)]
        merge_db: Option<PathBuf>,

        /// Override the language ID sent to the server (inferred from extension if omitted).
        #[arg(long)]
        language_id: Option<String>,
    },

    /// Create an arena, mount it via NFS or FUSE, and wait for shutdown.
    Serve {
        /// Path to the arena file.
        #[arg(long, default_value = "./leyline.arena")]
        arena: PathBuf,

        /// Arena size in MiB.
        #[arg(long, default_value_t = 64)]
        arena_size_mib: u64,

        /// Path to the controller (.ctrl) file. Defaults to arena path with .ctrl extension.
        #[arg(long)]
        control: Option<PathBuf>,

        /// Directory to mount the filesystem at.
        #[arg(long)]
        mount: PathBuf,

        /// Filesystem backend: "nfs" or "fuse".
        #[arg(long, default_value_t = cmd_serve::default_backend())]
        backend: String,

        /// NFS listen port (0 = auto-assign).
        #[arg(long, default_value_t = 0)]
        nfs_port: u16,

        /// Default language for validation of extensionless files (e.g. "go", "py").
        #[arg(long)]
        language: Option<String>,

        /// Timeout before automatic shutdown (e.g. "30s", "5m", "2h").
        #[arg(long)]
        timeout: Option<String>,
    },

    /// Check environment: which bundled LSP language servers are on
    /// PATH and which languages will fall back to tree-sitter-only.
    /// Exit code 0 if every bundled language has its server; nonzero if
    /// any are missing (unless `--allow-missing` is passed).
    #[cfg(feature = "lsp")]
    Doctor {
        /// Emit machine-readable JSON instead of the human table.
        /// Useful for cloister / mache install scripts that want to
        /// check + warn without parsing text.
        #[arg(long)]
        json: bool,

        /// Exit 0 even when some servers are missing (for CI / cloister
        /// install scripts that want to WARN rather than fail).
        #[arg(long)]
        allow_missing: bool,
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
        } => cmd_inspect::cmd_inspect(&id, &arena, control_path.as_deref(), query.as_deref()),
        Commands::Load { db, control } => cmd_load::cmd_load(&db, &control),
        Commands::Cdc { command } => {
            #[cfg(feature = "cdc")]
            {
                match command {
                    CdcCommands::Enable {
                        db,
                        batch_size,
                        json,
                    } => cmd_cdc::cmd_cdc_enable(&db, batch_size, json),
                    CdcCommands::Gc { db, dry_run, json } => {
                        cmd_cdc::cmd_cdc_gc(&db, dry_run, json)
                    }
                }
            }
            #[cfg(not(feature = "cdc"))]
            {
                let subcommand = match command {
                    CdcCommands::Enable { .. } => "cdc enable",
                    CdcCommands::Gc { .. } => "cdc gc",
                };
                anyhow::bail!(
                    "{subcommand} requires the 'cdc' feature (compile with --features cdc)"
                )
            }
        }
        Commands::Splice { db, node, text } => cmd_splice::cmd_splice(&db, &node, &text),
        #[cfg(feature = "lsp")]
        Commands::Lsp {
            server,
            server_args,
            input,
            output,
            merge_db,
            language_id,
        } => {
            cmd_lsp::cmd_lsp(
                &server,
                &server_args,
                &input,
                &output,
                merge_db.as_deref(),
                language_id.as_deref(),
            )
            .await
        }
        Commands::Serve {
            arena,
            arena_size_mib,
            control,
            mount,
            backend,
            nfs_port,
            language,
            timeout,
        } => {
            #[cfg(feature = "mount")]
            {
                cmd_serve::cmd_serve(
                    &arena,
                    arena_size_mib,
                    control.as_deref(),
                    &mount,
                    &backend,
                    nfs_port,
                    language.as_deref(),
                    timeout.as_deref(),
                )
                .await
            }
            #[cfg(not(feature = "mount"))]
            {
                let _ = (
                    arena,
                    arena_size_mib,
                    control,
                    mount,
                    backend,
                    nfs_port,
                    language,
                    timeout,
                );
                anyhow::bail!("serve requires the 'mount' feature (compile with --features mount)")
            }
        }
        #[cfg(feature = "lsp")]
        Commands::Doctor {
            json,
            allow_missing,
        } => cmd_doctor::run_doctor(json, allow_missing),
    }
}
