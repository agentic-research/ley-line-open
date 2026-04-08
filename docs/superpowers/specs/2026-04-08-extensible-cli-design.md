# Extensible CLI Design: leyline (open) + leyline (extended)

**Date:** 2026-04-08
**Status:** Approved
**Bead:** ley-line-open-36aa7a (epic)

## Problem

ley-line-open (LLO) and ley-line (LL) both have CLI binaries. LLO's `ll-open` has only `parse`. LL's `leyline` has 15+ subcommands, many of which use only open-source crates that already live in LLO. There's no defined way for LL to extend LLO's CLI, leading to duplicated logic and unclear boundaries.

## Design Principle

LLO is the product. LL is the premium layer. Both ship as a binary called `leyline`. They're distinguished by edition: `leyline 0.1.0 (open)` vs `leyline 0.1.0 (extended)`. Users install one or the other.

The `.db` file is the interface between leyline and mache. Mache never imports leyline. It queries `sqlite_master` for table presence and activates features accordingly.

## Architecture

### Crate Split

The CLI splits into a library crate (shareable) and a binary crate (thin wrapper):

```
rs/ll-open/
  cli-lib/          leyline-cli-lib    (library — Commands enum + command fns)
  cli/              leyline            (binary — thin main.rs)
```

**`leyline-cli-lib`** exports:
- `Commands` enum (clap `#[derive(Subcommand)]`) with all open subcommands
- `pub fn run(cmd: Commands) -> Result<()>` dispatcher
- Individual `pub fn cmd_parse(...)`, `pub fn cmd_splice(...)`, etc.
- `pub const EDITION: &str = "open"`

**`leyline` binary** (in LLO):
```rust
#[derive(Parser)]
#[command(name = "leyline", version = version_string())]
struct Cli {
    #[command(subcommand)]
    command: leyline_cli_lib::Commands,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    leyline_cli_lib::run(cli.command)
}

fn version_string() -> &'static str {
    concat!(env!("CARGO_PKG_VERSION"), " (", leyline_cli_lib::EDITION, ")")
}
```

**`leyline` binary** (in LL, extends):
```rust
#[derive(Subcommand)]
enum Command {
    #[command(flatten)]
    Open(leyline_cli_lib::Commands),
    Daemon { ... },
    Send { ... },
    Receive { ... },
    Embed { ... },
    Search { ... },
    Tools { ... },
    Math { ... },
    Mache { ... },
}

// Override edition
const EDITION: &str = "extended";
```

### Subcommand Ownership

**Open (LLO `leyline-cli-lib`):**

| Subcommand | Purpose | Key Dependencies |
|---|---|---|
| `parse` | Directory -> .db (nodes + _ast + _source) | leyline-ts, leyline-schema |
| `splice` | Edit AST node in .db, reproject | leyline-ts (splice.rs) |
| `serve` | Mount .db via NFS/FUSE | leyline-fs, leyline-core |
| `load` | Write .db bytes into arena, bump generation | leyline-core (Controller, ArenaHeader) |
| `inspect` | Query arena's SQLite (node lookup or raw SQL) | leyline-core, rusqlite |
| `lsp` | Spawn LSP server, enrich .db with _lsp* tables | leyline-lsp |

**Extended (LL only):**

| Subcommand | Why Private |
|---|---|
| `daemon` | Depends on leyline-net (UDP/TCP/ECDH), leyline-sign, leyline-sheaf, event router |
| `receive` / `send` | Wire protocol: FEC, ECDH handshake, manifest signing |
| `mache` | Legacy receiver mode (depends on leyline-net) |
| `embed` / `search` | MiniLM inference, vector index (leyline-embed) |
| `tools` | Tool embedding + GBNF grammar (leyline-embed) |
| `math` | SU(n) holonomy, Leech lattice (research) |

### Helper Functions

These move from ley-line's CLI into `leyline-cli-lib` since they're used by open subcommands:

- `control_path_for(arena, explicit)` — derive .ctrl path from arena path
- `default_backend()` — "nfs" on macOS, "fuse" elsewhere
- `mount_nfs(port, mountpoint, noac)` — shell out to mount_nfs/mount.nfs
- `parse_duration(s)` — "60s" / "5m" / "1h" parser
- `load_into_arena(control, db_bytes)` — mmap arena, write buffer, bump gen
- `query_arena(ctrl_path, sql)` — deserialize active buffer, run SQL

### File Layout

```
rs/ll-open/cli-lib/
  Cargo.toml
  src/
    lib.rs           Commands enum, run(), EDITION, shared helpers
    cmd_parse.rs     directory -> .db (existing logic from cli/src/main.rs)
    cmd_splice.rs    AST node edit + reproject
    cmd_serve.rs     NFS/FUSE mount from arena
    cmd_load.rs      .db -> arena writer
    cmd_inspect.rs   arena SQLite query tool
    cmd_lsp.rs       LSP enrichment pipeline

rs/ll-open/cli/
  Cargo.toml         depends on leyline-cli-lib
  src/
    main.rs          thin: parse Cli, call run()
```

### Dependencies

`leyline-cli-lib` Cargo.toml:
```toml
[dependencies]
leyline-ts = { path = "../ts", features = [...] }
leyline-schema = { path = "../../ll-core/schema" }
leyline-core = { path = "../../ll-core/core" }
leyline-fs = { path = "../fs" }
leyline-lsp = { path = "../lsp", optional = true }
rusqlite = { version = "0.34", features = ["bundled", "serialize"] }
tree-sitter = "0.26"
clap = { version = "4", features = ["derive"] }
anyhow = "1"
memmap2 = "0.9"
tokio = { version = "1", features = ["full"] }
env_logger = "0.11"
log = "0.4"
serde_json = "1"

[features]
default = ["lsp"]
lsp = ["dep:leyline-lsp"]
```

### Edition Identification

```rust
// leyline-cli-lib/src/lib.rs
pub const EDITION: &str = "open";

// Used in version string:
// "0.1.0 (open)" or "0.1.0 (extended)"
```

`leyline --version` prints edition. Subcommands unique to extended are simply absent in open builds — clap reports "unrecognized subcommand" naturally.

## Porting Strategy

1. Create `cli-lib/` crate, move existing `cmd_parse` logic there
2. Port `splice` — uses `leyline_ts::splice::{splice, reproject, splice_and_reproject}`
3. Port `serve` — uses `leyline_fs::{fuse, nfs, graph}`, `leyline_core::Controller`
4. Port `load` — uses `leyline_core::{Controller, ArenaHeader, write_to_arena}`
5. Port `inspect` — uses `leyline_core::Controller`, `rusqlite`
6. Port `lsp` — uses `leyline_lsp::LspClient`
7. Slim down `cli/` binary to thin wrapper
8. Update workspace Cargo.toml

Each port is independent and testable. The existing `ll-open parse` functionality moves into `cli-lib` without behavior changes.

## What This Does NOT Cover

- LL-side cutover (LL continues working as-is until ready)
- node_refs/node_defs extraction (separate bead: ley-line-open-371bca)
- LSP enrichment pipeline details (separate bead: ley-line-open-3701d6)
- Mache CGO elimination (separate epic: mache-36d961)
