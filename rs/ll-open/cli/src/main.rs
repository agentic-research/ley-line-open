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

        /// Bind address for the MCP HTTP transport. Defaults to 127.0.0.1.
        ///
        /// Container deployments need 0.0.0.0 so docker `-p HOST_PORT:8384`
        /// port-forwarding can reach the listener (loopback-only binds
        /// are unreachable from the host's docker proxy).
        ///
        /// SECURITY: passing 0.0.0.0 (or any non-loopback address)
        /// requires `--mcp-allow-public` as a deliberate opt-in. The MCP
        /// wire has no auth — it's intended for cloister-mediated
        /// localhost or attested peers only. The two-flag gate prevents
        /// fat-fingering `--mcp-bind 0.0.0.0` on a dev box and quietly
        /// exposing the daemon to every interface. In a container,
        /// 0.0.0.0 binds inside the container's netns; combine with a
        /// loopback host publish such as `docker run -p
        /// 127.0.0.1:18384:8384` to keep host-side exposure on loopback.
        #[arg(long)]
        mcp_bind: Option<std::net::IpAddr>,

        /// Required when `--mcp-bind` is set to a non-loopback address.
        /// Acts as a "yes I really mean to expose MCP off-loopback"
        /// confirmation. See `--mcp-bind` for the security context.
        ///
        /// Container deployments pass this in the image CMD because the
        /// container-side 0.0.0.0 is legitimate plumbing. Outside
        /// containers, only pass this when you control the firewall and
        /// understand the LAN exposure surface. The shared-secret token
        /// gate (ADR-0022) closes the same-machine surface but does not
        /// substitute for network-level controls when binding to a
        /// public address. Bead `ley-line-open-b7dd03`.
        #[arg(long, default_value_t = false)]
        mcp_allow_public: bool,

        /// Disable the shared-secret token gate on `/mcp` (ADR-0022,
        /// bead `ley-line-open-b885d1`). Default behavior: the daemon
        /// auto-generates a 32-byte token at
        /// `~/.local/share/leyline/daemon.token` (0600) and rejects
        /// requests without `x-leyline-token: <hex>`. Pass this flag
        /// only for pre-provisioned containers / CI smokes where no
        /// token file is mounted and the perimeter is enforced
        /// elsewhere. Logged as a warning at startup.
        #[arg(long, default_value_t = false)]
        mcp_no_auth: bool,
    },
}

#[derive(Parser)]
#[command(name = "leyline", about = "Pre-bake source code into a .db for mache")]
struct Cli {
    #[command(subcommand)]
    command: Cmd,
}

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let version: &'static str = Box::leak(
        format!(
            "{} ({})",
            env!("CARGO_PKG_VERSION"),
            leyline_cli_lib::EDITION
        )
        .into_boxed_str(),
    );
    let matches = Cli::command().version(version).get_matches();
    let cli = Cli::from_arg_matches(&matches)?;

    match cli.command {
        Cmd::Shared(cmd) => {
            // Parse is a fire-and-forget terminal command: after
            // cmd_parse returns Ok the work is on disk and the user
            // is staring at the wall clock. Skipping the tokio runtime
            // drop + SQLite Connection drop + libc atexit handlers
            // recovers ~125 ms of pure post-work user-visible wall
            // time on macOS. We use `libc::_exit` (not
            // `std::process::exit`) because std's variant still runs
            // libc cleanup; `_exit` is the immediate kill syscall.
            // Safe: `synchronous=OFF` + DELETE-mode SQLite means no
            // owed fsync, the segments + .db + head + indexes are on
            // disk, and we've already flushed stderr via eprintln in
            // cmd_parse. Other shared commands fall through to the
            // normal return path. See bead `ley-line-open-cbbedf`.
            let is_parse = matches!(cmd, leyline_cli_lib::Commands::Parse { .. });
            let r = leyline_cli_lib::run(cmd).await;
            if is_parse && r.is_ok() {
                // Flush stdout/stderr explicitly before _exit since
                // _exit doesn't flush stdio.
                use std::io::Write;
                let _ = std::io::stdout().flush();
                let _ = std::io::stderr().flush();
                // SAFETY: libc::_exit is a syscall wrapper; it
                // unconditionally exits the process with the given
                // status. No invariants needed.
                unsafe { libc::_exit(0) };
            }
            r
        }
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
            mcp_bind,
            mcp_allow_public,
            mcp_no_auth,
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
                mcp_bind,
                mcp_allow_public,
                mcp_no_auth,
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
