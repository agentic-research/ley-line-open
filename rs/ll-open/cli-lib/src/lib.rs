//! Shared CLI library for ley-line (open edition).
//!
//! Exports a `Commands` enum that can be used standalone or flattened into
//! a wrapper enum by downstream binaries (e.g. the private `leyline` binary
//! that adds `daemon`, `embed`, `send`, etc.).

// The build script includes this file to compute `LEYLINE_BUILD_VERSION`;
// the crate compiles it only for its tests.
#[cfg(test)]
mod build_version;
pub mod checkout;
#[cfg(feature = "cdc")]
pub mod cmd_cdc;
pub mod cmd_daemon;
#[cfg(feature = "lsp")]
pub mod cmd_doctor;
pub mod cmd_execution;
pub mod cmd_inspect;
pub mod cmd_load;
#[cfg(feature = "lsp")]
pub mod cmd_lsp;
pub mod cmd_parse;
pub mod cmd_rebuild;
pub mod cmd_rootfs;
pub mod cmd_self;
pub mod cmd_splice;
pub mod daemon;
pub mod durable;
pub mod head_claim;
pub mod lifecycle;
pub mod merge;
pub mod source_write;
pub mod store_root;
#[cfg(feature = "sheaf")]
pub mod topology_pass;
pub mod tree;
pub mod walk;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Subcommand, ValueEnum};

/// Edition tag for this build of the CLI library.
pub const EDITION: &str = "open";

/// The value of the daemon's `--cdc-target` flag. `source_blobs` is the one
/// CDC target (ADR-0033); the per-node `nodes` target was removed in
/// `ley-line-open-afa952`. The flag stays because mache spawns the daemon
/// with `--cdc --cdc-target source-blobs` (`internal/leyline/socket.go`), and
/// a removed flag would refuse that spawn; `--cdc-target nodes` is refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum CdcTarget {
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
    /// Populate or resume the private chunk-backed `source_blobs` index.
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

/// What `rootfs import-oci` does with an image's symbolic links, which
/// rootfs manifest v1 cannot express.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum SymlinkMode {
    /// Refuse the import, naming the first link.
    Refuse,
    /// Replace each link with a copy of its in-image target.
    Resolve,
}

#[derive(Debug, Subcommand)]
pub enum RootfsCommands {
    /// Import a digest-pinned image as a rootfs for this host's platform,
    /// and print the published rootfs as JSON. The image comes from an OCI
    /// image layout (`--layout`, offline: produce it with skopeo, crane,
    /// `docker buildx --output type=oci`, buildah or podman, or use a Zarf
    /// package's `images/`) or is pulled by digest from a registry
    /// (`--from-registry`, HTTPS, anonymous). Both give the same rootfs and
    /// report for the same digest. Never contacts a container daemon.
    ImportOci {
        /// The OCI image layout directory (holding `oci-layout`).
        #[arg(
            long,
            requires = "image",
            conflicts_with = "from_registry",
            required_unless_present = "from_registry"
        )]
        layout: Option<PathBuf>,

        /// With `--layout`: `sha256:<hex>` (or `name@sha256:<hex>`) of the
        /// image index or manifest. Tags are refused.
        #[arg(long, requires = "layout", conflicts_with = "from_registry")]
        image: Option<String>,

        /// Pull `[registry/]repository@sha256:<hex>` (an index or manifest)
        /// from its registry instead: only this host's manifest, its config
        /// and its layers are fetched, each verified before use. Tags are
        /// refused; a name without a registry is Docker Hub's.
        #[arg(long, value_name = "REFERENCE")]
        from_registry: Option<String>,

        /// With `--from-registry`: the one registry, a loopback
        /// `127.0.0.1:<port>` or `[::1]:<port>`, that may be reached over
        /// plain HTTP. For a local test registry; refused for any other
        /// address. Without it, plain HTTP is refused.
        #[arg(
            long,
            value_name = "HOST:PORT",
            requires = "from_registry",
            conflicts_with = "layout"
        )]
        insecure_plain_http_registry: Option<String>,

        /// The content-addressed rootfs store: the directory
        /// `execution-daemon --cas-root` serves.
        #[arg(long)]
        cas_root: PathBuf,

        /// Symbolic-link policy.
        #[arg(long, value_enum, default_value_t = SymlinkMode::Refuse)]
        symlinks: SymlinkMode,
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
    /// Call the first-party execution/v1 service over the daemon UDS.
    Execution {
        #[command(subcommand)]
        command: ExecutionCommands,
    },
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

    /// Recompute `storeRoot` and `parserId` from a parsed .db and compare
    /// them to its sibling head.capnp (ADR-0047 D-ii). Exits non-zero on
    /// mismatch, on a head that stamps neither, on a head digest v2 head
    /// (refused by version), or on an unreadable head.
    VerifyHead {
        /// Path to the parsed SQLite database (the head is `<db>.head.capnp`
        /// beside it).
        db: PathBuf,
    },

    /// Rebuild a parsed .db's projection into a new .db from its
    /// `source_blobs` and `_source` manifest alone, reading nothing from disk
    /// (ADR-0040 D6). Refuses an arena whose `derived_from` names another
    /// parser or another tree.
    Rebuild {
        /// The parsed .db to rebuild from (opened read-only).
        from: PathBuf,

        /// The new .db to write; must not exist.
        into: PathBuf,

        /// Rebuild only these files (`_source.id`, repeatable): a
        /// sub-manifest. Default: every file.
        #[arg(long = "path")]
        paths: Vec<String>,
    },

    /// Build execution rootfs trees in a content-addressed rootfs store.
    Rootfs {
        #[command(subcommand)]
        command: RootfsCommands,
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

#[derive(Debug, Subcommand)]
pub enum ExecutionCommands {
    /// Discover execution interfaces and backend availability.
    Capabilities {
        #[arg(long)]
        control: PathBuf,
    },
    /// Explicitly provision one execution backend.
    Provision {
        #[arg(long)]
        control: PathBuf,
        #[arg(long, default_value = "microVm")]
        backend_class: String,
        #[arg(long)]
        idempotency_key: String,
    },
    /// Read substrate or one run's status.
    Status {
        #[arg(long)]
        control: PathBuf,
        #[arg(long)]
        run_id: Option<String>,
    },
    /// Read ordered lifecycle events for one run.
    Inspect {
        #[arg(long)]
        control: PathBuf,
        #[arg(long)]
        run_id: String,
        #[arg(long, default_value_t = 0)]
        after_sequence: u64,
    },
    /// Collect a terminal receipt for one run.
    Collect {
        #[arg(long)]
        control: PathBuf,
        #[arg(long)]
        run_id: String,
    },
    /// Idempotently release one run's ephemeral resources.
    Cleanup {
        #[arg(long)]
        control: PathBuf,
        #[arg(long)]
        run_id: String,
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    /// Start a signed execution/v1 spec and grant from JSON files.
    Start {
        #[arg(long)]
        control: PathBuf,
        #[arg(long)]
        spec: PathBuf,
        #[arg(long)]
        grant: PathBuf,
    },
    /// Cancel one active run.
    Cancel {
        #[arg(long)]
        control: PathBuf,
        #[arg(long)]
        run_id: String,
        #[arg(long)]
        idempotency_key: Option<String>,
    },
}

/// Dispatch a command to its implementation.
pub async fn run(cmd: Commands) -> Result<()> {
    match cmd {
        Commands::Execution { command } => cmd_execution::run(&command).await,
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
        Commands::VerifyHead { db } => store_root::cmd_verify_head(&db),
        Commands::Rebuild { from, into, paths } => cmd_rebuild::cmd_rebuild(&from, &into, paths),
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
        Commands::Rootfs { command } => cmd_rootfs::run(&command),
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
    fn cdc_target_displays_its_clap_token() {
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
