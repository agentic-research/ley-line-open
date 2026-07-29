//! Markdown inline-injection acceptance gates (bead
//! `ley-line-open-ea1e42`, found by mache-eb2bf3).
//!
//! The block grammar leaves a paragraph's content as one opaque
//! `inline` node — code spans do not exist as nodes at all, so no
//! query over the parsed db could find a doc's symbol citations.
//! `queries/markdown/injections.scm` reparses every `inline` node
//! under `tree_sitter_md::INLINE_LANGUAGE`; `extract_markdown_inline`
//! emits each `code_span` as a `node_refs` row. A backtick span citing
//! a symbol IS a reference — that is the join surface mache's
//! `drift_doc_dead_symbol_reference` reads.
//!
//! ## Claims
//!
//! 1. **Doc-symbol citations land in `node_refs` on the host file.**
//!    A paragraph citing `` `PartitionSpec::address` `` and
//!    `` `render` `` produces two ref rows whose `source_id` is the
//!    `.md` file, with the backtick delimiters stripped from the
//!    token.
//! 2. **The refs channel, not `_ast`.** Injected subtrees emit no
//!    `_ast` rows by design (`fold_injected` doc comment) — the host's
//!    `_ast` keeps its block-grammar shape (`inline` stays opaque,
//!    fenced blocks keep their structure). Consumers join
//!    `node_refs.token` against `node_defs.token`, not `_ast`.
//! 3. **Host-hash independence.** The host `.md` file's `_ast`
//!    occurrence map is byte-identical with the injection pass on vs
//!    off (`LLO_DISABLE_INJECTIONS=1` falsification seam) — same
//!    invariant the Go→SQL injection pins.

use leyline_cli_lib::cmd_parse;
use rusqlite::Connection;
use std::fs;
use std::path::Path;
use std::sync::Mutex;
use tempfile::TempDir;

/// Serializes tests that mutate `LLO_DISABLE_INJECTIONS` — env vars are
/// process-global and the harness runs tests on parallel threads. Same
/// pattern as injection_extraction_test's `ENV_LOCK`.
static ENV_LOCK: Mutex<()> = Mutex::new(());

/// Restores the prior `LLO_DISABLE_INJECTIONS` on drop.
struct DisableInjectionsOverride {
    prev: Option<String>,
}

impl DisableInjectionsOverride {
    const KEY: &'static str = "LLO_DISABLE_INJECTIONS";

    fn set(value: &str) -> Self {
        let prev = std::env::var(Self::KEY).ok();
        // SAFETY: callers hold ENV_LOCK, so no other thread in this
        // test binary touches the env concurrently.
        unsafe { std::env::set_var(Self::KEY, value) };
        Self { prev }
    }
}

impl Drop for DisableInjectionsOverride {
    fn drop(&mut self) {
        // SAFETY: same ENV_LOCK scope as `set`.
        unsafe {
            match self.prev.take() {
                Some(v) => std::env::set_var(Self::KEY, v),
                None => std::env::remove_var(Self::KEY),
            }
        }
    }
}

/// mache-eb2bf3's measured fixture shape: prose citing two symbols in
/// backtick spans, a double-backtick span, escaped backticks, and a
/// fenced block whose content must NOT inject.
fn md_fixture() -> TempDir {
    let td = TempDir::new().unwrap();
    fs::write(
        td.path().join("README.md"),
        "\
# Title

Calls `PartitionSpec::address` then `render`, keeps `` `tick` ``,
and \\`escaped\\` is prose.

```rust
let fenced = \"never a code_span ref\";
```
",
    )
    .unwrap();
    td
}

fn parse_pass(db_path: &Path, repo: &Path) {
    let conn = Connection::open(db_path).unwrap();
    cmd_parse::parse_into_conn(&conn, repo, Some("markdown"), None).unwrap();
}

/// Every `node_refs` token for the fixture, token-ordered, with its
/// source_id.
fn ref_rows(db_path: &Path) -> Vec<(String, String)> {
    let conn = Connection::open(db_path).unwrap();
    let mut stmt = conn
        .prepare("SELECT token, source_id FROM node_refs ORDER BY token")
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

/// Full host occurrence map: (node_id, hex(node_hash)) for every `_ast`
/// row, node_id-ordered. THE host-structural-identity snapshot.
fn ast_hash_map(db_path: &Path) -> Vec<(String, String)> {
    let conn = Connection::open(db_path).unwrap();
    let mut stmt = conn
        .prepare("SELECT node_id, lower(hex(node_hash)) FROM _ast ORDER BY node_id")
        .unwrap();
    stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .unwrap()
        .map(|r| r.unwrap())
        .collect()
}

#[test]
fn md_code_span_citations_land_in_node_refs_on_the_host_file() {
    let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let repo = md_fixture();
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("live.db");
    parse_pass(&db_path, repo.path());

    // Delimiters stripped; the double-backtick span keeps its inner
    // backticks; the escaped backticks are prose (the grammar decides,
    // which is why this is not a hand-rolled backtick scanner); the
    // fenced block's content never reaches the inline grammar.
    assert_eq!(
        ref_rows(&db_path),
        vec![
            (
                "PartitionSpec::address".to_string(),
                "README.md".to_string()
            ),
            ("`tick`".to_string(), "README.md".to_string()),
            ("render".to_string(), "README.md".to_string()),
        ],
        "exactly the three code spans, tokens cleaned, on the host file"
    );
}

#[test]
fn md_inline_stays_opaque_in_ast_and_fenced_structure_is_unchanged() {
    // Claim 2: the refs channel. Injected subtrees emit no `_ast`
    // rows, so the host's block-grammar shape is the whole `_ast`
    // story: `inline` present, `code_span` absent, fenced-block kinds
    // exactly as before this feature.
    let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let repo = md_fixture();
    let db_dir = TempDir::new().unwrap();
    let db_path = db_dir.path().join("live.db");
    parse_pass(&db_path, repo.path());

    let conn = Connection::open(&db_path).unwrap();
    let kinds: Vec<String> = {
        let mut stmt = conn
            .prepare(
                "SELECT DISTINCT nc.kind FROM _ast a \
                 JOIN node_content nc ON nc.node_hash = a.node_hash \
                 ORDER BY nc.kind",
            )
            .unwrap();
        stmt.query_map([], |r| r.get(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect()
    };
    assert!(
        kinds.iter().any(|k| k == "inline"),
        "the block grammar's opaque inline node must still be in _ast; kinds: {kinds:?}"
    );
    assert!(
        kinds.iter().any(|k| k == "fenced_code_block"),
        "fenced-block structure must be unchanged; kinds: {kinds:?}"
    );
    assert!(
        !kinds.iter().any(|k| k == "code_span"),
        "injected inline nodes emit refs, not _ast rows (fold_injected contract); kinds: {kinds:?}"
    );
}

#[test]
fn md_host_hashes_independent_of_injection_pass() {
    // Claim 3: turning the injection pass off must change WHAT FACTS
    // exist (no code-span refs) without moving a single host hash —
    // the injected grammar's version can never re-hash a markdown
    // file.
    let _l = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let repo = md_fixture();

    let on_dir = TempDir::new().unwrap();
    let on_db = on_dir.path().join("on.db");
    parse_pass(&on_db, repo.path());

    let off_dir = TempDir::new().unwrap();
    let off_db = off_dir.path().join("off.db");
    {
        let _off = DisableInjectionsOverride::set("1");
        parse_pass(&off_db, repo.path());
    }

    assert_eq!(
        ast_hash_map(&on_db),
        ast_hash_map(&off_db),
        "host _ast occurrence map must be byte-identical with injections on vs off"
    );
    assert!(
        ref_rows(&off_db).is_empty(),
        "with injections disabled no code-span refs exist — the facts came from the injection pass"
    );
    assert!(
        !ref_rows(&on_db).is_empty(),
        "with injections enabled the code-span refs exist"
    );
}
