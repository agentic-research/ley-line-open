//! Enrichment pipeline — layered passes that run against the living db.
//!
//! Each pass owns a disjoint set of tables (Schema Partition invariant).
//! Passes declare dependencies and run in topological order.
//! The pipeline tracks version vectors in `_meta` for staleness detection.

use std::path::Path;
use std::sync::{Arc, RwLock};
use std::time::Instant;

use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::Serialize;

use super::{DaemonState, PassStatus};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Statistics from a single enrichment pass.
#[derive(Debug, Clone, Serialize)]
pub struct EnrichmentStats {
    pub pass_name: String,
    pub files_processed: u64,
    pub items_added: u64,
    pub duration_ms: u64,
}

/// An enrichment pass that reads from the living database and writes
/// derived data back into it.
///
/// # Schema Partition Invariant
///
/// Each pass's `writes()` set must be disjoint from every other pass's
/// `writes()` set. No two passes may write to the same table. This
/// ensures passes cannot conflict with each other.
pub trait EnrichmentPass: Send + Sync {
    /// Unique name for this pass (e.g., "tree-sitter", "lsp", "embeddings").
    fn name(&self) -> &str;

    /// Which passes must complete before this one. Empty = no dependencies.
    fn depends_on(&self) -> &[&str] {
        &[]
    }

    /// Which tables this pass reads from (for dependency documentation).
    fn reads(&self) -> &[&str];

    /// Which tables this pass owns (writes to). Must be disjoint from all
    /// other passes' write sets.
    fn writes(&self) -> &[&str];

    /// Run the enrichment pass.
    ///
    /// `changed_files` lists files that changed since the last run.
    /// `None` means "all files" (full enrichment).
    ///
    /// Sync by default. Passes that need async (e.g., LSP server spawning)
    /// should use `tokio::task::block_in_place` + `Handle::block_on`
    /// internally — this tells tokio to move other tasks off the thread.
    fn run(
        &self,
        conn: &Connection,
        source_dir: &Path,
        changed_files: Option<&[String]>,
    ) -> Result<EnrichmentStats>;
}

// ---------------------------------------------------------------------------
// Pipeline executor
// ---------------------------------------------------------------------------

/// Execute a single pass: run it, record outcome in DaemonState, bump its
/// `<name>_version` in `_meta` on success. Returns the pass's stats.
///
/// Both `run_pass` (target + deps) and `run_all` (every registered pass)
/// loop over this — keep the per-pass invariants in one place so a future
/// schema change to `_meta` versioning lands once, not twice.
fn execute_pass(
    pass: &dyn EnrichmentPass,
    conn: &Connection,
    source_dir: &Path,
    changed_files: Option<&[String]>,
    state: Option<&Arc<RwLock<DaemonState>>>,
) -> Result<EnrichmentStats> {
    let outcome = pass.run(conn, source_dir, changed_files);
    record_pass_outcome(state, pass.name(), &outcome, conn);
    let result = outcome?;

    let version_key = format!("{}_version", pass.name());
    let current: u64 = get_meta_u64(conn, &version_key).unwrap_or(0);
    set_meta(conn, &version_key, &(current + 1).to_string())?;

    Ok(result)
}

/// Run a named pass (and its dependencies) against the living db.
///
/// The executor resolves dependencies in topological order and runs each
/// pass that is stale (its basis version is behind the current parse version).
///
/// If `state` is provided, each pass's outcome is recorded in
/// `state.enrichment[pass_name]` (last_run_at_ms / basis / error).
pub fn run_pass(
    passes: &[Box<dyn EnrichmentPass>],
    target: &str,
    conn: &Connection,
    source_dir: &Path,
    changed_files: Option<&[String]>,
    state: Option<&Arc<RwLock<DaemonState>>>,
) -> Result<Vec<EnrichmentStats>> {
    let order = resolve_order(passes, target)?;
    let mut stats = Vec::new();

    for pass_name in order {
        let pass = passes
            .iter()
            .find(|p| p.name() == pass_name)
            .unwrap();

        let start = Instant::now();
        let result = execute_pass(pass.as_ref(), conn, source_dir, changed_files, state)?;
        stats.push(result);
        eprintln!(
            "enrichment pass '{}' completed in {:?}",
            pass_name,
            start.elapsed()
        );
    }

    Ok(stats)
}

/// Run all registered passes in dependency order.
pub fn run_all(
    passes: &[Box<dyn EnrichmentPass>],
    conn: &Connection,
    source_dir: &Path,
    changed_files: Option<&[String]>,
    state: Option<&Arc<RwLock<DaemonState>>>,
) -> Result<Vec<EnrichmentStats>> {
    let mut stats = Vec::new();
    // Simple topological sort: run passes with no unmet dependencies first.
    let mut completed: Vec<&str> = Vec::new();
    let mut remaining: Vec<&dyn EnrichmentPass> = passes.iter().map(|p| p.as_ref()).collect();

    while !remaining.is_empty() {
        let next = remaining.iter().position(|p| {
            p.depends_on().iter().all(|dep| completed.contains(dep))
        });

        match next {
            Some(idx) => {
                let pass = remaining.remove(idx);
                let result = execute_pass(pass, conn, source_dir, changed_files, state)?;
                completed.push(pass.name());
                stats.push(result);
            }
            None => {
                let stuck: Vec<&str> = remaining.iter().map(|p| p.name()).collect();
                anyhow::bail!(
                    "enrichment pipeline stuck — unresolved dependencies for: {:?}",
                    stuck
                );
            }
        }
    }

    Ok(stats)
}

/// Wall-clock millis since UNIX_EPOCH.
fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Record a pass's success or failure into `DaemonState.enrichment[name]`.
/// On success, captures the parse_version basis at completion. On failure,
/// records the error message.
fn record_pass_outcome(
    state: Option<&Arc<RwLock<DaemonState>>>,
    name: &str,
    outcome: &Result<EnrichmentStats>,
    conn: &Connection,
) {
    let Some(state) = state else { return };
    let basis = get_meta_u64(conn, "tree-sitter_version");
    let mut s = match state.write() {
        Ok(g) => g,
        Err(_) => return,
    };
    let entry = s.enrichment.entry(name.to_string()).or_insert_with(PassStatus::default);
    entry.last_run_at_ms = Some(now_ms());
    match outcome {
        Ok(_) => {
            entry.basis = basis;
            entry.error = None;
        }
        Err(e) => {
            entry.error = Some(format!("{e:#}"));
        }
    }
}

// ---------------------------------------------------------------------------
// Dependency resolution
// ---------------------------------------------------------------------------

/// Resolve the dependency order for a target pass.
/// Returns pass names in the order they should execute.
fn resolve_order(
    passes: &[Box<dyn EnrichmentPass>],
    target: &str,
) -> Result<Vec<String>> {
    let mut order = Vec::new();
    let mut visited = std::collections::HashSet::new();
    resolve_recursive(passes, target, &mut order, &mut visited)?;
    Ok(order)
}

fn resolve_recursive(
    passes: &[Box<dyn EnrichmentPass>],
    name: &str,
    order: &mut Vec<String>,
    visited: &mut std::collections::HashSet<String>,
) -> Result<()> {
    if visited.contains(name) {
        return Ok(());
    }
    visited.insert(name.to_string());

    let pass = passes
        .iter()
        .find(|p| p.name() == name)
        .with_context(|| format!("unknown enrichment pass: {name}"))?;

    for dep in pass.depends_on() {
        resolve_recursive(passes, dep, order, visited)?;
    }

    order.push(name.to_string());
    Ok(())
}

// ---------------------------------------------------------------------------
// _meta helpers
// ---------------------------------------------------------------------------

fn get_meta_u64(conn: &Connection, key: &str) -> Option<u64> {
    conn.query_row(
        "SELECT value FROM _meta WHERE key = ?1",
        [key],
        |row| row.get::<_, String>(0),
    )
    .ok()
    .and_then(|v| v.parse().ok())
}

fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO _meta (key, value) VALUES (?1, ?2)",
        rusqlite::params![key, value],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// TreeSitterPass — the base enrichment pass
// ---------------------------------------------------------------------------

/// Wraps `parse_into_conn` as the first enrichment pass.
pub struct TreeSitterPass;

impl EnrichmentPass for TreeSitterPass {
    fn name(&self) -> &str {
        "tree-sitter"
    }

    fn reads(&self) -> &[&str] {
        &[] // reads source files, not db tables
    }

    fn writes(&self) -> &[&str] {
        &["nodes", "_ast", "_source", "node_refs", "node_defs", "_imports", "_file_index"]
    }

    fn run(
        &self,
        conn: &Connection,
        source_dir: &Path,
        changed_files: Option<&[String]>,
    ) -> Result<EnrichmentStats> {
        let start = Instant::now();
        // Forward `changed_files` as the parse scope. When the caller knows
        // which files changed (e.g. lazy LSP enrichment), we skip the full
        // tree walk; otherwise parse_into_conn does its own _file_index diff.
        let result = crate::cmd_parse::parse_into_conn(conn, source_dir, None, changed_files)?;

        Ok(EnrichmentStats {
            pass_name: "tree-sitter".to_string(),
            files_processed: result.parsed,
            items_added: result.parsed, // each file produces nodes
            duration_ms: start.elapsed().as_millis() as u64,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    struct MockPass {
        name: &'static str,
        deps: &'static [&'static str],
        reads: &'static [&'static str],
        writes: &'static [&'static str],
    }

    impl EnrichmentPass for MockPass {
        fn name(&self) -> &str { self.name }
        fn depends_on(&self) -> &[&str] { self.deps }
        fn reads(&self) -> &[&str] { self.reads }
        fn writes(&self) -> &[&str] { self.writes }
        fn run(&self, _conn: &Connection, _source: &Path, _changed: Option<&[String]>) -> Result<EnrichmentStats> {
            Ok(EnrichmentStats {
                pass_name: self.name.to_string(),
                files_processed: 0,
                items_added: 0,
                duration_ms: 0,
            })
        }
    }

    /// Pass that always errors — for the failure-path test.
    struct FailingPass;
    impl EnrichmentPass for FailingPass {
        fn name(&self) -> &str { "failing" }
        fn reads(&self) -> &[&str] { &[] }
        fn writes(&self) -> &[&str] { &[] }
        fn run(&self, _conn: &Connection, _source: &Path, _changed: Option<&[String]>) -> Result<EnrichmentStats> {
            anyhow::bail!("intentional pass failure");
        }
    }

    /// Set up a minimal in-memory db with the `_meta` table, ready for
    /// version bumps.
    fn meta_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE _meta (key TEXT PRIMARY KEY, value TEXT);").unwrap();
        conn
    }

    #[test]
    fn execute_pass_bumps_version_on_success() {
        let conn = meta_conn();
        let pass = MockPass {
            name: "alpha",
            deps: &[],
            reads: &[],
            writes: &["t"],
        };
        let stats = execute_pass(&pass, &conn, Path::new("/"), None, None).unwrap();
        assert_eq!(stats.pass_name, "alpha");

        // First run: alpha_version = 1
        let v: Option<u64> = get_meta_u64(&conn, "alpha_version");
        assert_eq!(v, Some(1));

        // Second run: bumps to 2
        execute_pass(&pass, &conn, Path::new("/"), None, None).unwrap();
        assert_eq!(get_meta_u64(&conn, "alpha_version"), Some(2));
    }

    #[test]
    fn execute_pass_does_not_bump_version_on_failure() {
        let conn = meta_conn();
        let pass = FailingPass;
        let result = execute_pass(&pass, &conn, Path::new("/"), None, None);
        assert!(result.is_err(), "failing pass must propagate error");

        // Failed pass must NOT have bumped its version. This is the
        // invariant — staleness detection downstream relies on the
        // version reflecting *successful* runs only.
        let v = get_meta_u64(&conn, "failing_version");
        assert_eq!(v, None, "failing pass version must remain unset");
    }

    #[test]
    fn execute_pass_records_outcome_in_state() {
        use std::sync::{Arc, RwLock};
        let conn = meta_conn();
        // Pre-populate tree-sitter_version so basis tracking can capture it.
        set_meta(&conn, "tree-sitter_version", "5").unwrap();

        let pass = MockPass {
            name: "beta",
            deps: &[],
            reads: &[],
            writes: &["t"],
        };
        let state = Arc::new(RwLock::new(DaemonState::initializing()));
        execute_pass(&pass, &conn, Path::new("/"), None, Some(&state)).unwrap();

        // record_pass_outcome should have written a PassStatus entry.
        let s = state.read().unwrap();
        let status = s.enrichment.get("beta").expect("beta status recorded");
        assert!(status.last_run_at_ms.is_some());
        assert_eq!(status.basis, Some(5), "basis should snapshot tree-sitter_version");
        assert!(status.error.is_none());
    }

    #[test]
    fn resolve_order_simple() {
        let passes: Vec<Box<dyn EnrichmentPass>> = vec![
            Box::new(MockPass { name: "a", deps: &[], reads: &[], writes: &["t1"] }),
            Box::new(MockPass { name: "b", deps: &["a"], reads: &["t1"], writes: &["t2"] }),
            Box::new(MockPass { name: "c", deps: &["b"], reads: &["t2"], writes: &["t3"] }),
        ];

        let order = resolve_order(&passes, "c").unwrap();
        assert_eq!(order, vec!["a", "b", "c"]);
    }

    #[test]
    fn resolve_order_no_deps() {
        let passes: Vec<Box<dyn EnrichmentPass>> = vec![
            Box::new(MockPass { name: "x", deps: &[], reads: &[], writes: &["t1"] }),
        ];

        let order = resolve_order(&passes, "x").unwrap();
        assert_eq!(order, vec!["x"]);
    }

    #[test]
    fn resolve_order_unknown_pass_errors() {
        let passes: Vec<Box<dyn EnrichmentPass>> = vec![];
        assert!(resolve_order(&passes, "missing").is_err());
    }

    #[test]
    fn resolve_order_diamond_deps() {
        let passes: Vec<Box<dyn EnrichmentPass>> = vec![
            Box::new(MockPass { name: "base", deps: &[], reads: &[], writes: &["t0"] }),
            Box::new(MockPass { name: "left", deps: &["base"], reads: &["t0"], writes: &["t1"] }),
            Box::new(MockPass { name: "right", deps: &["base"], reads: &["t0"], writes: &["t2"] }),
            Box::new(MockPass { name: "top", deps: &["left", "right"], reads: &["t1", "t2"], writes: &["t3"] }),
        ];

        let order = resolve_order(&passes, "top").unwrap();
        // base must come first, top must come last
        assert_eq!(order[0], "base");
        assert_eq!(order[order.len() - 1], "top");
        assert!(order.contains(&"left".to_string()));
        assert!(order.contains(&"right".to_string()));
    }
}
