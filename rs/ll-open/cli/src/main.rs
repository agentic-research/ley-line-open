//! Thin binary wrapper for ley-line (open edition).
//!
//! Shared commands (parse, splice, serve, load, inspect, lsp) live in
//! `leyline_cli_lib::Commands`. The `Daemon` variant is defined here so
//! ley-line (private) can define its own extended Daemon without conflict.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand, ValueEnum};
use leyline_cli_lib::cmd_serve;

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum ExecutionBackendKind {
    Native,
    #[value(name = "micro-vm")]
    MicroVm,
}

#[derive(Subcommand)]
enum Cmd {
    #[command(flatten)]
    Shared(leyline_cli_lib::Commands),

    /// Run the first-party native-nono execution daemon over the control UDS.
    /// All runtime resources are explicit; this command never invokes
    /// krunvm, Taskfile, or a repository helper.
    ExecutionDaemon {
        /// Path to the arena file. Defaults to ~/.mache/execution.arena.
        #[arg(long)]
        arena: Option<PathBuf>,

        /// Path to the controller (.ctrl) file.
        #[arg(long)]
        control: Option<PathBuf>,

        /// Existing immutable content-addressed rootfs directory.
        #[arg(long)]
        cas_root: PathBuf,

        /// Parent directory for ephemeral per-run rootfs volumes.
        #[arg(long)]
        run_root: PathBuf,

        /// First-party leyline-native-worker executable.
        #[arg(long)]
        worker: PathBuf,

        /// Trusted artifact/workspace-to-rootfs catalog JSON document.
        #[arg(long)]
        catalog: PathBuf,

        /// Backend to supervise: native nono or embedded libkrun microVM.
        #[arg(long, value_enum, default_value_t = ExecutionBackendKind::Native)]
        backend: ExecutionBackendKind,

        /// Embedded libkrun shared library (required for `--backend micro-vm`).
        #[arg(long)]
        libkrun: Option<PathBuf>,

        /// Device paths explicitly granted to the libkrun worker.
        #[arg(long = "device")]
        devices: Vec<PathBuf>,

        /// Read-only runtime library/resource paths passed to nono.
        #[arg(long = "runtime-file")]
        runtime_files: Vec<PathBuf>,

        /// Maximum time to wait for the worker readiness handshake.
        #[arg(long, default_value_t = 5_000)]
        ready_timeout_ms: u64,

        /// Activate or resume the private CDC index.
        #[arg(long, default_value_t = false)]
        cdc: bool,

        /// Explicitly allow metadata-only evidence validation for local
        /// fixtures. Production Cloister integrations must provide a real
        /// Signet/NotMe/Interlace verifier instead.
        #[arg(long, default_value_t = false)]
        allow_unverified_evidence: bool,
    },

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
        /// auto-generates a 32-byte token at the platform data dir
        /// (XDG `$XDG_DATA_HOME/leyline/daemon.token` on Linux —
        /// typically `~/.local/share/leyline/daemon.token`; or
        /// `~/Library/Application Support/leyline/daemon.token` on
        /// macOS), mode `0600`, and rejects requests without
        /// `x-leyline-token: <hex>`. Pass this flag only for
        /// pre-provisioned containers / CI smokes where no token file
        /// is mounted and the perimeter is enforced elsewhere. Logged
        /// as a warning at startup.
        #[arg(long, default_value_t = false)]
        mcp_no_auth: bool,

        /// Serve MCP over a Unix domain socket at this path (bead
        /// `ley-line-open-6569de`).
        ///
        /// Independent of `--mcp-port`; either, both, or neither. The
        /// socket is created mode `0600` — owner-only — and carries the
        /// SAME ADR-0022 token gate as the TCP listener, so
        /// `--mcp-no-auth` is not required to use it.
        ///
        /// This is what an attested caller dials. `--control` carries the
        /// OPS protocol, not MCP; cloister runs in workerd and reaches
        /// bundles over AF_UNIX through notme-proxy (cloister ADR-0005),
        /// the same way rosary and mache are already reached.
        #[arg(long)]
        mcp_uds: Option<std::path::PathBuf>,

        /// Drop any existing live-db + zero the controller so this
        /// daemon starts with a cold parse against `--source`,
        /// regardless of what the arena's prior owner left behind.
        ///
        /// Required opt-in when reusing an arena that previously
        /// served a different `--source` (otherwise startup refuses
        /// with a source-root-mismatch error). Bead
        /// `ley-line-open-c7d00f`.
        #[arg(long, default_value_t = false)]
        reset_arena: bool,

        /// Activate or resume the private CDC index before publishing the
        /// daemon's first arena snapshot.
        #[arg(long, default_value_t = false)]
        cdc: bool,
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
        Cmd::ExecutionDaemon {
            arena,
            control,
            cas_root,
            run_root,
            worker,
            catalog,
            backend: backend_kind,
            libkrun,
            devices,
            runtime_files,
            ready_timeout_ms,
            cdc,
            allow_unverified_evidence,
        } => {
            if !worker.is_file() {
                bail!(
                    "native worker is not an executable file: {}",
                    worker.display()
                );
            }
            if !cas_root.is_dir() {
                bail!(
                    "CAS root is not an existing directory: {}",
                    cas_root.display()
                );
            }
            std::fs::create_dir_all(&run_root)
                .with_context(|| format!("create execution run root {}", run_root.display()))?;
            let catalog_bytes = std::fs::read(&catalog)
                .with_context(|| format!("read execution catalog {}", catalog.display()))?;
            let resolver = Arc::new(
                leyline_runtime::CatalogResolver::from_json(&catalog_bytes)
                    .context("parse execution catalog")?,
            );
            let mache_dir = dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".mache");
            std::fs::create_dir_all(&mache_dir)
                .with_context(|| format!("create daemon directory {}", mache_dir.display()))?;
            let config = leyline_cli_lib::cmd_daemon::DaemonConfig {
                arena: arena.unwrap_or_else(|| mache_dir.join("execution.arena")),
                arena_size_mib: 64,
                control,
                mount: None,
                backend: cmd_serve::default_backend(),
                nfs_port: 0,
                language: None,
                timeout: None,
                source: None,
                mcp_port: None,
                mcp_bind: None,
                mcp_allow_public: false,
                mcp_no_auth: false,
                mcp_uds: None,
                reset_arena: false,
            };
            let verifier: Arc<dyn leyline_runtime::EvidenceVerifier> = if allow_unverified_evidence
            {
                Arc::new(leyline_runtime::MetadataOnlyEvidenceVerifier)
            } else {
                Arc::new(leyline_runtime::RejectUnverifiedEvidence)
            };
            match backend_kind {
                ExecutionBackendKind::Native => {
                    if libkrun.is_some() || !devices.is_empty() {
                        bail!("--libkrun and --device require --backend micro-vm");
                    }
                    let backend =
                        leyline_runtime::backends::native_backend::NativeWorkerBackend::new(
                            leyline_runtime::backends::native_backend::NativeWorkerConfig {
                                worker,
                                cas_root,
                                ephemeral_root: run_root,
                                runtime_files,
                                ready_timeout: Duration::from_millis(ready_timeout_ms),
                            },
                        );
                    let service = Arc::new(leyline_runtime::ExecutionService::new(backend));
                    let policy = leyline_runtime::authorization::AuthorizationPolicy {
                        required_backend: leyline_runtime::BackendClass::Native,
                        ..Default::default()
                    };
                    let handler = Arc::new(
                        leyline_cli_lib::daemon::execution::RuntimeExecutionHandler::new_with_verifier(
                            service, policy, resolver,
                            Arc::clone(&verifier),
                        ),
                    );
                    leyline_cli_lib::cmd_daemon::run_execution_daemon_with_options(
                        config,
                        handler,
                        leyline_cli_lib::cmd_daemon::DaemonOptions { cdc },
                    )
                    .await
                }
                ExecutionBackendKind::MicroVm => {
                    let Some(libkrun) = libkrun else {
                        bail!("--libkrun is required with --backend micro-vm");
                    };
                    if !libkrun.is_file() {
                        bail!("libkrun is not an existing file: {}", libkrun.display());
                    }
                    if devices.iter().any(|path| !path.exists()) {
                        bail!("all --device paths must exist");
                    }
                    let backend =
                        leyline_runtime::backends::libkrun::backend::KrunWorkerBackend::new(
                            leyline_runtime::backends::libkrun::backend::KrunWorkerConfig {
                                worker,
                                cas_root,
                                ephemeral_root: run_root,
                                libkrun,
                                runtime_files,
                                devices,
                                ready_timeout: Duration::from_millis(ready_timeout_ms),
                            },
                        );
                    let service = Arc::new(leyline_runtime::ExecutionService::new(backend));
                    let policy = leyline_runtime::authorization::AuthorizationPolicy {
                        required_backend: leyline_runtime::BackendClass::MicroVm,
                        ..Default::default()
                    };
                    let handler = Arc::new(
                        leyline_cli_lib::daemon::execution::RuntimeExecutionHandler::new_with_verifier(
                            service, policy, resolver,
                            Arc::clone(&verifier),
                        ),
                    );
                    leyline_cli_lib::cmd_daemon::run_execution_daemon_with_options(
                        config,
                        handler,
                        leyline_cli_lib::cmd_daemon::DaemonOptions { cdc },
                    )
                    .await
                }
            }
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
            mcp_uds,
            reset_arena,
            cdc,
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

            let config = leyline_cli_lib::cmd_daemon::DaemonConfig {
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
                mcp_uds,
                reset_arena,
            };
            leyline_cli_lib::cmd_daemon::run_daemon_with_options(
                config,
                Arc::new(leyline_cli_lib::daemon::NoExt),
                leyline_cli_lib::cmd_daemon::DaemonOptions { cdc },
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

    #[test]
    fn daemon_cdc_is_explicit_and_parseable() {
        let default_cli = Cli::try_parse_from(["leyline", "daemon"]).unwrap();
        match default_cli.command {
            Cmd::Daemon { cdc, .. } => assert!(!cdc),
            _ => panic!("expected Daemon variant"),
        }

        let enabled_cli = Cli::try_parse_from(["leyline", "daemon", "--cdc"]).unwrap();
        match enabled_cli.command {
            Cmd::Daemon { cdc, .. } => assert!(cdc),
            _ => panic!("expected Daemon variant"),
        }
    }

    #[test]
    fn execution_daemon_requires_explicit_runtime_resources() {
        let cli = Cli::try_parse_from([
            "leyline",
            "execution-daemon",
            "--cas-root",
            "/var/lib/leyline/cas",
            "--run-root",
            "/var/lib/leyline/runs",
            "--worker",
            "/usr/libexec/leyline-native-worker",
            "--catalog",
            "/etc/leyline/execution-catalog.json",
        ])
        .unwrap();
        match cli.command {
            Cmd::ExecutionDaemon {
                cas_root,
                run_root,
                worker,
                catalog,
                ..
            } => {
                assert_eq!(cas_root, PathBuf::from("/var/lib/leyline/cas"));
                assert_eq!(run_root, PathBuf::from("/var/lib/leyline/runs"));
                assert_eq!(worker, PathBuf::from("/usr/libexec/leyline-native-worker"));
                assert_eq!(
                    catalog,
                    PathBuf::from("/etc/leyline/execution-catalog.json")
                );
            }
            _ => panic!("expected execution-daemon command"),
        }
    }

    #[test]
    fn execution_daemon_can_select_embedded_libkrun_without_a_subprocess_provider() {
        let cli = Cli::try_parse_from([
            "leyline",
            "execution-daemon",
            "--backend",
            "micro-vm",
            "--cas-root",
            "/cas",
            "--run-root",
            "/runs",
            "--worker",
            "/worker",
            "--catalog",
            "/catalog.json",
            "--libkrun",
            "/lib/libkrun.dylib",
            "--device",
            "/dev/null",
        ])
        .unwrap();
        match cli.command {
            Cmd::ExecutionDaemon {
                backend,
                libkrun,
                devices,
                ..
            } => {
                assert_eq!(backend, ExecutionBackendKind::MicroVm);
                assert_eq!(libkrun, Some(PathBuf::from("/lib/libkrun.dylib")));
                assert_eq!(devices, vec![PathBuf::from("/dev/null")]);
            }
            _ => panic!("expected execution-daemon command"),
        }
    }
}
