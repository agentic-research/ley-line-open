//! Thin binary wrapper for ley-line (open edition).
//!
//! Shared commands (parse, splice, serve, load, inspect, lsp) live in
//! `leyline_cli_lib::Commands`. The `Daemon` variant is defined here so
//! ley-line (private) can define its own extended Daemon without conflict.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use leyline_cli_lib::cmd_serve;

#[derive(Subcommand)]
enum Cmd {
    #[command(flatten)]
    Shared(leyline_cli_lib::Commands),

    /// Run the daemon: arena + mount + UDS socket for coordination.
    Daemon {
        /// Path to the arena file. Defaults to ~/.mache/default.arena.
        #[arg(long)]
        arena: Option<PathBuf>,

        /// Arena size in MiB.
        #[arg(long, default_value_t = 64)]
        arena_size_mib: u64,

        /// Path to the controller (.ctrl) file.
        #[arg(long)]
        control: Option<PathBuf>,

        /// Directory to mount the filesystem at. If omitted, no mount (headless mode).
        #[arg(long)]
        mount: Option<PathBuf>,

        /// Filesystem backend: "nfs" or "fuse".
        #[arg(long, default_value_t = cmd_serve::default_backend())]
        backend: String,

        /// NFS listen port (0 = auto-assign).
        #[arg(long, default_value_t = 0)]
        nfs_port: u16,

        /// Default language for validation.
        #[arg(long)]
        language: Option<String>,

        /// Timeout before automatic shutdown (e.g. "30s", "5m", "2h").
        #[arg(long)]
        timeout: Option<String>,

        /// Source directory to parse on startup. Creates .db and loads into arena.
        /// If mache is on PATH, also spawns mache as a managed child process.
        #[arg(long)]
        source: Option<PathBuf>,

        /// Expose the daemon's ops as MCP tools over HTTP on this port.
        /// Same dispatch table as the UDS socket — POST /mcp speaks JSON-RPC,
        /// `tools/list` and `tools/call` are wired. cloister gateway routes
        /// `lsp_*` calls here.
        #[arg(long)]
        mcp_port: Option<u16>,
    },
}

#[derive(Parser)]
#[command(
    name = "leyline",
    about = "Pre-bake source code into a .db for mache"
)]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let version: &'static str = Box::leak(
        format!("{} ({})", env!("CARGO_PKG_VERSION"), leyline_cli_lib::EDITION).into_boxed_str(),
    );
    let matches = Cli::command().version(version).get_matches();
    let cli = Cli::from_arg_matches(&matches)?;

    match cli.command {
        Cmd::Shared(cmd) => leyline_cli_lib::run(cmd).await,
        Cmd::Daemon {
            arena,
            arena_size_mib,
            control,
            mount,
            backend,
            nfs_port,
            language,
            timeout,
            source,
            mcp_port,
        } => {
            // Default arena/ctrl to ~/.mache/ so mache's path containment check passes.
            let mache_dir = dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".mache");
            let _ = std::fs::create_dir_all(&mache_dir);
            let arena = arena.unwrap_or_else(|| mache_dir.join("default.arena"));

            leyline_cli_lib::cmd_daemon::run_daemon(
                &arena,
                arena_size_mib,
                control.as_deref(),
                mount.as_deref(),
                &backend,
                nfs_port,
                language.as_deref(),
                timeout.as_deref(),
                Arc::new(leyline_cli_lib::daemon::NoExt),
                source.as_deref(),
                mcp_port,
            )
            .await
        }
    }
}
