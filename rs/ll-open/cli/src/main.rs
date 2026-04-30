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
            // KNOWN scale limitation: arena_size_mib defaults to 64
            // (see Cmd::Daemon { arena_size_mib, default_value_t = 64 }).
            // For registry-scale ingest (helm/charts: 1.1 GB output db
            // for 4.5k YAML files) the user must pass --arena-size-mib
            // explicitly or `op_load` will error with "exceeds arena
            // buffer capacity". A future bump should be deliberate;
            // pinned in tests::default_arena_size_is_64.
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

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn default_arena_size_is_64_mib_known_scale_limit() {
        // KNOWN scale limit pinned. The CLI's --arena-size-mib defaults
        // to 64 MiB (~32 MiB per buffer after header). At registry-
        // scale (helm/charts 1.1 GB ingest, 50k-file Aports clones) the
        // default is too small and op_load errors with "exceeds arena
        // buffer capacity". This pin makes a future default-bump a
        // deliberate, visible behavior change rather than a silent
        // shift. Update this test alongside any default_value_t change.
        let cli = Cli::try_parse_from(["leyline", "daemon"]).unwrap();
        match cli.command {
            Cmd::Daemon { arena_size_mib, .. } => {
                assert_eq!(
                    arena_size_mib, 64,
                    "default arena size pinned at 64 MiB (registry-scale workflows must pass --arena-size-mib explicitly)",
                );
            }
            _ => panic!("expected Daemon variant"),
        }
    }

    #[test]
    fn default_nfs_port_is_zero() {
        // 0 = "auto-assign". Pin so a refactor doesn't silently bind
        // to a fixed port and break parallel daemon launches.
        let cli = Cli::try_parse_from(["leyline", "daemon"]).unwrap();
        match cli.command {
            Cmd::Daemon { nfs_port, .. } => {
                assert_eq!(nfs_port, 0, "nfs_port=0 means auto-assign");
            }
            _ => panic!("expected Daemon variant"),
        }
    }
}
