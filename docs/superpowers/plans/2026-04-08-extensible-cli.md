# Extensible CLI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extract CLI command logic into a library crate (`leyline-cli-lib`) so both `leyline` (open) and `leyline` (extended) can share subcommands, then port the remaining open subcommands (splice, serve, load, inspect, lsp) from ley-line.

**Architecture:** Split `rs/ll-open/cli/` into `cli-lib/` (library with `Commands` enum + `pub fn` per command) and `cli/` (thin binary wrapper). The binary is renamed from `ll-open` to `leyline`. Each ported command calls existing LLO library crates — no new business logic, just CLI wiring.

**Tech Stack:** Rust (edition 2024), clap 4 (derive), anyhow, tokio, rusqlite, leyline-ts, leyline-fs, leyline-lsp, leyline-core

---

### Task 1: Create `cli-lib` crate with `Commands` enum and move `parse` logic

**Files:**
- Create: `rs/ll-open/cli-lib/Cargo.toml`
- Create: `rs/ll-open/cli-lib/src/lib.rs`
- Create: `rs/ll-open/cli-lib/src/cmd_parse.rs`
- Modify: `rs/ll-open/cli/Cargo.toml`
- Modify: `rs/ll-open/cli/src/main.rs`
- Modify: `rs/Cargo.toml` (workspace members glob already covers `ll-open/*`)

- [ ] **Step 1: Create `cli-lib/Cargo.toml`**

```toml
[package]
name = "leyline-cli-lib"
version = "0.1.0"
edition = "2024"

[dependencies]
leyline-ts = { path = "../ts", features = ["html", "markdown", "json", "yaml", "go", "python", "elixir"] }
leyline-schema = { path = "../../ll-core/schema" }
leyline-core = { path = "../../ll-core/core" }
leyline-fs = { path = "../fs" }
leyline-lsp = { path = "../lsp", optional = true }
rusqlite = { version = "0.34", features = ["bundled", "serialize"] }
tree-sitter = "0.26"
clap = { version = "4", features = ["derive"] }
anyhow = "1"
memmap2 = "0.9"
bytemuck = { version = "1.14", features = ["derive"] }
tokio = { version = "1", features = ["full"] }
env_logger = "0.11"
log = "0.4"
serde_json = "1"

[features]
default = ["lsp"]
lsp = ["dep:leyline-lsp"]
```

- [ ] **Step 2: Create `cli-lib/src/lib.rs` with Commands enum and EDITION**

```rust
pub mod cmd_parse;

use anyhow::Result;
use clap::Subcommand;
use std::path::PathBuf;

/// Edition identifier — "open" for ley-line-open, overridden to "extended" by ley-line.
pub const EDITION: &str = "open";

#[derive(Subcommand)]
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

/// Dispatch a command to its handler.
pub fn run(cmd: Commands) -> Result<()> {
    match cmd {
        Commands::Parse {
            source,
            output,
            lang,
        } => cmd_parse::cmd_parse(&source, &output, lang.as_deref()),
    }
}
```

- [ ] **Step 3: Create `cli-lib/src/cmd_parse.rs` — move all parse logic from `cli/src/main.rs`**

Move the following functions verbatim from `cli/src/main.rs` into `cmd_parse.rs`:
- `cmd_parse()`
- `ensure_dirs()`
- `project_file()`
- `walk_children()`
- `collect_files()`

Add the necessary imports at the top:

```rust
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use leyline_ts::languages::TsLanguage;
use leyline_ts::schema::{create_ast_schema, insert_ast, insert_node, insert_source};
use rusqlite::Connection;
use tree_sitter::TreeCursor;
```

All five functions move as-is. `cmd_parse` becomes `pub fn cmd_parse(...)`.

- [ ] **Step 4: Update `cli/Cargo.toml` — depend on `leyline-cli-lib`, rename binary to `leyline`**

```toml
[package]
name = "leyline-cli"
version = "0.1.0"
edition = "2024"

[[bin]]
name = "leyline"
path = "src/main.rs"

[dependencies]
leyline-cli-lib = { path = "../cli-lib" }
clap = { version = "4", features = ["derive"] }
anyhow = "1"
env_logger = "0.11"
```

- [ ] **Step 5: Rewrite `cli/src/main.rs` as thin wrapper**

```rust
use anyhow::Result;
use clap::Parser;

#[derive(Parser)]
#[command(
    name = "leyline",
    about = "Leyline — data plane for agentic systems",
    version = concat!(env!("CARGO_PKG_VERSION"), " (", leyline_cli_lib::EDITION, ")")
)]
struct Cli {
    #[command(subcommand)]
    command: leyline_cli_lib::Commands,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cli = Cli::parse();
    leyline_cli_lib::run(cli.command)
}
```

- [ ] **Step 6: Verify build and existing tests pass**

Run: `cd /Users/jamesgardner/remotes/art/ley-line-open/rs && cargo build -p leyline-cli && cargo test --workspace`

Expected: Compiles, all 48+ tests pass, binary is now `leyline` instead of `ll-open`.

- [ ] **Step 7: Verify `leyline parse` works**

Run: `cd /Users/jamesgardner/remotes/art/ley-line-open/rs && cargo run -p leyline-cli -- parse /tmp/test-src -o /tmp/test-out.db`

Where `/tmp/test-src` has a simple `.go` file. Expected: same output as before.

- [ ] **Step 8: Verify `leyline --version` shows edition**

Run: `cargo run -p leyline-cli -- --version`

Expected: `leyline 0.1.0 (open)`

- [ ] **Step 9: Commit**

```bash
git add rs/ll-open/cli-lib/ rs/ll-open/cli/
git commit -m "refactor: extract cli-lib crate, rename binary to leyline

Move parse command logic into leyline-cli-lib library crate. The binary
is now 'leyline' with edition identification ('open' vs 'extended').
This enables ley-line (private) to extend via clap flatten."
```

---

### Task 2: Add `splice` subcommand

**Files:**
- Create: `rs/ll-open/cli-lib/src/cmd_splice.rs`
- Modify: `rs/ll-open/cli-lib/src/lib.rs`

- [ ] **Step 1: Create `cmd_splice.rs`**

```rust
use std::path::Path;

use anyhow::{Context, Result};

/// Splice new text into an AST node in a .db file.
///
/// Reads the .db, splices the node via byte-range replacement,
/// validates the result parses, reprojects, and writes back.
pub fn cmd_splice(db: &Path, node: &str, text: &str) -> Result<()> {
    log::info!("Splicing node '{}' in {}", node, db.display());

    let db_bytes =
        std::fs::read(db).with_context(|| format!("read db: {}", db.display()))?;

    let updated = leyline_ts::splice::splice_db_bytes(&db_bytes, node, text)?;

    std::fs::write(db, &updated)
        .with_context(|| format!("write db: {}", db.display()))?;

    log::info!("Spliced '{}': db {} bytes", node, updated.len());

    Ok(())
}
```

- [ ] **Step 2: Add `Splice` variant to `Commands` enum and dispatch in `lib.rs`**

Add to `lib.rs`:

```rust
pub mod cmd_splice;
```

Add variant to `Commands`:

```rust
    /// Splice new text into an AST node (edit a parsed .db file).
    Splice {
        /// Path to the SQLite .db file (output of `leyline parse`).
        #[arg(long)]
        db: PathBuf,

        /// Node ID to splice into (e.g. "src/main.go/function_declaration/identifier").
        #[arg(long)]
        node: String,

        /// New text content for the node.
        #[arg(long)]
        text: String,
    },
```

Add to `run()` match:

```rust
        Commands::Splice { db, node, text } => cmd_splice::cmd_splice(&db, &node, &text),
```

- [ ] **Step 3: Build and verify**

Run: `cd /Users/jamesgardner/remotes/art/ley-line-open/rs && cargo build -p leyline-cli-lib && cargo test --workspace`

Expected: Compiles, all tests pass.

- [ ] **Step 4: Smoke test splice**

```bash
# Create a test db
echo 'package main

func Hello() string { return "hello" }
' > /tmp/splice-test.go
mkdir -p /tmp/splice-src && cp /tmp/splice-test.go /tmp/splice-src/
cargo run -p leyline-cli -- parse /tmp/splice-src -o /tmp/splice-test.db

# Splice a node (inspect with sqlite3 to find a valid node_id first)
sqlite3 /tmp/splice-test.db "SELECT node_id FROM _ast WHERE node_kind='identifier' LIMIT 1"
# Use the result as the node argument
```

- [ ] **Step 5: Commit**

```bash
git add rs/ll-open/cli-lib/src/cmd_splice.rs rs/ll-open/cli-lib/src/lib.rs
git commit -m "feat(cli): add splice subcommand — edit AST nodes in .db files"
```

---

### Task 3: Add `load` subcommand

**Files:**
- Create: `rs/ll-open/cli-lib/src/cmd_load.rs`
- Modify: `rs/ll-open/cli-lib/src/lib.rs`
- Modify: `rs/ll-open/cli-lib/Cargo.toml` (memmap2, bytemuck already listed)

- [ ] **Step 1: Create `cmd_load.rs`**

```rust
use std::path::Path;

use anyhow::{Context, Result, bail};
use bytemuck;
use leyline_core::{ArenaHeader, Controller, write_to_arena};

/// Load a .db file into the arena: write to inactive buffer, flip, bump generation.
pub fn cmd_load(db: &Path, control: &Path) -> Result<()> {
    log::info!("Loading {} into arena", db.display());
    let db_bytes =
        std::fs::read(db).with_context(|| format!("read db file: {}", db.display()))?;
    load_into_arena(control, &db_bytes)
}

/// Write db bytes into an arena via the control block, bump generation.
pub fn load_into_arena(control: &Path, db_bytes: &[u8]) -> Result<()> {
    let ctrl = Controller::open_or_create(control)?;
    let arena_path = ctrl.arena_path();
    let arena_size = ctrl.arena_size();
    let current_gen = ctrl.generation();

    if arena_path.is_empty() {
        bail!("control block has no arena path — is `leyline serve` running?");
    }

    let arena_file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&arena_path)
        .with_context(|| format!("open arena: {}", arena_path))?;
    anyhow::ensure!(
        arena_file.metadata()?.len() == arena_size,
        "arena file size mismatch (expected {}, got {})",
        arena_size,
        arena_file.metadata()?.len()
    );
    let mut mmap = unsafe { memmap2::MmapMut::map_mut(&arena_file)? };

    // Initialize header if arena is fresh
    let existing_magic = u32::from_ne_bytes(mmap[..4].try_into().unwrap());
    if existing_magic == 0 {
        let header = ArenaHeader {
            magic: ArenaHeader::MAGIC,
            version: ArenaHeader::VERSION,
            active_buffer: 0,
            padding: [0; 2],
            sequence: 0,
        };
        let hb = bytemuck::bytes_of(&header);
        mmap[..hb.len()].copy_from_slice(hb);
        mmap.flush()?;
    }

    write_to_arena(&mut mmap, db_bytes)?;

    let new_header: ArenaHeader =
        *bytemuck::from_bytes(&mmap[..std::mem::size_of::<ArenaHeader>()]);

    let new_gen = current_gen + 1;
    let mut ctrl = Controller::open_or_create(control)?;
    ctrl.set_arena(&arena_path, arena_size, new_gen)?;

    log::info!(
        "Loaded {} bytes into arena, generation {} (seq {}, active buffer {})",
        db_bytes.len(),
        new_gen,
        new_header.sequence,
        new_header.active_buffer
    );

    Ok(())
}
```

- [ ] **Step 2: Add `Load` variant to `Commands` and dispatch**

Add to `lib.rs`:

```rust
pub mod cmd_load;
```

Add variant:

```rust
    /// Load a .db file into the arena (write to inactive buffer, flip, bump generation).
    Load {
        /// Path to the SQLite .db file to load.
        #[arg(long)]
        db: PathBuf,

        /// Path to the control block file.
        #[arg(long)]
        control: PathBuf,
    },
```

Add to `run()`:

```rust
        Commands::Load { db, control } => cmd_load::cmd_load(&db, &control),
```

- [ ] **Step 3: Build and test**

Run: `cd /Users/jamesgardner/remotes/art/ley-line-open/rs && cargo build -p leyline-cli-lib && cargo test --workspace`

Expected: Compiles, all tests pass.

- [ ] **Step 4: Commit**

```bash
git add rs/ll-open/cli-lib/src/cmd_load.rs rs/ll-open/cli-lib/src/lib.rs
git commit -m "feat(cli): add load subcommand — write .db into arena"
```

---

### Task 4: Add `inspect` subcommand

**Files:**
- Create: `rs/ll-open/cli-lib/src/cmd_inspect.rs`
- Modify: `rs/ll-open/cli-lib/src/lib.rs`

- [ ] **Step 1: Create `cmd_inspect.rs`**

```rust
use std::path::Path;

use anyhow::{Context, Result};
use leyline_core::{ArenaHeader, Controller};

/// Inspect a node or run a SQL query against the arena's active buffer.
pub fn cmd_inspect(id: &str, arena: &Path, control: Option<&Path>, query: Option<&str>) -> Result<()> {
    let ctrl_path = control
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| {
            let mut p = arena.to_path_buf();
            p.set_extension("ctrl");
            p
        });

    let ctrl = Controller::open_or_create(&ctrl_path)?;
    let arena_path = ctrl.arena_path();
    let arena_size = ctrl.arena_size();

    anyhow::ensure!(!arena_path.is_empty(), "no arena path in controller");

    let file = std::fs::File::open(&arena_path)
        .with_context(|| format!("open arena: {arena_path}"))?;
    let mmap = unsafe { memmap2::Mmap::map(&file)? };
    let header: ArenaHeader =
        *bytemuck::from_bytes(&mmap[..std::mem::size_of::<ArenaHeader>()]);

    let buf_size = (arena_size as usize - ArenaHeader::HEADER_SIZE as usize) / 2;
    let offset = ArenaHeader::HEADER_SIZE as usize + (header.active_buffer as usize * buf_size);
    let db_bytes = &mmap[offset..offset + buf_size];

    let mut conn = rusqlite::Connection::open_in_memory()?;
    conn.deserialize_read_exact(
        rusqlite::DatabaseName::Main,
        std::io::Cursor::new(db_bytes),
        buf_size,
        true,
    )?;

    let sql = query.unwrap_or("SELECT id, parent_id, name, kind, size FROM nodes WHERE id = ?1");

    if query.is_some() {
        // Raw SQL mode
        let mut stmt = conn.prepare(sql)?;
        let col_count = stmt.column_count();
        let col_names: Vec<String> = (0..col_count)
            .map(|i| stmt.column_name(i).unwrap_or("?").to_string())
            .collect();
        println!("{}", col_names.join("\t"));

        let rows = stmt.query_map([], |row| {
            let mut vals = Vec::with_capacity(col_count);
            for i in 0..col_count {
                let val: rusqlite::types::Value = row.get(i)?;
                vals.push(match val {
                    rusqlite::types::Value::Null => "NULL".to_string(),
                    rusqlite::types::Value::Integer(n) => n.to_string(),
                    rusqlite::types::Value::Real(f) => f.to_string(),
                    rusqlite::types::Value::Text(s) => s,
                    rusqlite::types::Value::Blob(b) => format!("<{} bytes>", b.len()),
                });
            }
            Ok(vals)
        })?;
        for row in rows {
            println!("{}", row?.join("\t"));
        }
    } else {
        // Node lookup mode
        let mut stmt = conn.prepare(sql)?;
        let result = stmt.query_row([id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
            ))
        });
        match result {
            Ok((id, parent, name, kind, size)) => {
                println!("id:     {id}");
                println!("parent: {}", parent.unwrap_or_default());
                println!("name:   {name}");
                println!("kind:   {} ({})", kind, if kind == 1 { "dir" } else { "file" });
                println!("size:   {size}");
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => {
                println!("node '{id}' not found");
            }
            Err(e) => return Err(e.into()),
        }
    }

    Ok(())
}
```

- [ ] **Step 2: Add `Inspect` variant to `Commands` and dispatch**

Add to `lib.rs`:

```rust
pub mod cmd_inspect;
```

Add variant:

```rust
    /// Inspect a node in the arena's SQLite database.
    Inspect {
        /// Node ID to look up (primary key in the nodes table).
        id: String,

        /// Path to the arena file.
        #[arg(long, default_value = "./leyline.arena")]
        arena: PathBuf,

        /// Path to the control block file (default: <arena>.ctrl).
        #[arg(long)]
        control_path: Option<PathBuf>,

        /// SQL query to run instead of default node lookup.
        #[arg(long)]
        query: Option<String>,
    },
```

Add to `run()`:

```rust
        Commands::Inspect {
            id,
            arena,
            control_path,
            query,
        } => cmd_inspect::cmd_inspect(&id, &arena, control_path.as_deref(), query.as_deref()),
```

- [ ] **Step 3: Build and test**

Run: `cd /Users/jamesgardner/remotes/art/ley-line-open/rs && cargo build -p leyline-cli-lib && cargo test --workspace`

Expected: Compiles, all tests pass.

- [ ] **Step 4: Commit**

```bash
git add rs/ll-open/cli-lib/src/cmd_inspect.rs rs/ll-open/cli-lib/src/lib.rs
git commit -m "feat(cli): add inspect subcommand — query arena SQLite"
```

---

### Task 5: Add `serve` subcommand (open edition — no jj/embed/heartbeat)

**Files:**
- Create: `rs/ll-open/cli-lib/src/cmd_serve.rs`
- Modify: `rs/ll-open/cli-lib/src/lib.rs`

- [ ] **Step 1: Create `cmd_serve.rs`**

```rust
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use leyline_core::{Controller, create_arena};
use leyline_fs::fuse::mount_fuse;
use leyline_fs::graph::{Graph, HotSwapGraph};

/// Serve an arena via NFS or FUSE mount.
pub async fn cmd_serve(
    arena: &Path,
    arena_size_mib: u64,
    control: Option<&Path>,
    mount: &Path,
    backend: &str,
    nfs_port: u16,
    language: Option<&str>,
    timeout: Option<&str>,
) -> Result<()> {
    let arena_size = arena_size_mib * 1024 * 1024;
    let ctrl_path = control
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| {
            let mut p = arena.to_path_buf();
            p.set_extension("ctrl");
            p
        });

    log::info!("leyline serve starting");
    log::info!("  arena: {}", arena.display());
    log::info!("  arena size: {} MiB", arena_size_mib);
    log::info!("  control: {}", ctrl_path.display());
    log::info!("  mount: {}", mount.display());

    // Create/open arena file
    let _mmap = create_arena(arena, arena_size)?;

    // Create/open control block, set arena path at gen 0 if fresh
    let mut ctrl = Controller::open_or_create(&ctrl_path)?;
    if ctrl.generation() == 0 && ctrl.arena_path().is_empty() {
        let arena_abs = std::fs::canonicalize(arena)?;
        ctrl.set_arena(arena_abs.to_str().unwrap(), arena_size, 0)?;
    }

    // HotSwapGraph with optional validation
    let graph: Arc<dyn Graph> = {
        let default_lang = language.and_then(|ext| {
            let lang = leyline_fs::validate::language_for_extension(ext);
            if lang.is_none() {
                log::warn!("unknown language '{ext}', validation disabled for extensionless files");
            } else {
                log::info!("  validation: enabled (default language: {ext})");
            }
            lang
        });
        Arc::new(HotSwapGraph::new(ctrl_path.clone())?.with_validation(default_lang))
    };

    // Mount
    let _fuse_session;
    let _nfs_handle;

    match backend {
        "nfs" => {
            let listen_addr = format!("127.0.0.1:{nfs_port}");
            let (port, handle) =
                leyline_fs::nfs::serve_nfs(graph, &listen_addr).await?;
            log::info!("NFS server on 127.0.0.1:{port}");

            mount_nfs(port, mount)?;
            log::info!("NFS mounted at {}", mount.display());

            _nfs_handle = Some(handle);
            _fuse_session = None;
        }
        _ => {
            let session = mount_fuse(graph, mount)?;
            log::info!("FUSE mounted at {}", mount.display());
            _fuse_session = Some(session);
            _nfs_handle = None;
        }
    }

    log::info!("leyline serve ready — press Ctrl+C to stop");

    // Wait for timeout or signal
    if let Some(dur_str) = timeout {
        let dur = parse_duration(dur_str)?;
        tokio::time::sleep(dur).await;
        log::info!("timeout reached, shutting down");
    } else {
        tokio::signal::ctrl_c().await?;
        log::info!("received Ctrl+C, shutting down");
    }

    Ok(())
}

/// Mount an NFS share from localhost using the system mount command.
fn mount_nfs(port: u16, mountpoint: &Path) -> Result<()> {
    std::fs::create_dir_all(mountpoint)?;
    let opts = if cfg!(target_os = "macos") {
        format!("port={port},mountport={port},vers=3,tcp,locallocks,noresvport,noac")
    } else {
        format!("port={port},mountport={port},vers=3,tcp,nolock,noac")
    };
    let cmd = if cfg!(target_os = "macos") {
        "mount_nfs"
    } else {
        "mount.nfs"
    };
    let status = std::process::Command::new(cmd)
        .args(["-o", &opts, "localhost:/", mountpoint.to_str().unwrap()])
        .status()?;
    anyhow::ensure!(status.success(), "NFS mount failed (exit {})", status);
    Ok(())
}

/// Parse a human-friendly duration string like "60s", "5m", "1h".
fn parse_duration(s: &str) -> Result<std::time::Duration> {
    let s = s.trim();
    let (num, unit) = if let Some(n) = s.strip_suffix('s') {
        (n, 1u64)
    } else if let Some(n) = s.strip_suffix('m') {
        (n, 60)
    } else if let Some(n) = s.strip_suffix('h') {
        (n, 3600)
    } else {
        (s, 1)
    };
    let n: u64 = num.parse().context("invalid duration number")?;
    Ok(std::time::Duration::from_secs(n * unit))
}

/// Default mount backend for the platform.
pub fn default_backend() -> String {
    if cfg!(target_os = "macos") {
        "nfs".into()
    } else {
        "fuse".into()
    }
}
```

- [ ] **Step 2: Add `Serve` variant to `Commands` and async dispatch**

Add to `lib.rs`:

```rust
pub mod cmd_serve;
```

Add variant:

```rust
    /// Serve arena via NFS/FUSE mount.
    Serve {
        /// Path to the arena file.
        #[arg(long, default_value = "./leyline.arena")]
        arena: PathBuf,

        /// Arena size in MiB.
        #[arg(long, default_value_t = 64)]
        arena_size_mib: u64,

        /// Path to the control block file (default: <arena>.ctrl).
        #[arg(long)]
        control: Option<PathBuf>,

        /// Mount point (e.g. /tmp/ley-mount).
        #[arg(long)]
        mount: PathBuf,

        /// Mount backend: "nfs" (default on macOS) or "fuse".
        #[arg(long, default_value_t = cmd_serve::default_backend())]
        backend: String,

        /// NFS server port (0 = ephemeral).
        #[arg(long, default_value_t = 0)]
        nfs_port: u16,

        /// Default language for validation of extensionless files (go, py, js, ts, rs).
        #[arg(long)]
        language: Option<String>,

        /// Auto-shutdown after duration (e.g. "60s", "5m", "1h").
        #[arg(long)]
        timeout: Option<String>,
    },
```

Change `run()` to `pub async fn run(...)` and update the serve dispatch:

```rust
        Commands::Serve {
            arena,
            arena_size_mib,
            control,
            mount,
            backend,
            nfs_port,
            language,
            timeout,
        } => cmd_serve::cmd_serve(
            &arena,
            arena_size_mib,
            control.as_deref(),
            &mount,
            &backend,
            nfs_port,
            language.as_deref(),
            timeout.as_deref(),
        ).await,
```

Since `run()` is now async, update the binary's `main.rs` to use `#[tokio::main]`:

```rust
#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let cli = Cli::parse();
    leyline_cli_lib::run(cli.command).await
}
```

- [ ] **Step 3: Build and test**

Run: `cd /Users/jamesgardner/remotes/art/ley-line-open/rs && cargo build -p leyline-cli && cargo test --workspace`

Expected: Compiles, all tests pass.

- [ ] **Step 4: Commit**

```bash
git add rs/ll-open/cli-lib/src/cmd_serve.rs rs/ll-open/cli-lib/src/lib.rs rs/ll-open/cli/src/main.rs
git commit -m "feat(cli): add serve subcommand — mount arena via NFS/FUSE"
```

---

### Task 6: Add `lsp` subcommand

**Files:**
- Create: `rs/ll-open/cli-lib/src/cmd_lsp.rs`
- Modify: `rs/ll-open/cli-lib/src/lib.rs`

- [ ] **Step 1: Create `cmd_lsp.rs`**

```rust
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Stats from an LSP projection run.
pub struct LspStats {
    pub symbols: usize,
    pub diagnostics: usize,
    pub definitions: usize,
    pub hovers: usize,
    pub references: usize,
}

/// Spawn an LSP server, collect symbols + diagnostics, project into .db.
pub async fn cmd_lsp(
    server: &str,
    server_args: &[String],
    input: &Path,
    output: &Path,
    merge_db: Option<&Path>,
    language_id: Option<&str>,
) -> Result<()> {
    use leyline_lsp::{client::LspClient, project};

    // Infer language ID from extension if not provided
    let lang_id = language_id
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            input
                .extension()
                .and_then(|e| e.to_str())
                .map(|ext| match ext {
                    "py" | "pyi" => "python",
                    "js" | "jsx" => "javascript",
                    "ts" | "tsx" => "typescript",
                    "rs" => "rust",
                    "go" => "go",
                    "c" | "h" => "c",
                    "cpp" | "cc" | "hpp" => "cpp",
                    "java" => "java",
                    "rb" => "ruby",
                    other => other,
                })
                .unwrap_or("plaintext")
                .to_string()
        });

    let existing_db = merge_db
        .map(|p| std::fs::read(p).with_context(|| format!("read merge db: {}", p.display())))
        .transpose()?;

    let input = std::fs::canonicalize(input)
        .with_context(|| format!("resolve input: {}", input.display()))?;
    let source_text = std::fs::read_to_string(&input)
        .with_context(|| format!("read input: {}", input.display()))?;

    let file_uri = format!("file://{}", input.display());
    let root_dir = input.parent().unwrap_or(Path::new("/"));
    let root_uri = format!("file://{}", root_dir.display());

    let args_ref: Vec<&str> = server_args.iter().map(|s| s.as_str()).collect();
    let mut client = LspClient::start(server, &args_ref, &root_uri).await?;

    client.open_file(&file_uri, &lang_id, &source_text).await?;

    // Poll for symbols — some servers need time to index
    let mut symbols = Vec::new();
    for attempt in 0..10 {
        let wait = if attempt == 0 { 2 } else { 1 };
        tokio::time::sleep(std::time::Duration::from_secs(wait)).await;
        symbols = client.document_symbols(&file_uri).await?;
        if !symbols.is_empty() {
            break;
        }
        log::debug!("No symbols yet, retrying ({}/10)...", attempt + 1);
    }
    client.drain_notifications().await;

    let diagnostics: Vec<_> = client
        .diagnostics
        .iter()
        .flat_map(|(_, diags)| diags.clone())
        .collect();

    log::info!(
        "LSP: {} symbols, {} diagnostics from {}",
        symbols.len(),
        diagnostics.len(),
        server
    );

    let db_bytes = if let Some(existing_bytes) = existing_db.as_deref() {
        // Merge mode: open existing db, enrich with LSP data
        let mut conn = rusqlite::Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA journal_mode=OFF;")?;
        let mut cursor = std::io::Cursor::new(existing_bytes);
        conn.deserialize_read_exact(
            rusqlite::DatabaseName::Main,
            &mut cursor,
            existing_bytes.len(),
            false,
        )?;

        let matched = project::merge_lsp_into_ast(&symbols, &diagnostics, &conn)?;
        log::info!("Merged LSP data: {matched} symbols matched to AST nodes");

        let enrichment =
            project::enrich_symbols(&mut client, &conn, &symbols, &file_uri).await?;
        log::info!("Enrichment: {enrichment}");

        let data = conn.serialize(rusqlite::DatabaseName::Main)?;
        data.to_vec()
    } else {
        // Standalone mode: project into fresh db
        let conn = rusqlite::Connection::open_in_memory()?;
        project::project_lsp_into(&symbols, &diagnostics, &file_uri, &conn)?;

        let enrichment =
            project::enrich_symbols(&mut client, &conn, &symbols, &file_uri).await?;
        log::info!("Enrichment: {enrichment}");

        let data = conn.serialize(rusqlite::DatabaseName::Main)?;
        data.to_vec()
    };

    std::fs::write(output, &db_bytes)
        .with_context(|| format!("write output: {}", output.display()))?;

    log::info!("Wrote {} bytes to {}", db_bytes.len(), output.display());

    let _ = client.shutdown().await;
    Ok(())
}
```

- [ ] **Step 2: Add `Lsp` variant to `Commands` and dispatch**

Gate behind `lsp` feature in `lib.rs`:

```rust
#[cfg(feature = "lsp")]
pub mod cmd_lsp;
```

Add variant:

```rust
    /// Spawn an LSP server, collect symbols + diagnostics, project into .db.
    #[cfg(feature = "lsp")]
    Lsp {
        /// LSP server command (e.g. "gopls", "pyright-langserver").
        #[arg(long)]
        server: String,

        /// Extra arguments for the server command (e.g. "--stdio").
        #[arg(long, num_args = 0.., allow_hyphen_values = true)]
        server_args: Vec<String>,

        /// Input source file to analyze.
        #[arg(long)]
        input: PathBuf,

        /// Output SQLite .db file.
        #[arg(long)]
        output: PathBuf,

        /// Merge into an existing .db with tree-sitter AST (instead of standalone).
        #[arg(long)]
        merge_db: Option<PathBuf>,

        /// Language ID for didOpen (default: inferred from extension).
        #[arg(long)]
        language_id: Option<String>,
    },
```

Add to `run()`:

```rust
        #[cfg(feature = "lsp")]
        Commands::Lsp {
            server,
            server_args,
            input,
            output,
            merge_db,
            language_id,
        } => cmd_lsp::cmd_lsp(
            &server,
            &server_args,
            &input,
            &output,
            merge_db.as_deref(),
            language_id.as_deref(),
        ).await,
```

- [ ] **Step 3: Build and test**

Run: `cd /Users/jamesgardner/remotes/art/ley-line-open/rs && cargo build -p leyline-cli-lib && cargo test --workspace`

Expected: Compiles, all tests pass.

- [ ] **Step 4: Commit**

```bash
git add rs/ll-open/cli-lib/src/cmd_lsp.rs rs/ll-open/cli-lib/src/lib.rs
git commit -m "feat(cli): add lsp subcommand — LSP enrichment into .db"
```

---

### Task 7: Final verification and cleanup

**Files:**
- Modify: `rs/ll-open/cli-lib/src/lib.rs` (final review)
- Modify: `rs/Cargo.lock` (auto-updated)

- [ ] **Step 1: Run full workspace build and test**

Run: `cd /Users/jamesgardner/remotes/art/ley-line-open/rs && cargo build --workspace && cargo test --workspace`

Expected: All crates compile, all tests pass.

- [ ] **Step 2: Run clippy**

Run: `cd /Users/jamesgardner/remotes/art/ley-line-open/rs && cargo clippy --workspace -- -D warnings`

Expected: No warnings.

- [ ] **Step 3: Verify binary help output**

Run: `cargo run -p leyline-cli -- --help`

Expected output shows all subcommands: parse, splice, serve, load, inspect, lsp.

Run: `cargo run -p leyline-cli -- --version`

Expected: `leyline 0.1.0 (open)`

- [ ] **Step 4: Verify `Commands` is re-exportable**

Quick compile check — create a test that confirms `leyline_cli_lib::Commands` implements `Subcommand`:

```rust
// In a scratch test or just compile-check:
fn _assert_subcommand() {
    fn _takes_subcommand<T: clap::Subcommand>() {}
    _takes_subcommand::<leyline_cli_lib::Commands>();
}
```

This confirms LL can `#[command(flatten)]` the enum.

- [ ] **Step 5: Commit Cargo.lock changes**

```bash
git add rs/Cargo.lock
git commit -m "chore: update Cargo.lock for cli-lib crate"
```

- [ ] **Step 6: Close bead**

Close bead `ley-line-open-36aa7a` (epic) with a comment summarizing the implementation.
