# CDC Production Activation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use
> superpowers:subagent-driven-development (recommended) or
> superpowers:executing-plans to implement this plan task-by-task. Steps use
> checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make ley-line-open's existing CDC/materialize-on-read subsystem
explicitly activatable by library consumers, the CLI, and the daemon.

**Architecture:** A focused `leyline-fs::activation` module owns deterministic,
resumable backfill over a `rusqlite::Connection`. The shared CLI exposes the
same API through `leyline cdc enable --db`, while `daemon --cdc` runs it against
the persistent WAL living database before the first arena snapshot.

**Tech Stack:** Rust 1.97, rusqlite, clap, leyline-cdc, leyline-fs,
leyline-cli-lib, Taskfile.

## Global Constraints

- Follow RED-GREEN-REFACTOR: observe every new behavior fail before production
  code is written.
- `nodes.record` remains authoritative and byte-for-byte unchanged.
- CDC activation is explicit; writable open alone must not create CDC tables.
- Foreign arenas without CDC tables keep the `ContentSource::Record` fallback.
- CDC tables are ley-line-open private derived state; do not bump
  `leyline-schema`.
- The CLI library includes the lightweight `cdc` feature in `default`; the CLI
  includes it in `default`, `all`, and `full`; `leyline-fs` keeps it opt-in.
- Taskfile targets are the validation source of truth.
- Do not use Taskfile `--force`; preserve hashed task caching.

---

### Task 1: Resumable activation core

**Files:**

- Create: `rs/ll-open/fs/tests/cdc_activation.rs`
- Create: `rs/ll-open/fs/src/activation.rs`
- Modify: `rs/ll-open/fs/src/lib.rs`
- Modify: `rs/ll-open/fs/src/chunked.rs`

**Interfaces:**

- Consumes:
  `chunked::{create_chunked_content_schema, has_chunked_content,
  store_content_chunked}`.
- Produces:
  `activation::{ActivationOptions, ActivationReport,
  activate_chunked_content}`.

- [ ] **Step 1: Write the failing public-API test**

Create a real SQLite projection with two readable leaf rows and a directory
row. Import the wished-for API before it exists:

```rust
#![cfg(feature = "cdc")]

use leyline_fs::activation::{
    ActivationOptions, activate_chunked_content,
};
use rusqlite::{Connection, params};

fn projection() -> Connection {
    let conn = Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE nodes (
            id TEXT PRIMARY KEY,
            parent_id TEXT,
            name TEXT NOT NULL,
            kind INTEGER NOT NULL,
            size INTEGER DEFAULT 0,
            mtime INTEGER NOT NULL,
            record TEXT
        );",
    )
    .unwrap();
    for (id, kind, record) in [
        ("a.rs", 0_i64, "fn a() {}\n"),
        ("empty.rs", 0_i64, ""),
        ("dir", 1_i64, ""),
    ] {
        conn.execute(
            "INSERT INTO nodes
             (id,parent_id,name,kind,size,mtime,record)
             VALUES (?1,'',?1,?2,?3,7,?4)",
            params![id, kind, record.len() as i64, record],
        )
        .unwrap();
    }
    conn
}

#[test]
fn activation_backfills_files_and_is_idempotent() {
    let conn = projection();
    let first = activate_chunked_content(
        &conn,
        ActivationOptions { batch_size: 1 },
    )
    .unwrap();
    assert_eq!(first.eligible_nodes, 2);
    assert_eq!(first.populated_nodes, 2);
    assert_eq!(first.already_fresh_nodes, 0);
    assert_eq!(first.processed_source_bytes, 10);

    let second = activate_chunked_content(
        &conn,
        ActivationOptions { batch_size: 1 },
    )
    .unwrap();
    assert_eq!(second.populated_nodes, 0);
    assert_eq!(second.already_fresh_nodes, 2);
    assert_eq!(second.processed_source_bytes, 0);
    assert_eq!(first.manifest_rows, second.manifest_rows);
    assert_eq!(first.unique_chunk_rows, second.unique_chunk_rows);
}
```

- [ ] **Step 2: Run the public-API test and verify RED**

Run:

```bash
cargo test -p leyline-fs --no-default-features --features cdc \
  --test cdc_activation activation_backfills_files_and_is_idempotent
```

Expected: compilation fails because `leyline_fs::activation` does not exist.

- [ ] **Step 3: Implement the minimal activation API**

Export `activation` behind `#[cfg(feature = "cdc")]` in `fs/src/lib.rs`.
Implement these exact public types in `activation.rs`:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActivationOptions {
    pub batch_size: usize,
}

impl Default for ActivationOptions {
    fn default() -> Self {
        Self { batch_size: 256 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct ActivationReport {
    pub eligible_nodes: u64,
    pub populated_nodes: u64,
    pub already_fresh_nodes: u64,
    pub processed_source_bytes: u64,
    pub manifest_rows: u64,
    pub unique_chunk_rows: u64,
    pub unique_chunk_bytes: u64,
}

pub fn activate_chunked_content(
    conn: &rusqlite::Connection,
    options: ActivationOptions,
) -> anyhow::Result<ActivationReport>;
```

Validate `batch_size > 0` and validate the required `nodes` columns through
`pragma_table_info('nodes')`. Create the CDC schema, select
`id, CAST(record AS BLOB)` for `kind = 0 AND record IS NOT NULL ORDER BY id`,
and process rows in `LIMIT ? OFFSET ?` pages. These rows are readable
structural leaves, not necessarily source-file roots. Before storing a row,
call `has_chunked_content`; a fresh row increments `already_fresh_nodes`,
otherwise call `store_content_chunked` and increment populated/processed
counts with checked arithmetic. Finish by counting manifest rows, unique
chunks, and `SUM(length(chunk_bytes))`.

Adjust `has_chunked_content` so a fresh zero-length witness is sufficient when
`nodes.size = 0`; non-empty files still require manifest spans:

```rust
if live_size == 0 {
    return Ok(true);
}
```

Read `live_size` in the existing freshness query rather than issuing a second
node query.

- [ ] **Step 4: Run the activation test and verify GREEN**

Run the command from Step 2.

Expected: one passing test.

- [ ] **Step 5: Add failure/resume and stale-row RED tests**

Add:

```rust
#[test]
fn activation_resumes_after_a_per_node_failure() {
    let conn = projection();
    leyline_fs::chunked::create_chunked_content_schema(&conn).unwrap();
    conn.execute_batch(
        "CREATE TRIGGER fail_second BEFORE INSERT ON content_manifest
         WHEN NEW.node_id = 'empty.rs'
         BEGIN SELECT RAISE(ABORT, 'injected activation failure'); END;",
    )
    .unwrap();
    assert!(
        activate_chunked_content(&conn, ActivationOptions { batch_size: 1 })
            .unwrap_err()
            .to_string()
            .contains("empty.rs")
    );
    assert!(leyline_fs::chunked::has_chunked_content(&conn, "a.rs").unwrap());

    conn.execute_batch("DROP TRIGGER fail_second").unwrap();
    let resumed = activate_chunked_content(
        &conn,
        ActivationOptions { batch_size: 1 },
    )
    .unwrap();
    assert_eq!(resumed.already_fresh_nodes, 1);
    assert_eq!(resumed.populated_nodes, 1);
}

#[test]
fn activation_rebuilds_a_stale_manifest_from_authoritative_record() {
    let conn = projection();
    activate_chunked_content(&conn, ActivationOptions::default()).unwrap();
    conn.execute(
        "UPDATE nodes SET record = 'fn changed() {}', size = 15, mtime = 8
         WHERE id = 'a.rs'",
        [],
    )
    .unwrap();
    let report =
        activate_chunked_content(&conn, ActivationOptions::default()).unwrap();
    assert_eq!(report.populated_nodes, 1);
    assert_eq!(report.already_fresh_nodes, 1);
}
```

Run each test individually and confirm it fails for the missing node-context
error or stale-rebuild behavior before adjusting production code.

- [ ] **Step 6: Make resume/stale tests GREEN and run the CDC crate suite**

Attach node IDs to store errors using `with_context`. Correct any counting bug
without weakening assertions.

Run:

```bash
cargo test -p leyline-fs --no-default-features --features cdc
```

Expected: all CDC and activation tests pass.

- [ ] **Step 7: Commit the activation core**

```bash
git add rs/ll-open/fs/src/lib.rs rs/ll-open/fs/src/activation.rs \
  rs/ll-open/fs/src/chunked.rs rs/ll-open/fs/tests/cdc_activation.rs
git commit -m "[ley-line-open-f16e53] feat(cdc): add resumable activation API"
```

---

### Task 2: Shared CLI activation command

**Files:**

- Create: `rs/ll-open/cli-lib/src/cmd_cdc.rs`
- Modify: `rs/ll-open/cli-lib/src/lib.rs`
- Modify: `rs/ll-open/cli-lib/Cargo.toml`
- Modify: `rs/ll-open/cli/Cargo.toml`
- Test: `rs/ll-open/cli-lib/tests/cdc_command_test.rs`

**Interfaces:**

- Consumes:
  `leyline_fs::activation::{ActivationOptions, ActivationReport,
  activate_chunked_content}`.
- Produces: `cmd_cdc::cmd_cdc_enable(db, batch_size, json)` and shared clap
  command `leyline cdc enable`.

- [ ] **Step 1: Write CLI RED tests**

Test the callable command rather than a subprocess:

```rust
#[test]
fn cdc_enable_mutates_a_real_db_and_returns_stable_json() {
    let db = seed_projection_file();
    let report = leyline_cli_lib::cmd_cdc::enable_database(
        &db,
        ActivationOptions { batch_size: 1 },
    )
    .unwrap();
    let value = serde_json::to_value(report).unwrap();
    assert_eq!(value["eligible_nodes"], 2);
    assert_eq!(value["populated_nodes"], 2);
}

#[test]
fn cdc_enable_rejects_a_non_projection_database() {
    let db = empty_sqlite_file();
    let error = leyline_cli_lib::cmd_cdc::enable_database(
        &db,
        ActivationOptions::default(),
    )
    .unwrap_err();
    assert!(format!("{error:#}").contains("nodes"));
}
```

Run:

```bash
cargo test -p leyline-cli-lib --features cdc \
  --test cdc_command_test
```

Expected: FAIL because feature/module/function do not exist.

- [ ] **Step 2: Wire the Cargo feature and minimal command module**

Add `cdc = ["leyline-fs/cdc"]` to cli-lib and include it in cli-lib's
`default` list. Add `cdc = ["leyline-cli-lib/cdc"]` to cli and include it in
the CLI's `default`, `all`, and `full` lists.

Implement:

```rust
pub fn enable_database(
    db: &Path,
    options: ActivationOptions,
) -> Result<ActivationReport> {
    let conn = rusqlite::Connection::open(db)
        .with_context(|| format!("open CDC database {}", db.display()))?;
    activate_chunked_content(&conn, options)
        .with_context(|| format!("activate CDC in {}", db.display()))
}
```

Make Step 1 GREEN.

- [ ] **Step 3: Add clap parse and output RED tests**

Define nested shared command types:

```rust
#[derive(Debug, Subcommand)]
pub enum CdcCommands {
    Enable {
        #[arg(long)]
        db: PathBuf,
        #[arg(long, default_value_t = 256)]
        batch_size: usize,
        #[arg(long)]
        json: bool,
    },
}
```

Add a parser test asserting `leyline cdc enable --db graph.db --batch-size 8
--json` produces the exact values. Add an output formatter test that parses the
JSON string back into `serde_json::Value`.

Run the specific tests and observe RED before adding the `Commands::Cdc`
variant, dispatcher, and formatter.

- [ ] **Step 4: Make clap/output tests GREEN**

Human output is one stable line:

```text
CDC enabled: eligible=N populated=N already_fresh=N source_bytes=N manifest_rows=N unique_chunks=N unique_chunk_bytes=N
```

JSON output is `serde_json::to_string(&report)`.

Run:

```bash
cargo test -p leyline-cli-lib --features cdc \
  --test cdc_command_test
cargo test -p leyline-cli --features cdc
```

Expected: all tests pass.

- [ ] **Step 5: Commit the CLI command**

```bash
git add rs/ll-open/cli-lib/src/cmd_cdc.rs \
  rs/ll-open/cli-lib/src/lib.rs rs/ll-open/cli-lib/Cargo.toml \
  rs/ll-open/cli/Cargo.toml \
  rs/ll-open/cli-lib/tests/cdc_command_test.rs
git commit -m "[ley-line-open-f16e53] feat(cli): expose CDC activation"
```

---

### Task 3: Daemon opt-in and real arena consumer gate

**Files:**

- Modify: `rs/ll-open/cli/src/main.rs`
- Modify: `rs/ll-open/cli-lib/src/cmd_daemon.rs`
- Modify: `rs/ll-open/cli-lib/tests/wal_live_db_test.rs`
- Create: `rs/ll-open/cli-lib/tests/cdc_activation_consumer_test.rs`

**Interfaces:**

- Consumes: Task 1 activation API and existing
  `cmd_daemon::snapshot_to_arena`.
- Produces: `DaemonConfig::cdc: bool`, CLI `daemon --cdc`, and the release
  consumer gate.

- [ ] **Step 1: Write daemon CLI/config RED tests**

Extend the existing CLI parser tests:

```rust
let cli = Cli::try_parse_from(["leyline", "daemon", "--cdc"]).unwrap();
match cli.command {
    Cmd::Daemon { cdc, .. } => assert!(cdc),
    _ => panic!("expected daemon"),
}
```

Update test-only `DaemonConfig` constructors with `cdc: false`, then add a
focused daemon test whose config uses `cdc: true` and whose source fixture has
a file row. Assert the persisted `<control>.live.db` contains
`content_manifest_meta`.

Run the focused tests and observe RED because the field/flag do not exist.

- [ ] **Step 2: Implement daemon activation before first snapshot**

Add `cdc: bool` to the CLI variant and `DaemonConfig`. Immediately after
`init_living_db` succeeds and before the existing first
`snapshot_to_arena(&live_conn, &ctrl_path)`, run:

```rust
if cdc {
    let report = leyline_fs::activation::activate_chunked_content(
        &live_conn,
        leyline_fs::activation::ActivationOptions::default(),
    )
    .context("activate CDC in daemon living database")?;
    eprintln!(
        "CDC activation: eligible={} populated={} already_fresh={} source_bytes={}",
        report.eligible_nodes,
        report.populated_nodes,
        report.already_fresh_nodes,
        report.processed_source_bytes,
    );
}
```

Make the CLI/config tests GREEN.

- [ ] **Step 3: Write the real arena consumer RED test**

The integration test must:

1. Write a deterministic 1 MiB valid JSON source fixture.
2. Parse it with `cmd_parse::parse_into_conn` into a file-backed database and
   select the largest authoritative readable leaf.
3. Call the production activation API.
4. Create a deliberately small arena with `cmd_serve::setup_arena`.
5. Publish with `cmd_daemon::snapshot_to_arena`.
6. Reopen with `leyline_fs::SqliteGraph::from_arena`.
7. Read an interior 4 KiB range of that leaf through
   `read_content_at_traced`.
8. Assert byte equality, `ContentSource::Chunked`, and
   `chunks_touched(...) <= 2`.
9. Assert the controller's arena size grew and its current root equals the
   serialized database hash.

Run:

```bash
cargo test -p leyline-cli-lib --features cdc \
  --test cdc_activation_consumer_test -- --nocapture
```

Expected: RED until the production daemon/activation/publish composition is
correct. A pass that reports `ContentSource::Record` is a failure.

- [ ] **Step 4: Make the consumer gate GREEN**

Fix only production composition defects exposed by the harness. Do not weaken
source-path, byte-equality, touched-chunk, arena-growth, or root assertions.

Run the command from Step 3 twice to prove it is stable.

- [ ] **Step 5: Commit daemon activation and consumer gate**

```bash
git add rs/ll-open/cli/src/main.rs \
  rs/ll-open/cli-lib/src/cmd_daemon.rs \
  rs/ll-open/cli-lib/tests/wal_live_db_test.rs \
  rs/ll-open/cli-lib/tests/cdc_activation_consumer_test.rs
git commit -m "[ley-line-open-f16e53] feat(daemon): activate CDC before publish"
```

---

### Task 4: Documentation, changelog, and release gates

**Files:**

- Modify: `README.md`
- Modify: `ARCHITECTURE.md`
- Modify: `CHANGELOG.md`
- Modify: `Taskfile.yml`
- Modify: `.github/workflows/ci.yml` only if it bypasses `task ci`

**Interfaces:**

- Consumes: the command, daemon flag, and consumer test from Tasks 2-3.
- Produces: documented operator workflow and a cached Taskfile-owned release
  gate.

- [ ] **Step 1: Write the Taskfile gate RED assertion**

First extend `lint:cache-contract` with:

```sh
grep -q 'test:cdc-activation:' Taskfile.yml
grep -q -- '- task: test:cdc-activation' Taskfile.yml
```

Run `task lint:cache-contract` and observe RED. Then define the composed target
using the same Cargo/sccache substrate as the rest of the Taskfile:

```yaml
test:cdc-activation:
  desc: Production CDC activation + materialize-on-read consumer gate
  deps: [cache:sccache:install]
  dir: rs
  cmds:
    - cargo test -p leyline-cli-lib --features cdc
        --test cdc_activation_consumer_test
```

Add `- task: test:cdc-activation` next to `test:fs-cdc` in `ci`. Do not add
`--force`: Cargo and the Taskfile-managed sccache preserve compiled artifacts
for the downstream `test` and `check:all-features` stages.

- [ ] **Step 2: Wire and run the focused Taskfile gate twice**

Run:

```bash
task test:cdc-activation
task test:cdc-activation
```

Expected: first invocation passes by running the test; second invocation is
green and reports a fresh Cargo build without recompiling unchanged crates;
`task cache:sccache:stats` shows cache hits rather than a from-source rebuild.

- [ ] **Step 3: Update docs and changelog**

Document:

- `leyline cdc enable --db output.db`;
- `leyline daemon --cdc --source <repo>`;
- activation storage amplification and resumability;
- `nodes.record` ownership and foreign fallback;
- no `leyline-schema` version bump;
- incremental writes are wired after activation.

Remove the stale v0.10.2 changelog sentence claiming writes always rechunk in
full. Add an Unreleased entry for production activation.

- [ ] **Step 4: Run release-proportional validation**

Run:

```bash
task test:cdc-activation
task test:fs-cdc
task check:all-features
task ci
```

Expected: all commands exit 0. Record elapsed time and whether the second
focused invocation reused cached output.

- [ ] **Step 5: Commit documentation and gates**

```bash
git add README.md ARCHITECTURE.md CHANGELOG.md Taskfile.yml \
  .github/workflows/ci.yml
git commit -m "[ley-line-open-f16e53] docs(cdc): gate production activation"
```

Only stage `.github/workflows/ci.yml` if inspection proved a change was
necessary.

---

### Task 5: Bead, PR, and downstream handoff

**Files:**

- Modify: `.beads/beads.jsonl` only through a bounded export of affected IDs.

**Interfaces:**

- Consumes: green activation implementation and release evidence.
- Produces: closed `ley-line-open-f16e53`, reviewable PR, and inputs for
  `ley-line-open-035363` and `ley-line-open-040775`.

- [ ] **Step 1: Record evidence on the activation bead**

Comment with exact commands, test counts, elapsed times, consumer source
marker, touched chunk count, arena growth, and root verification result.

- [ ] **Step 2: Review the branch diff and run the final Taskfile gate**

```bash
git diff --check origin/main...HEAD
git status --short
task ci
```

Expected: clean diff check, only intended files, CI exit 0.

- [ ] **Step 3: Close the activation bead and publish bounded bead state**

Close `ley-line-open-f16e53`. Export only the CDC epic and affected CDC bead
IDs; merge those exact JSON objects into `.beads/beads.jsonl`. Do not run a
full-store export because `rosary-64494d` documents stale-row resurrection.
Validate unique IDs and exact live/export equality for the bounded set.

- [ ] **Step 4: Commit bounded bead publication**

```bash
git add .beads/beads.jsonl
git commit -m "[ley-line-open-f16e53] chore(beads): publish CDC activation"
```

- [ ] **Step 5: Push and open the activation PR**

```bash
git push -u origin codex/cdc-production-activation
gh pr create --base main --head codex/cdc-production-activation \
  --title "Activate CDC for production consumers" \
  --body-file /tmp/leyline-cdc-activation-pr.md
```

The PR body includes the design, RED evidence, green commands, no-schema-bump
decision, storage amplification note, and linked GC/consumer-contract beads.

- [ ] **Step 6: Start the next independent CDC design cycle**

After the activation PR is green and landed, brainstorm and specify
`ley-line-open-035363` transactional reachability GC. Do not fold GC into the
activation PR because activation correctness and deletion safety require
independent review and failure gates.
