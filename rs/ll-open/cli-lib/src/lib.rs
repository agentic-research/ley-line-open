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
pub mod cmd_self;
pub mod cmd_serve;
pub mod cmd_splice;
pub mod daemon;
pub mod topology_pass;
pub mod walk;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Subcommand, ValueEnum};

/// Edition tag for this build of the CLI library.
pub const EDITION: &str = "open";

/// Which authoritative table `cdc enable` builds the derived chunk index
/// over. Explicit, never heuristic (ADR-0033): the two targets have
/// different freshness designs — `nodes` rows mutate, so their manifests
/// carry a witness; `source_blobs` rows are content-addressed and immutable,
/// so theirs do not — and a database may legitimately carry either or both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CdcTarget {
    /// Construct-granular `nodes` rows (witness-gated manifests).
    Nodes,
    /// Whole-file `source_blobs` rows (witness-free manifests; rows below
    /// the 8 KiB chunking floor are skipped by design).
    SourceBlobs,
}

impl std::fmt::Display for CdcTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The clap-facing name IS the display name, so `--help`'s default
        // shows exactly the token a user must type.
        self.to_possible_value()
            .expect("CdcTarget has no skipped variants")
            .get_name()
            .fmt(f)
    }
}

/// CDC storage-administration subcommands.
#[derive(Debug, Subcommand)]
pub enum CdcCommands {
    /// Populate or resume the private chunk-backed content index.
    Enable {
        /// SQLite projection to activate in place.
        #[arg(long)]
        db: PathBuf,

        /// Authoritative table to build the chunk index over.
        #[arg(long, value_enum, default_value_t = CdcTarget::Nodes)]
        target: CdcTarget,

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

/// Self-management subcommands (bead ley-line-open-321ded, rustup/uv
/// model — the binary owns its own install/update instead of every
/// consumer's Taskfile reinventing a copy step; see `cmd_self` for the
/// full contract).
#[derive(Debug, Subcommand)]
pub enum SelfCommands {
    /// Copy the running executable into a stable install dir
    /// (default ~/.local/bin; $LEYLINE_INSTALL_DIR overrides the
    /// default; --bin-dir wins over both). Idempotent and atomic.
    /// Prints the PATH export line if the dir is off PATH — never
    /// edits rc files.
    Install {
        /// Target directory for the binary (wins over $LEYLINE_INSTALL_DIR).
        #[arg(long)]
        bin_dir: Option<PathBuf>,
    },

    /// Download a release, verify its SHA256SUMS digest, and atomically
    /// replace the installed binary (previous kept as leyline.prev).
    Update {
        /// Pin a release tag (e.g. v0.13.0). Defaults to the latest release.
        #[arg(long)]
        version: Option<String>,

        /// Allow downgrades and same-version reinstalls.
        #[arg(long)]
        force: bool,
    },

    /// Print the resolved install dir, whether it is on PATH, and the
    /// running binary's path.
    Where,
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

    /// Manage this binary's own installation: install, update, where.
    //
    // Bead ley-line-open-321ded. Doc comment above is user-facing help
    // text; this note is not: `Self` is a Rust keyword, so the variant
    // is `SelfManage` with the CLI token pinned to `self` explicitly.
    #[command(name = "self")]
    SelfManage {
        #[command(subcommand)]
        command: SelfCommands,
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
                        target,
                        batch_size,
                        json,
                    } => match target {
                        CdcTarget::Nodes => cmd_cdc::cmd_cdc_enable(&db, batch_size, json),
                        CdcTarget::SourceBlobs => {
                            cmd_cdc::cmd_cdc_enable_source_blobs(&db, batch_size, json)
                        }
                    },
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
        Commands::SelfManage { command } => {
            // Process env, current exe, and the release endpoints are
            // captured HERE, at the imperative boundary — cmd_self is
            // pure over SelfContext so its whole surface (including the
            // digest-refusal path) tests against file:// fixtures.
            let ctx = cmd_self::SelfContext::from_process();
            match command {
                SelfCommands::Install { bin_dir } => {
                    cmd_self::self_install(&ctx, bin_dir.as_deref()).map(|_| ())
                }
                SelfCommands::Update { version, force } => {
                    cmd_self::self_update(&ctx, version.as_deref(), force).map(|_| ())
                }
                SelfCommands::Where => cmd_self::self_where(&ctx).map(|_| ()),
            }
        }
        #[cfg(feature = "lsp")]
        Commands::Doctor {
            json,
            allow_missing,
        } => cmd_doctor::run_doctor(json, allow_missing),
    }
}

// In-module falsifiers for the diff-scoped mutants gate (lib tests only):
// the dispatcher's error propagation and the target token are contract, and
// their killers previously lived only in integration tests it cannot see.
#[cfg(test)]
mod dispatch_tests {
    use super::*;

    /// The Display token IS the clap token — `--help` shows exactly what a
    /// user must type, so a Display that renders nothing is a broken help
    /// surface, not a cosmetic bug.
    #[test]
    fn cdc_target_displays_its_clap_tokens() {
        assert_eq!(CdcTarget::Nodes.to_string(), "nodes");
        assert_eq!(CdcTarget::SourceBlobs.to_string(), "source-blobs");
    }

    /// `run` must propagate command failure — an `Ok(())` stub turns every
    /// failing invocation into a silent success at the process boundary.
    /// Feature-independent on purpose: with `cdc` the missing database is
    /// the error, without it the refused subcommand is — either way, Err.
    #[tokio::test]
    async fn run_surfaces_a_failing_command_as_an_error() {
        let missing = run(Commands::Cdc {
            command: CdcCommands::Enable {
                db: std::path::PathBuf::from("/nonexistent/llo-dispatch-test/x.db"),
                target: CdcTarget::SourceBlobs,
                batch_size: 16,
                json: false,
            },
        })
        .await;
        assert!(missing.is_err());
    }

    /// The `self` token and its three subcommands are the product
    /// surface consumers script against (`leyline self install` in
    /// mache's install flow) — pin that clap parses them exactly.
    #[test]
    fn self_surface_parses_install_update_where() {
        use clap::Parser;
        #[derive(Parser)]
        struct Harness {
            #[command(subcommand)]
            cmd: Commands,
        }

        let h = Harness::try_parse_from(["t", "self", "install", "--bin-dir", "/x/bin"]).unwrap();
        match h.cmd {
            Commands::SelfManage {
                command: SelfCommands::Install { bin_dir },
            } => assert_eq!(bin_dir, Some(PathBuf::from("/x/bin"))),
            other => panic!("expected self install, got {other:?}"),
        }

        let h = Harness::try_parse_from(["t", "self", "update", "--version", "v1.2.3", "--force"])
            .unwrap();
        match h.cmd {
            Commands::SelfManage {
                command: SelfCommands::Update { version, force },
            } => {
                assert_eq!(version.as_deref(), Some("v1.2.3"));
                assert!(force);
            }
            other => panic!("expected self update, got {other:?}"),
        }

        let h = Harness::try_parse_from(["t", "self", "where"]).unwrap();
        assert!(matches!(
            h.cmd,
            Commands::SelfManage {
                command: SelfCommands::Where
            }
        ));
    }
}
