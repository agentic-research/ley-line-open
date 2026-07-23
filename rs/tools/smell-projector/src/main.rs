//! smell-projector — projects `rs/` into a SQLite schema so mache-shaped
//! smell rules can query it. Four tables today:
//!
//! - `workspace_deps` — deps declared in `[workspace.dependencies]`.
//! - `crate_deps` — per-crate deps parsed from `rs/**/Cargo.toml`.
//!   Drives the `workspace_deps_drift` rule (bead `ley-line-open-3b2f55`
//!   Phase 3): fail on any crate that pins a literal version for a dep
//!   already declared in `[workspace.dependencies]`.
//! - `unsafe_sites` — every `unsafe` occurrence in `rs/*/src/**/*.rs`,
//!   with a `has_safety` flag. Drives the `unsafe_without_safety` rule
//!   (bead `ley-line-open-85fb1f`).
//! - `unwrap_sites` — every bare `.unwrap()` in production code. Drives
//!   the `unwrap_without_expect` rule (bead `ley-line-open-85fb1f`
//!   follow-up).
//!
//! Deliberately mirrors mache's rule + baseline format so the projector
//! is drop-in replaceable by mache proper if we ever want to swap the
//! engine — same JSON rule shape (`ID`, `Description`, `Requires`,
//! `ScopeColumn`, `Query` with `%s` scope placeholder), same
//! `docs/smell-baseline.json` shape (`version: 1`, `counts: [{rule_id,
//! source_id, count}]`).
//!
//! Renamed from `cargo-toml-projector` once the source-projection tables
//! landed — the old name lied about scope.

use anyhow::{Context, Result, anyhow};
use clap::Parser;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use toml_edit::{DocumentMut, Item, Table, Value};
use walkdir::WalkDir;

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

#[derive(Parser, Debug)]
#[command(
    author,
    version,
    about = "Project rs/ (Cargo.toml + *.rs) into SQLite + run mache-shaped smell rules"
)]
struct Cli {
    /// Repo root (where `rs/Cargo.toml`, `smell-rules/`, `docs/smell-baseline.json` live).
    #[arg(long, default_value = ".")]
    repo_root: PathBuf,

    /// Rust workspace subdirectory (relative to `--repo-root`).
    #[arg(long, default_value = "rs")]
    workspace: PathBuf,

    /// Directory of *.json smell-rule definitions (relative to `--repo-root`).
    #[arg(long, default_value = "smell-rules")]
    rules_dir: PathBuf,

    /// Committed baseline JSON (relative to `--repo-root`).
    #[arg(long, default_value = "docs/smell-baseline.json")]
    baseline: PathBuf,

    /// Optional path to write the projection SQLite DB to (default: in-memory).
    #[arg(long)]
    out_db: Option<PathBuf>,

    /// Regenerate the baseline from current findings (no gate).
    #[arg(long, conflicts_with = "dogfood")]
    write_baseline: bool,

    /// Print all findings and exit 0 (no gate; advisory mode).
    #[arg(long)]
    dogfood: bool,
}

// ---------------------------------------------------------------------------
// Rule + baseline formats — MUST match mache's shape exactly. See
// `~/remotes/art/mache/examples/smell-rules/long_unexported_function.json`
// and `~/remotes/art/mache/docs/smell-baseline.json`.
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
#[allow(non_snake_case)]
struct Rule {
    ID: String,
    #[allow(dead_code)]
    Description: String,
    Requires: Vec<String>,
    #[allow(dead_code)]
    ScopeColumn: String,
    /// SQL with a `%s` placeholder for an optional scope filter clause.
    Query: String,
}

#[derive(Debug, Serialize, Deserialize, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct BaselineCount {
    rule_id: String,
    source_id: String,
    count: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct Baseline {
    version: u32,
    counts: Vec<BaselineCount>,
}

impl Baseline {
    fn empty() -> Self {
        Self {
            version: 1,
            counts: Vec::new(),
        }
    }
    fn from_findings(findings: &BTreeMap<(String, String), u64>) -> Self {
        let mut counts: Vec<BaselineCount> = findings
            .iter()
            .map(|((rule_id, source_id), count)| BaselineCount {
                rule_id: rule_id.clone(),
                source_id: source_id.clone(),
                count: *count,
            })
            .collect();
        counts.sort();
        Self { version: 1, counts }
    }
    fn to_map(&self) -> BTreeMap<(String, String), u64> {
        self.counts
            .iter()
            .map(|c| ((c.rule_id.clone(), c.source_id.clone()), c.count))
            .collect()
    }
}

// ---------------------------------------------------------------------------
// Projection: workspace_deps + crate_deps
// ---------------------------------------------------------------------------

fn schema() -> &'static str {
    r#"
        CREATE TABLE workspace_deps (
            dep_name TEXT PRIMARY KEY,
            version  TEXT
        );

        CREATE TABLE crate_deps (
            crate            TEXT NOT NULL,
            section          TEXT NOT NULL,
            dep_name         TEXT NOT NULL,
            version_literal  TEXT,
            workspace_true   INTEGER NOT NULL,
            features         TEXT,
            source_id        TEXT NOT NULL,
            node_id          TEXT NOT NULL,
            start_row        INTEGER NOT NULL,
            start_col        INTEGER NOT NULL,
            end_row          INTEGER NOT NULL,
            end_col          INTEGER NOT NULL,
            PRIMARY KEY (crate, section, dep_name)
        );

        -- Per-file `unsafe` sites in `rs/*/src/**/*.rs`. `has_safety = 1`
        -- when a `// SAFETY:` comment or `# Safety` docstring appears in
        -- the preceding 5 lines; else 0. tests/, benches/, comment-only
        -- lines, and `#[cfg(test)]` subtrees are filtered out at
        -- projection time so the query is a pure `WHERE has_safety = 0`.
        -- Bead `ley-line-open-85fb1f`.
        CREATE TABLE unsafe_sites (
            source_id    TEXT NOT NULL,
            node_id      TEXT NOT NULL,
            start_row    INTEGER NOT NULL,
            start_col    INTEGER NOT NULL,
            end_row      INTEGER NOT NULL,
            end_col      INTEGER NOT NULL,
            has_safety   INTEGER NOT NULL,
            PRIMARY KEY (source_id, start_row, start_col)
        );

        -- Per-file bare `.unwrap()` sites in `rs/*/src/**/*.rs`. Only the
        -- panic-on-None/Err form (`x.unwrap()`) counts — the fallback
        -- variants (`unwrap_or`, `unwrap_or_else`, `unwrap_or_default`,
        -- `unwrap_err`) are safe and skipped. tests/, benches/,
        -- comment-only lines, `.unwrap()` inside string literals, and
        -- `#[cfg(test)]` module subtrees are filtered at projection
        -- time. `.expect("msg")` is Rust's canonical annotation for a
        -- panicking accessor and is NOT flagged. Bead 85fb1f follow-up.
        -- A `thread::sleep` in a test is a race against the scheduler: under
        -- load the machine loses and the test fails with nothing broken.
        -- `fs_concurrent_put_get_interleaved` flaked CI on a licensing PR that
        -- touched zero code. Bead `ley-line-open-c6101e`. Baselined rather
        -- than gated at zero — some remaining ones are poll-until-ready
        -- backoffs that need a readiness predicate, not deletion.
        CREATE TABLE sleep_sites (
            source_id    TEXT NOT NULL,
            node_id      TEXT NOT NULL,
            start_row    INTEGER NOT NULL,
            start_col    INTEGER NOT NULL,
            end_row      INTEGER NOT NULL,
            end_col      INTEGER NOT NULL,
            PRIMARY KEY (source_id, start_row, start_col)
        );

        CREATE TABLE unwrap_sites (
            source_id    TEXT NOT NULL,
            node_id      TEXT NOT NULL,
            start_row    INTEGER NOT NULL,
            start_col    INTEGER NOT NULL,
            end_row      INTEGER NOT NULL,
            end_col      INTEGER NOT NULL,
            PRIMARY KEY (source_id, start_row, start_col)
        );
    "#
}

/// Byte-offset → (line, col) lookup for a single file.
struct LineIndex {
    /// Sorted byte offsets of every `\n`. `line_starts[i]` is the byte
    /// offset of the START of line `i+1` (line 0 starts at 0).
    line_starts: Vec<usize>,
}

impl LineIndex {
    fn new(text: &str) -> Self {
        let mut line_starts = vec![0usize];
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                line_starts.push(i + 1);
            }
        }
        Self { line_starts }
    }
    /// Returns `(row, col)` — both 0-indexed byte offsets within the file.
    fn locate(&self, byte: usize) -> (u32, u32) {
        // Binary search for the largest `line_starts[i] <= byte`.
        let idx = match self.line_starts.binary_search(&byte) {
            Ok(i) => i,
            Err(i) => i.saturating_sub(1),
        };
        let line_start = self.line_starts[idx];
        (idx as u32, (byte - line_start) as u32)
    }
}

struct DepRow {
    crate_name: String,
    section: String,
    dep_name: String,
    version_literal: Option<String>,
    workspace_true: bool,
    features: Option<String>,
    source_id: String,
    node_id: String,
    start_row: u32,
    start_col: u32,
    end_row: u32,
    end_col: u32,
}

/// Root workspace Cargo.toml parsed once — carries the `[workspace.dependencies]`
/// canonical list.
struct Workspace {
    root: PathBuf,
    members: Vec<PathBuf>,
    /// `dep_name → version_literal` for entries in `[workspace.dependencies]`.
    /// `None` when the entry is a table but has no explicit `version` field
    /// (e.g. `serde = { features = ["derive"] }` — unusual but possible).
    deps: BTreeMap<String, Option<String>>,
}

fn parse_workspace(workspace_root: &Path) -> Result<Workspace> {
    let root_manifest_path = workspace_root.join("Cargo.toml");
    let text = fs::read_to_string(&root_manifest_path)
        .with_context(|| format!("read {}", root_manifest_path.display()))?;
    let doc: DocumentMut = text
        .parse()
        .with_context(|| format!("parse {}", root_manifest_path.display()))?;

    let ws = doc
        .as_table()
        .get("workspace")
        .and_then(Item::as_table)
        .ok_or_else(|| {
            anyhow!(
                "[workspace] table missing in {}",
                root_manifest_path.display()
            )
        })?;

    let mut members = Vec::new();
    if let Some(m) = ws.get("members").and_then(Item::as_array) {
        for entry in m.iter() {
            let s = entry
                .as_str()
                .ok_or_else(|| anyhow!("non-string workspace member entry"))?;
            members.push(PathBuf::from(s));
        }
    }

    let mut deps = BTreeMap::new();
    if let Some(t) = ws.get("dependencies").and_then(Item::as_table) {
        for (name, item) in t.iter() {
            deps.insert(name.to_string(), extract_version(item));
        }
    }

    Ok(Workspace {
        root: workspace_root.to_path_buf(),
        members,
        deps,
    })
}

/// Extract the version-literal string from a dep entry.
///  - `dep = "1.0"` → Some("1.0")
///  - `dep = { version = "1.0", ... }` → Some("1.0")
///  - `dep = { workspace = true, ... }` → None (caller handles workspace_true)
///  - `dep = { path = "..." }` → None
fn extract_version(item: &Item) -> Option<String> {
    match item {
        Item::Value(Value::String(s)) => Some(s.value().to_string()),
        Item::Value(Value::InlineTable(t)) => t.get("version").and_then(|v| match v {
            Value::String(s) => Some(s.value().to_string()),
            _ => None,
        }),
        Item::Table(t) => t.get("version").and_then(|i| match i {
            Item::Value(Value::String(s)) => Some(s.value().to_string()),
            _ => None,
        }),
        _ => None,
    }
}

fn is_workspace_true(item: &Item) -> bool {
    match item {
        Item::Value(Value::InlineTable(t)) => t
            .get("workspace")
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        Item::Table(t) => t
            .get("workspace")
            .and_then(|i| i.as_value())
            .and_then(|v| v.as_bool())
            .unwrap_or(false),
        _ => false,
    }
}

fn extract_features(item: &Item) -> Option<String> {
    let features: Option<Vec<String>> = match item {
        Item::Value(Value::InlineTable(t)) => t.get("features").and_then(|v| match v {
            Value::Array(a) => Some(
                a.iter()
                    .filter_map(|e| e.as_str().map(|s| s.to_string()))
                    .collect(),
            ),
            _ => None,
        }),
        Item::Table(t) => t.get("features").and_then(|i| match i {
            Item::Value(Value::Array(a)) => Some(
                a.iter()
                    .filter_map(|e| e.as_str().map(|s| s.to_string()))
                    .collect(),
            ),
            _ => None,
        }),
        _ => None,
    };
    features.and_then(|f| {
        if f.is_empty() {
            None
        } else {
            Some(f.join(","))
        }
    })
}

/// Enumerate all Cargo.toml files under workspace members. Excludes the
/// root manifest itself and any manifest under a directory segment named
/// `tests` (test fixture crates are not workspace members).
fn enumerate_member_manifests(workspace: &Workspace) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for member in &workspace.members {
        // Members can be globs like `ll-open/*`. Expand by scanning the
        // parent directory for immediate subdirs when the last component
        // is `*`.
        let member_path = workspace.root.join(member);
        if member.file_name().and_then(|s| s.to_str()) == Some("*") {
            let parent = member_path
                .parent()
                .ok_or_else(|| anyhow!("member glob without parent: {}", member.display()))?;
            if !parent.is_dir() {
                continue;
            }
            for entry in
                fs::read_dir(parent).with_context(|| format!("read_dir {}", parent.display()))?
            {
                let entry = entry?;
                let path = entry.path();
                if path.is_dir() {
                    let manifest = path.join("Cargo.toml");
                    if manifest.is_file() && !contains_tests_segment(&manifest) {
                        out.push(manifest);
                    }
                }
            }
        } else if member_path.is_dir() {
            let manifest = member_path.join("Cargo.toml");
            if manifest.is_file() && !contains_tests_segment(&manifest) {
                out.push(manifest);
            }
        }
    }
    out.sort();
    Ok(out)
}

fn contains_tests_segment(path: &Path) -> bool {
    path.components()
        .any(|c| c.as_os_str() == "tests" || c.as_os_str() == "target")
}

/// Load `[workspace.dependencies]` into the SQLite table.
fn insert_workspace_deps(conn: &Connection, workspace: &Workspace) -> Result<()> {
    let mut stmt =
        conn.prepare("INSERT INTO workspace_deps (dep_name, version) VALUES (?1, ?2)")?;
    for (name, version) in &workspace.deps {
        stmt.execute(params![name, version])?;
    }
    Ok(())
}

/// Parse a single Cargo.toml and emit dep rows for `[dependencies]`,
/// `[dev-dependencies]`, `[build-dependencies]`, and their target-specific
/// variants under `[target.*.]`.
fn project_crate(manifest_path: &Path, repo_root: &Path) -> Result<Vec<DepRow>> {
    let text = fs::read_to_string(manifest_path)
        .with_context(|| format!("read {}", manifest_path.display()))?;
    let doc: DocumentMut = text
        .parse()
        .with_context(|| format!("parse {}", manifest_path.display()))?;
    let index = LineIndex::new(&text);
    let text_ref = text.as_str();

    let crate_name = doc
        .as_table()
        .get("package")
        .and_then(Item::as_table)
        .and_then(|t| t.get("name"))
        .and_then(|i| i.as_str())
        .unwrap_or("<unknown>")
        .to_string();

    let source_id = manifest_path
        .strip_prefix(repo_root)
        .unwrap_or(manifest_path)
        .to_string_lossy()
        .to_string();

    let mut out = Vec::new();

    // Top-level dep sections.
    for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
        if let Some(table) = doc.as_table().get(section).and_then(Item::as_table) {
            let section_hint = section_line_hint(text_ref, section);
            collect_section(
                table,
                section,
                &crate_name,
                &source_id,
                text_ref,
                &index,
                section_hint,
                &mut out,
            );
        }
    }

    // Target-specific sections: [target.'cfg(...)'.dependencies] etc.
    if let Some(target) = doc.as_table().get("target").and_then(Item::as_table) {
        for (target_key, target_item) in target.iter() {
            let target_table = match target_item.as_table() {
                Some(t) => t,
                None => continue,
            };
            for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
                if let Some(inner) = target_table.get(section).and_then(Item::as_table) {
                    let scoped_section = format!("target.{}.{}", target_key, section);
                    // Header form: [target.'<key>'.<section>] — we match on
                    // `.<section>]` at the end of a line as a cheap hint.
                    let section_hint = target_section_line_hint(text_ref, target_key, section);
                    collect_section(
                        inner,
                        &scoped_section,
                        &crate_name,
                        &source_id,
                        text_ref,
                        &index,
                        section_hint,
                        &mut out,
                    );
                }
            }
        }
    }

    Ok(out)
}

/// Find the line index (0-based) where `[<section>]` appears, so text
/// search for a dep starts AFTER the section header (and doesn't collide
/// with an entry of the same name in an earlier section).
fn section_line_hint(text: &str, section: &str) -> u32 {
    let needle = format!("[{}]", section);
    for (line_no, line) in text.lines().enumerate() {
        if line.trim() == needle {
            return line_no as u32;
        }
    }
    0
}

fn target_section_line_hint(text: &str, target_key: &str, section: &str) -> u32 {
    // Match `[target.<key>.<section>]` with either quoted or bare key.
    let suffix = format!(".{}]", section);
    for (line_no, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("[target.")
            && trimmed.ends_with(&suffix)
            && trimmed.contains(target_key)
        {
            return line_no as u32;
        }
    }
    0
}

#[allow(clippy::too_many_arguments)]
fn collect_section(
    table: &Table,
    section: &str,
    crate_name: &str,
    source_id: &str,
    text: &str,
    index: &LineIndex,
    section_start_line: u32,
    out: &mut Vec<DepRow>,
) {
    for (dep_name, item) in table.iter() {
        let version_literal = extract_version(item);
        let workspace_true = is_workspace_true(item);
        let features = extract_features(item);
        let (start_row, start_col, end_row, end_col) =
            span_of(table, dep_name, item, text, index, section_start_line);
        out.push(DepRow {
            crate_name: crate_name.to_string(),
            section: section.to_string(),
            dep_name: dep_name.to_string(),
            version_literal,
            workspace_true,
            features,
            source_id: source_id.to_string(),
            node_id: format!("{}:{}:{}", crate_name, section, dep_name),
            start_row,
            start_col,
            end_row,
            end_col,
        });
    }
}

/// Best-effort location.
///  1. Prefer toml_edit's `Item::span()` — accurate for standard tables.
///  2. Fall back to `Key::span()` on the containing table.
///  3. Last-ditch: scan `text` for a line matching `<dep_name>\s*=` at the
///     start of a line (allowing whitespace). Reliable for inline entries
///     in `[dependencies]` blocks where toml_edit's spans are absent.
fn span_of(
    table: &Table,
    dep_name: &str,
    item: &Item,
    text: &str,
    index: &LineIndex,
    section_start_line: u32,
) -> (u32, u32, u32, u32) {
    let range = item
        .span()
        .or_else(|| table.key(dep_name).and_then(|k| k.span()));
    if let Some(r) = range {
        let (sr, sc) = index.locate(r.start);
        let (er, ec) = index.locate(r.end.saturating_sub(1));
        return (sr, sc, er, ec);
    }
    // Text-search fallback: scan lines AFTER `section_start_line` for
    // `<dep_name>\s*=`. Scoping to the section prevents false matches when
    // the same dep appears in multiple sections (tokio in dependencies +
    // dev-dependencies).
    let quoted = format!("\"{}\"", dep_name);
    let start = section_start_line as usize;
    for (line_no, line) in text.lines().enumerate().skip(start + 1) {
        let trimmed = line.trim_start();
        // Stop at the next section header — otherwise we'd cross into a
        // sibling section.
        if trimmed.starts_with('[') && trimmed.ends_with(']') {
            break;
        }
        let (matched, name_len) = if let Some(after) = trimmed.strip_prefix(dep_name) {
            (after.trim_start().starts_with('='), dep_name.len())
        } else if let Some(after) = trimmed.strip_prefix(&quoted) {
            (after.trim_start().starts_with('='), quoted.len())
        } else {
            (false, 0)
        };
        if !matched {
            continue;
        }
        let leading_ws = line.len() - trimmed.len();
        let start_col = leading_ws as u32;
        let end_col = (leading_ws + name_len).saturating_sub(1) as u32;
        return (line_no as u32, start_col, line_no as u32, end_col);
    }
    (0, 0, 0, 0)
}

fn insert_crate_deps(conn: &Connection, rows: &[DepRow]) -> Result<()> {
    let mut stmt = conn.prepare(
        "INSERT INTO crate_deps (
            crate, section, dep_name, version_literal, workspace_true,
            features, source_id, node_id, start_row, start_col, end_row, end_col
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
    )?;
    for r in rows {
        stmt.execute(params![
            r.crate_name,
            r.section,
            r.dep_name,
            r.version_literal,
            r.workspace_true as i64,
            r.features,
            r.source_id,
            r.node_id,
            r.start_row,
            r.start_col,
            r.end_row,
            r.end_col,
        ])?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Projection: unsafe_sites (rs/*/src/**/*.rs) — bead ley-line-open-85fb1f
// ---------------------------------------------------------------------------

struct UnsafeRow {
    source_id: String,
    node_id: String,
    start_row: u32,
    start_col: u32,
    end_row: u32,
    end_col: u32,
    has_safety: bool,
}

/// Walk `rs/*/src/**/*.rs` and emit one row per `unsafe` occurrence
/// that is NOT under `tests/`, `benches/`, an `#[cfg(test)]` subtree,
/// or a comment line. The `has_safety` flag is true when either
///   - `// SAFETY:` or `SAFETY:` appears in the preceding 5 lines, OR
///   - `# Safety` (rustdoc section header) appears in the preceding
///     5 lines (for `unsafe fn`/`unsafe impl` docstrings).
///
/// Rustdoc convention: `# Safety` is the canonical H1 for documenting
/// the invariant of an unsafe function; `// SAFETY:` is the canonical
/// inline comment for justifying an `unsafe { }` block. Either counts.
fn project_unsafe_sites(workspace_root: &Path, repo_root: &Path) -> Result<Vec<UnsafeRow>> {
    let mut rows = Vec::new();
    for entry in WalkDir::new(workspace_root)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("rs") {
            continue;
        }
        let s = path.to_string_lossy();
        // Only `src/` — skip tests/, benches/, target/, examples/.
        if !s.contains("/src/") {
            continue;
        }
        if s.contains("/target/") || s.contains("/tests/") || s.contains("/benches/") {
            continue;
        }
        // Skip files that the parent module gates behind `#[cfg(test)]
        // mod <basename>;` — the whole file is test-only.
        if is_file_cfg_test_gated(path) {
            continue;
        }

        let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let lines: Vec<&str> = text.lines().collect();

        // Per-file cutoff: first line where a MODULE-LEVEL
        // `#[cfg(test)]` gates a `mod tests { ... }` block. Everything
        // at or below is test code.
        //
        // Per-fn `#[cfg(test)]` attributes (indented, gate a single
        // fn, not a module) MUST NOT trigger the cutoff — otherwise
        // every unsafe below a test-only helper would be silently
        // exempted from the gate. Bug caught by external review of
        // `control.rs` where line 217's `#[cfg(test)]` on a per-fn
        // helper hid 8 feature-gated interrupt-accessor unsafe blocks
        // at lines 357–416 from the projection.
        let cfg_test_cutoff = find_test_module_line(&lines).unwrap_or(usize::MAX);

        for (idx, line) in lines.iter().enumerate() {
            let line_no = idx + 1;
            if line_no >= cfg_test_cutoff {
                break;
            }

            // `\bunsafe\b` — presence check. Skip comment-only lines
            // (leading `//`, `///`, `//!`, `*`).
            if !contains_unsafe_keyword(line) {
                continue;
            }
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") || trimmed.starts_with("*") {
                continue;
            }
            // Type-level unsafe in a fn-pointer type: `unsafe extern "C" fn(`
            // or `unsafe fn(` with NO name between `fn` and `(`. These are
            // type-signature keywords, not runtime unsafe uses, and don't
            // need a SAFETY comment on their own — the containing unsafe
            // block (transmute call site) carries the invariant.
            if is_type_level_unsafe_fn(line) {
                continue;
            }

            // SAFETY check with two contexts:
            //   1. `// SAFETY:` on any of the preceding 5 lines
            //      (canonical convention for `unsafe { }` blocks).
            //   2. `# Safety` heading in the consecutive `///`/`//!`
            //      doc-comment block immediately above (rustdoc
            //      convention for `unsafe fn` / `unsafe impl` — the
            //      block can be arbitrarily long, so a fixed-lines
            //      window misses it).
            let start_ctx = idx.saturating_sub(5);
            let inline_safety = lines[start_ctx..idx].iter().any(|l| l.contains("SAFETY:"));
            let doc_safety = doc_block_ending_at(&lines, idx)
                .iter()
                .any(|l| contains_safety_heading(l));
            let has_safety = inline_safety || doc_safety;

            let col = line.find("unsafe").unwrap_or(0);
            let src_id = path
                .strip_prefix(repo_root)
                .unwrap_or(path)
                .to_string_lossy()
                .to_string();
            rows.push(UnsafeRow {
                node_id: format!("{}:{}:{}", src_id, line_no, col),
                source_id: src_id,
                start_row: line_no as u32,
                start_col: col as u32,
                end_row: line_no as u32,
                end_col: (col + "unsafe".len()) as u32,
                has_safety,
            });
        }
    }
    Ok(rows)
}

/// True iff `line` contains the `unsafe` keyword as a real token
/// outside any string literal or line comment. Reject false positives:
///   - `_unsafe`, `unsafe_foo`      — ident boundary
///   - `"unsafe"`, `b"unsafe"`      — double-quoted string
///   - `` `unsafe` ``               — markdown-in-comment backtick
///   - `// unsafe {...}`            — anywhere after `//`
///
/// Doesn't handle raw strings (`r#"..."#`) perfectly, but the byte
/// literal + basic double-quote handling covers every false positive
/// observed in this workspace.
fn contains_unsafe_keyword(line: &str) -> bool {
    let bytes = line.as_bytes();
    let needle = b"unsafe";
    let mut i = 0;
    let mut in_string = false;
    while i + needle.len() <= bytes.len() {
        let b = bytes[i];
        // Stop at line comment start.
        if !in_string && b == b'/' && bytes.get(i + 1) == Some(&b'/') {
            return false;
        }
        // Toggle string state on unescaped `"`. Also treats backtick
        // (used in `//` doc-comment markdown to quote `unsafe`) as a
        // string boundary — those lines are already filtered by the
        // caller's leading-`//` check, but backtick still helps for
        // block-comment lines that don't start with `//`.
        if b == b'"' && (i == 0 || bytes[i - 1] != b'\\') {
            in_string = !in_string;
        }
        if b == b'`' && !in_string {
            // Any `unsafe` between backticks on this line is prose.
            // Skip past the next backtick.
            if let Some(off) = bytes[i + 1..].iter().position(|&c| c == b'`') {
                i += off + 2;
                continue;
            }
        }
        if !in_string && &bytes[i..i + needle.len()] == needle {
            let before_ok = i == 0 || !is_ident_byte(bytes[i - 1]);
            let after = bytes.get(i + needle.len()).copied().unwrap_or(b' ');
            let after_ok = !is_ident_byte(after);
            if before_ok && after_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

fn is_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Return the 1-indexed line number where a MODULE-LEVEL cfg-test
/// predicate gates a `mod <name> { ... }` block — the earliest such
/// line, if any. Everything at-or-below is treated as test code by
/// the unsafe/SAFETY + unwrap projectors.
///
/// Recognizes:
///   - `#[cfg(test)]`
///   - `#[cfg(all(test, ...))]`
///   - `#[cfg(any(test, ...))]`
///   - `#[cfg(...anything...test...)]` in general (substring match on
///     `test` after `cfg(` — safe because Rust cfg predicates don't
///     nest `test` in string literals).
///
/// A per-fn cfg-test attribute (typically indented, followed by `fn ...`
/// not `mod ...`) is NOT a cutoff — it gates a single item, not a
/// whole tail of the file, so items after it are still production.
fn find_test_module_line(lines: &[&str]) -> Option<usize> {
    for (i, l) in lines.iter().enumerate() {
        if !is_cfg_test_attr(l) {
            continue;
        }
        // Walk forward through blank / attribute lines to find the
        // next item declaration. Cutoff fires only if it's `mod`.
        for candidate in lines.iter().skip(i + 1) {
            let t = candidate.trim_start();
            if t.is_empty() || t.starts_with("//") || t.starts_with("#[") {
                continue;
            }
            let starts_mod = t.starts_with("mod ")
                || t.starts_with("pub mod ")
                || t.starts_with("pub(crate) mod ")
                || t.starts_with("pub(super) mod ");
            if starts_mod {
                return Some(i + 1);
            }
            // Any other item → this cfg-test attribute gates a single
            // fn/const/etc; NOT a cutoff. Keep scanning for a later
            // module-level one.
            break;
        }
    }
    None
}

/// True iff `line` is a `#[cfg(...)]` attribute whose predicate
/// references the `test` cfg — matching `#[cfg(test)]`,
/// `#[cfg(all(test, ...))]`, `#[cfg(any(test, ...))]`, and other
/// nested combinations.
fn is_cfg_test_attr(line: &str) -> bool {
    let t = line.trim_start();
    let Some(rest) = t.strip_prefix("#[cfg(") else {
        return false;
    };
    // Reject the closing `)]` if it's the immediate follow (empty pred).
    // Predicate substring ends at the matching `)]`.
    let Some(end) = rest.rfind(")]") else {
        return false;
    };
    let pred = &rest[..end];
    // Match `test` as a bare identifier (avoid matching `test_foo`).
    let bytes = pred.as_bytes();
    let needle = b"test";
    let mut i = 0;
    while i + needle.len() <= bytes.len() {
        if &bytes[i..i + needle.len()] == needle {
            let before_ok = i == 0 || !is_ident_byte(bytes[i - 1]);
            let after = bytes.get(i + needle.len()).copied().unwrap_or(b' ');
            let after_ok = !is_ident_byte(after);
            if before_ok && after_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

/// True iff this file (`file_path`) is `#[cfg(test)]`-gated in its
/// parent module — i.e., the parent `lib.rs` or `mod.rs` contains
/// `#[cfg(test)]\n[pub ]?mod <basename>;`. Files gated this way are
/// entirely test-only; every unwrap/unsafe in them should be
/// excluded from the projections.
///
/// Best-effort text scan: reads the parent module file and checks
/// for the pattern. Returns `false` if the parent can't be resolved.
fn is_file_cfg_test_gated(file_path: &Path) -> bool {
    let Some(parent_dir) = file_path.parent() else {
        return false;
    };
    let Some(stem) = file_path.file_stem().and_then(|s| s.to_str()) else {
        return false;
    };
    // If this IS a lib.rs / mod.rs / main.rs, no parent to check.
    if stem == "lib" || stem == "mod" || stem == "main" {
        return false;
    }
    let parent_candidates = [
        parent_dir.join("mod.rs"),
        parent_dir.join("lib.rs"),
        parent_dir.join("main.rs"),
    ];
    for parent in &parent_candidates {
        let Ok(text) = fs::read_to_string(parent) else {
            continue;
        };
        let lines: Vec<&str> = text.lines().collect();
        for (i, l) in lines.iter().enumerate() {
            if !is_cfg_test_attr(l) {
                continue;
            }
            // Look at the next non-blank / non-attr line.
            for candidate in lines.iter().skip(i + 1) {
                let t = candidate.trim_start();
                if t.is_empty() || t.starts_with("//") || t.starts_with("#[") {
                    continue;
                }
                // Match `[pub[(...)]* ]mod <stem>;`
                let after_pub = t
                    .strip_prefix("pub(crate) ")
                    .or_else(|| t.strip_prefix("pub(super) "))
                    .or_else(|| t.strip_prefix("pub "))
                    .unwrap_or(t);
                let Some(after_mod) = after_pub.strip_prefix("mod ") else {
                    break;
                };
                let name = after_mod
                    .trim_end_matches(';')
                    .trim_end()
                    .trim_end_matches('{')
                    .trim();
                if name == stem {
                    return true;
                }
                break;
            }
        }
        return false;
    }
    false
}

/// True iff the line's `unsafe` appears in a fn-pointer TYPE position:
///   `unsafe extern "C" fn(...)`  — nameless fn type (e.g. in `transmute::<...>`)
///   `unsafe fn(...)`             — nameless fn type
/// Real fn DECLARATIONS always have an ident between `fn` and `(`
///   `unsafe fn foo()`, `unsafe extern "C" fn bar(...)`.
/// Type-position occurrences don't need their own SAFETY comment; the
/// surrounding `unsafe { }` block (transmute call site) carries it.
///
/// Works at any position in the line (e.g. `type Cb = unsafe extern ...`).
fn is_type_level_unsafe_fn(line: &str) -> bool {
    // Locate the `unsafe` keyword; then walk forward.
    let idx = match line.find("unsafe") {
        Some(i) => i,
        None => return false,
    };
    let rest = &line[idx + "unsafe".len()..];
    let rest = rest.trim_start();
    // Optional `extern "..."` prefix.
    let after_extern = if let Some(after_kw) = rest.strip_prefix("extern") {
        let after = after_kw.trim_start();
        match after.strip_prefix('"') {
            Some(inside) => match inside.find('"') {
                Some(end) => inside[end + 1..].trim_start(),
                None => return false,
            },
            None => return false,
        }
    } else {
        rest
    };
    // Expect `fn(` (nameless — type-level) OR `fn <ident>(` (decl).
    if let Some(after_fn) = after_extern.strip_prefix("fn") {
        let after_fn = after_fn.trim_start_matches(|c: char| c.is_whitespace());
        after_fn.starts_with('(')
    } else {
        false
    }
}

/// True iff `line` contains a rustdoc `# Safety` section header (or
/// `## Safety` etc.).
fn contains_safety_heading(line: &str) -> bool {
    let t = line.trim_start_matches(|c: char| c == '/' || c == '!' || c.is_whitespace());
    t.starts_with("# Safety") || t.starts_with("## Safety") || t.starts_with("### Safety")
}

/// Return the slice of `lines` that forms the consecutive
/// doc-comment block ending immediately before `idx` (exclusive).
/// A "doc-comment line" is one whose first non-whitespace chars are
/// `///` (outer doc) or `//!` (inner doc). Skips at most one blank
/// line between the doc block and the target line so patterns like:
///
/// ```text
/// /// # Safety
/// /// ...docs...
/// #[unsafe(no_mangle)]        // annotated item; blank not needed
/// pub unsafe extern "C" fn ...
/// ```
///
/// still bind the docstring to the item declaration line.
fn doc_block_ending_at<'a>(lines: &'a [&'a str], idx: usize) -> &'a [&'a str] {
    if idx == 0 {
        return &[];
    }
    // Walk backward from idx-1, allowing attribute lines (`#[...]`)
    // and up to one blank between the doc block and the target.
    let mut cursor = idx;
    while cursor > 0 {
        let l = lines[cursor - 1].trim_start();
        if l.starts_with("///") || l.starts_with("//!") {
            break;
        }
        if l.is_empty() || l.starts_with("#[") || l.starts_with("#![") {
            cursor -= 1;
            continue;
        }
        return &[];
    }
    let end = cursor;
    while cursor > 0 {
        let l = lines[cursor - 1].trim_start();
        if l.starts_with("///") || l.starts_with("//!") {
            cursor -= 1;
        } else {
            break;
        }
    }
    &lines[cursor..end]
}

fn insert_unsafe_sites(conn: &Connection, rows: &[UnsafeRow]) -> Result<()> {
    let mut stmt = conn.prepare(
        "INSERT OR REPLACE INTO unsafe_sites (
            source_id, node_id, start_row, start_col, end_row, end_col, has_safety
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?;
    for r in rows {
        stmt.execute(params![
            r.source_id,
            r.node_id,
            r.start_row,
            r.start_col,
            r.end_row,
            r.end_col,
            r.has_safety as i64,
        ])?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Projection: unwrap_sites (rs/*/src/**/*.rs) — bead ley-line-open-85fb1f
// follow-up (unwrap density audit).
// ---------------------------------------------------------------------------

struct UnwrapRow {
    source_id: String,
    node_id: String,
    start_row: u32,
    start_col: u32,
    end_row: u32,
    end_col: u32,
}

/// Walk `rs/*/src/**/*.rs` and emit one row per literal `.unwrap()`
/// call that is NOT under `tests/`, `benches/`, an `#[cfg(test)]`
/// module subtree, or a comment/string literal. Rejects the
/// non-panicking fallback variants:
///   `.unwrap_or(...)`, `.unwrap_or_else(...)`, `.unwrap_or_default()`,
/// and `.unwrap_err()` (which is the Result-inverse used in tests).
///
/// `.expect("<msg>")` is Rust's canonical annotation for a panicking
/// accessor and is intentionally NOT flagged — it plays the same role
/// as `// SAFETY:` does for `unsafe`. Legacy `.unwrap()` sites should
/// be converted to `.expect("<invariant>")` or `?`-propagated.
fn project_unwrap_sites(workspace_root: &Path, repo_root: &Path) -> Result<Vec<UnwrapRow>> {
    let mut rows = Vec::new();
    for entry in WalkDir::new(workspace_root)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("rs") {
            continue;
        }
        let s = path.to_string_lossy();
        if !s.contains("/src/") {
            continue;
        }
        if s.contains("/target/") || s.contains("/tests/") || s.contains("/benches/") {
            continue;
        }
        // Skip files that the parent module gates behind `#[cfg(test)]
        // mod <basename>;` — the whole file is test-only.
        if is_file_cfg_test_gated(path) {
            continue;
        }

        let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let lines: Vec<&str> = text.lines().collect();
        let cfg_test_cutoff = find_test_module_line(&lines).unwrap_or(usize::MAX);
        let inside_raw_string = raw_string_line_mask(&text);

        for (idx, line) in lines.iter().enumerate() {
            let line_no = idx + 1;
            if line_no >= cfg_test_cutoff {
                break;
            }
            if inside_raw_string.get(idx).copied().unwrap_or(false) {
                // Every byte on this line is inside a multi-line raw
                // string literal — skip entirely so prose containing
                // `.unwrap()` in a SQL comment or doc block doesn't
                // fire.
                continue;
            }

            let trimmed = line.trim_start();
            if trimmed.starts_with("//") || trimmed.starts_with("*") {
                continue;
            }

            for col in unwrap_positions(line) {
                let src_id = path
                    .strip_prefix(repo_root)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .to_string();
                rows.push(UnwrapRow {
                    node_id: format!("{}:{}:{}", src_id, line_no, col),
                    source_id: src_id,
                    start_row: line_no as u32,
                    start_col: col as u32,
                    end_row: line_no as u32,
                    end_col: (col + ".unwrap()".len()) as u32,
                });
            }
        }
    }
    Ok(rows)
}

/// Return a bitmask over `text`'s lines: `mask[i] == true` iff line
/// `i` is entirely (or predominantly) inside a Rust raw string
/// literal (`r"..."`, `r#"..."#`, `r##"..."##`, ...) that opened on
/// an earlier line and hasn't yet closed. Coarse: any line whose
/// FIRST character is inside a raw string is masked, so the
/// `.unwrap()` in a SQL comment inside `r#" ... "#` won't fire.
///
/// Doesn't handle plain (non-raw) `"..."` multi-line strings — those
/// are rare in Rust source (require explicit `\` continuations) and
/// the per-line tokenizer already handles their common single-line
/// form.
fn raw_string_line_mask(text: &str) -> Vec<bool> {
    // Precompute the line index for each byte, then walk the bytes
    // tracking raw-string state. When a raw string closes, mark
    // every line STRICTLY between its opening line and its closing
    // line as interior — the opening + closing lines carry real
    // code and are handled by the per-line tokenizer.
    let n_lines = text.lines().count().max(1);
    let mut mask = vec![false; n_lines];
    let bytes = text.as_bytes();
    let mut cur_line = 0usize;
    let mut i = 0usize;
    let mut raw_hashes: usize = 0; // 0 = outside; else # closing hashes needed
    let mut raw_open_line: usize = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b == b'\n' {
            cur_line += 1;
            i += 1;
            continue;
        }
        if raw_hashes == 0 {
            if b == b'r' && (i == 0 || !is_ident_byte(bytes[i - 1])) {
                let mut hashes = 0usize;
                let mut j = i + 1;
                while j < bytes.len() && bytes[j] == b'#' {
                    hashes += 1;
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b'"' {
                    raw_hashes = hashes + 1;
                    raw_open_line = cur_line;
                    i = j + 1;
                    continue;
                }
            }
            i += 1;
            continue;
        }
        // Inside a raw string.
        if b == b'"' {
            let need = raw_hashes - 1;
            let mut all_hash = true;
            for k in 0..need {
                if bytes.get(i + 1 + k) != Some(&b'#') {
                    all_hash = false;
                    break;
                }
            }
            if all_hash {
                // Mark interior lines (strictly between open + close).
                let close_line = cur_line;
                let start_mark = raw_open_line + 1;
                for ln in start_mark..close_line {
                    if ln < mask.len() {
                        mask[ln] = true;
                    }
                }
                i += 1 + need;
                raw_hashes = 0;
                continue;
            }
        }
        i += 1;
    }
    mask
}

/// Return byte-column positions where `.unwrap()` appears as a bare
/// method call — panicking accessor, not one of the fallback variants.
/// Skips matches inside line comments, plain double-quoted string
/// literals, and Rust raw strings (`r"..."`, `r#"..."#`, `r##"..."##`).
fn unwrap_positions(line: &str) -> Vec<usize> {
    let bytes = line.as_bytes();
    let needle = b".unwrap()";
    let mut out = Vec::new();
    let mut i = 0;
    let mut in_string = false;
    // For raw strings: number of `#` before the opening `"`. We're
    // inside a raw string until we see `"` followed by the same
    // number of `#`s. `raw_hashes == 0` when we're either not in a
    // string, or in a plain double-quoted string (uses the
    // `in_string` toggle above).
    let mut raw_hashes: usize = 0;
    while i + needle.len() <= bytes.len() {
        let b = bytes[i];
        if !in_string && raw_hashes == 0 && b == b'/' && bytes.get(i + 1) == Some(&b'/') {
            break;
        }
        // Detect start of a raw string: `r"` or `r#..#"`. Requires
        // that `r` is at a token boundary (not part of an identifier
        // like `str_r"..."` — unrealistic but safe).
        if !in_string && raw_hashes == 0 && b == b'r' && (i == 0 || !is_ident_byte(bytes[i - 1])) {
            let mut hashes = 0usize;
            let mut j = i + 1;
            while j < bytes.len() && bytes[j] == b'#' {
                hashes += 1;
                j += 1;
            }
            if j < bytes.len() && bytes[j] == b'"' {
                // Entered a raw string; remember how many `#`s close it.
                raw_hashes = hashes + 1; // +1 sentinel: 0 means "not in raw"
                i = j + 1;
                continue;
            }
        }
        if raw_hashes > 0 {
            // Look for the closing `"` followed by (raw_hashes - 1) `#`s.
            if b == b'"' {
                let need = raw_hashes - 1;
                let mut all_hash = true;
                for k in 0..need {
                    if bytes.get(i + 1 + k) != Some(&b'#') {
                        all_hash = false;
                        break;
                    }
                }
                if all_hash {
                    i += 1 + need;
                    raw_hashes = 0;
                    continue;
                }
            }
            i += 1;
            continue;
        }
        if b == b'"' && (i == 0 || bytes[i - 1] != b'\\') {
            in_string = !in_string;
        }
        if !in_string && &bytes[i..i + needle.len()] == needle {
            out.push(i);
            i += needle.len();
            continue;
        }
        i += 1;
    }
    out
}

fn insert_unwrap_sites(conn: &Connection, rows: &[UnwrapRow]) -> Result<()> {
    let mut stmt = conn.prepare(
        "INSERT OR REPLACE INTO unwrap_sites (
            source_id, node_id, start_row, start_col, end_row, end_col
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    for r in rows {
        stmt.execute(params![
            r.source_id,
            r.node_id,
            r.start_row,
            r.start_col,
            r.end_row,
            r.end_col,
        ])?;
    }
    Ok(())
}

/// One `thread::sleep(..)` / `sleep(Duration::..)` call site in test code.
struct SleepRow {
    source_id: String,
    node_id: String,
    start_row: u32,
    start_col: u32,
    end_row: u32,
    end_col: u32,
}

/// Byte columns of every sleep call on `line`, or empty.
///
/// Matches `thread::sleep(` and `sleep(Duration`. Deliberately narrow: a bare
/// `sleep(` would catch unrelated helpers, and the two forms above are what
/// every occurrence in this workspace actually uses.
fn sleep_positions(line: &str) -> Vec<usize> {
    let mut out = Vec::new();
    for pat in ["thread::sleep(", "sleep(Duration"] {
        let mut from = 0usize;
        while let Some(rel) = line[from..].find(pat) {
            let col = from + rel;
            // `thread::sleep(` already covers the `sleep(Duration` inside it —
            // don't double-count the same call.
            if pat == "sleep(Duration" && col >= 8 && line[..col].ends_with("thread::") {
                from = col + pat.len();
                continue;
            }
            out.push(col);
            from = col + pat.len();
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// Walk the workspace and emit one row per sleep call in TEST code.
///
/// Scope is the INVERSE of `unsafe_sites`/`unwrap_sites`: those deliberately
/// skip `/tests/`, `/benches/` and `#[cfg(test)]` modules, because they audit
/// production code. This rule audits the tests themselves, so it scans exactly
/// what the others exclude — integration tests under `/tests/`, benches, and
/// the `#[cfg(test)] mod tests` tail of `/src/` files.
fn project_sleep_sites(workspace_root: &Path, repo_root: &Path) -> Result<Vec<SleepRow>> {
    let mut rows = Vec::new();
    for entry in WalkDir::new(workspace_root)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("rs") {
            continue;
        }
        let s = path.to_string_lossy();
        if s.contains("/target/") {
            continue;
        }
        let in_test_tree = s.contains("/tests/") || s.contains("/benches/");
        let in_src = s.contains("/src/");
        if !in_test_tree && !in_src {
            continue;
        }

        let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let lines: Vec<&str> = text.lines().collect();
        // In `/src/` files only the `#[cfg(test)]` tail counts; in a test tree
        // the whole file does.
        let test_start = if in_test_tree {
            0usize
        } else {
            match find_test_module_line(&lines) {
                Some(l) => l,
                None => continue,
            }
        };
        let inside_raw_string = raw_string_line_mask(&text);

        for (idx, line) in lines.iter().enumerate() {
            let line_no = idx + 1;
            if line_no < test_start {
                continue;
            }
            if inside_raw_string.get(idx).copied().unwrap_or(false) {
                continue;
            }
            let trimmed = line.trim_start();
            if trimmed.starts_with("//") || trimmed.starts_with("*") {
                continue;
            }
            for col in sleep_positions(line) {
                let src_id = path
                    .strip_prefix(repo_root)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .to_string();
                rows.push(SleepRow {
                    node_id: format!("{}:{}:{}", src_id, line_no, col),
                    source_id: src_id,
                    start_row: line_no as u32,
                    start_col: col as u32,
                    end_row: line_no as u32,
                    end_col: (col + 5) as u32,
                });
            }
        }
    }
    rows.sort_by(|a, b| {
        (&a.source_id, a.start_row, a.start_col).cmp(&(&b.source_id, b.start_row, b.start_col))
    });
    Ok(rows)
}

fn insert_sleep_sites(conn: &Connection, rows: &[SleepRow]) -> Result<()> {
    let mut stmt = conn.prepare(
        "INSERT OR REPLACE INTO sleep_sites (
            source_id, node_id, start_row, start_col, end_row, end_col
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    for r in rows {
        stmt.execute(params![
            r.source_id,
            r.node_id,
            r.start_row,
            r.start_col,
            r.end_row,
            r.end_col,
        ])?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Rule loading + execution
// ---------------------------------------------------------------------------

fn load_rules(rules_dir: &Path) -> Result<Vec<Rule>> {
    let mut rules = Vec::new();
    for entry in WalkDir::new(rules_dir)
        .max_depth(1)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) != Some("json") {
            continue;
        }
        // Skip the baseline if it happens to live in this dir.
        if path.file_name().and_then(|s| s.to_str()) == Some("baseline.json") {
            continue;
        }
        let text =
            fs::read_to_string(path).with_context(|| format!("read rule {}", path.display()))?;
        let rule: Rule = serde_json::from_str(&text)
            .with_context(|| format!("parse rule {}", path.display()))?;
        rules.push(rule);
    }
    rules.sort_by(|a, b| a.ID.cmp(&b.ID));
    Ok(rules)
}

#[derive(Debug, Clone)]
struct Finding {
    rule_id: String,
    source_id: String,
    node_id: String,
    start_row: u32,
    start_col: u32,
}

fn run_rule(conn: &Connection, rule: &Rule) -> Result<Vec<Finding>> {
    // Check `Requires` — every named table must exist. Rules whose
    // tables don't exist are silently skipped (mirrors mache's
    // `--rule '*'` behavior).
    for req in &rule.Requires {
        let exists: Option<i64> = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
                params![req],
                |row| row.get(0),
            )
            .unwrap_or(None);
        if exists.is_none() {
            return Ok(Vec::new());
        }
    }
    let sql = rule.Query.replace("%s", "");
    let mut stmt = conn
        .prepare(&sql)
        .with_context(|| format!("prepare rule {} SQL: {}", rule.ID, sql))?;
    let mut rows = stmt.query([])?;
    let mut findings = Vec::new();
    while let Some(row) = rows.next()? {
        // Columns are positional per the rule contract: source_id, node_id,
        // start_row, start_col, end_row, end_col, metric.
        let source_id: String = row.get(0)?;
        let node_id: String = row.get(1)?;
        let start_row: i64 = row.get(2).unwrap_or(0);
        let start_col: i64 = row.get(3).unwrap_or(0);
        findings.push(Finding {
            rule_id: rule.ID.clone(),
            source_id,
            node_id,
            start_row: start_row as u32,
            start_col: start_col as u32,
        });
    }
    Ok(findings)
}

// ---------------------------------------------------------------------------
// Baseline diff + gate
// ---------------------------------------------------------------------------

fn findings_to_counts(findings: &[Finding]) -> BTreeMap<(String, String), u64> {
    let mut counts = BTreeMap::new();
    for f in findings {
        *counts
            .entry((f.rule_id.clone(), f.source_id.clone()))
            .or_insert(0) += 1;
    }
    counts
}

fn diff_against_baseline(
    current: &BTreeMap<(String, String), u64>,
    baseline: &BTreeMap<(String, String), u64>,
) -> Vec<String> {
    let mut msgs = Vec::new();
    for (key, cur) in current {
        let base = baseline.get(key).copied().unwrap_or(0);
        if *cur > base {
            msgs.push(format!(
                "NEW: rule={} source={} count={} (baseline={})",
                key.0, key.1, cur, base
            ));
        }
    }
    msgs
}

fn load_baseline(path: &Path) -> Result<Baseline> {
    if !path.exists() {
        return Ok(Baseline::empty());
    }
    let text =
        fs::read_to_string(path).with_context(|| format!("read baseline {}", path.display()))?;
    let baseline: Baseline = serde_json::from_str(&text)
        .with_context(|| format!("parse baseline {}", path.display()))?;
    Ok(baseline)
}

fn write_baseline(path: &Path, baseline: &Baseline) -> Result<()> {
    let text = serde_json::to_string_pretty(baseline)?;
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, text + "\n")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    let cli = Cli::parse();

    let repo_root = fs::canonicalize(&cli.repo_root)
        .with_context(|| format!("canonicalize repo-root {}", cli.repo_root.display()))?;
    let workspace_root = repo_root.join(&cli.workspace);
    let rules_dir = repo_root.join(&cli.rules_dir);
    let baseline_path = repo_root.join(&cli.baseline);

    let workspace = parse_workspace(&workspace_root)?;
    let manifests = enumerate_member_manifests(&workspace)?;

    let conn = match &cli.out_db {
        Some(p) => {
            if p.exists() {
                fs::remove_file(p)?;
            }
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent)?;
            }
            Connection::open(p)?
        }
        None => Connection::open_in_memory()?,
    };
    conn.execute_batch(schema())?;
    insert_workspace_deps(&conn, &workspace)?;
    for manifest in &manifests {
        let rows = project_crate(manifest, &repo_root)?;
        insert_crate_deps(&conn, &rows)?;
    }

    // Bead `ley-line-open-85fb1f`: project every `unsafe` site in
    // `rs/*/src/**/*.rs` into `unsafe_sites` so the JSON rule can
    // gate on `WHERE has_safety = 0`.
    let unsafe_rows = project_unsafe_sites(&workspace_root, &repo_root)?;
    insert_unsafe_sites(&conn, &unsafe_rows)?;

    // Unwrap density audit (85fb1f follow-up): project every bare
    // `.unwrap()` call in production code into `unwrap_sites` so the
    // JSON rule can gate on the baseline count.
    let unwrap_rows = project_unwrap_sites(&workspace_root, &repo_root)?;
    insert_unwrap_sites(&conn, &unwrap_rows)?;

    // Sleeps in TEST code (bead `ley-line-open-c6101e`) — a wall-clock stop
    // condition is a race against the scheduler, and one already flaked CI on
    // an unrelated PR. Inverse scope to the two above: this scans exactly the
    // test trees they skip.
    let sleep_rows = project_sleep_sites(&workspace_root, &repo_root)?;
    insert_sleep_sites(&conn, &sleep_rows)?;

    let rules = load_rules(&rules_dir)?;
    if rules.is_empty() {
        eprintln!(
            "warning: no rules found in {} — projection ran but no gate applied",
            rules_dir.display()
        );
    }

    let mut all_findings = Vec::new();
    for rule in &rules {
        let mut findings = run_rule(&conn, rule)?;
        all_findings.append(&mut findings);
    }

    let current_counts = findings_to_counts(&all_findings);

    if cli.write_baseline {
        let baseline = Baseline::from_findings(&current_counts);
        write_baseline(&baseline_path, &baseline)?;
        eprintln!(
            "wrote baseline: {} ({} entries)",
            baseline_path.display(),
            baseline.counts.len()
        );
        return Ok(());
    }

    if cli.dogfood {
        if all_findings.is_empty() {
            println!("no findings");
        } else {
            for f in &all_findings {
                println!(
                    "[{}] {} :{}:{} node={}",
                    f.rule_id, f.source_id, f.start_row, f.start_col, f.node_id
                );
            }
        }
        return Ok(());
    }

    // Gate mode.
    let baseline = load_baseline(&baseline_path)?;
    let baseline_map = baseline.to_map();
    let new_findings = diff_against_baseline(&current_counts, &baseline_map);

    if !new_findings.is_empty() {
        eprintln!(
            "smell gate FAILED — {} new finding(s) vs baseline {}:",
            new_findings.len(),
            baseline_path.display()
        );
        for msg in &new_findings {
            eprintln!("  {}", msg);
        }
        // Emit per-finding detail for offending (rule, source) pairs so
        // the reviewer sees which crate + section + dep is drifting.
        for f in &all_findings {
            let cur = current_counts
                .get(&(f.rule_id.clone(), f.source_id.clone()))
                .copied()
                .unwrap_or(0);
            let base = baseline_map
                .get(&(f.rule_id.clone(), f.source_id.clone()))
                .copied()
                .unwrap_or(0);
            if cur > base {
                eprintln!(
                    "    {} {}:{}:{} [{}]",
                    f.source_id, f.start_row, f.start_col, f.node_id, f.rule_id
                );
            }
        }
        eprintln!(
            "To grandfather (deliberate change): run `task smells:baseline` and commit {}.",
            baseline_path.display()
        );
        std::process::exit(1);
    }

    eprintln!(
        "smells: {} finding(s), all covered by baseline ({})",
        all_findings.len(),
        baseline_path.display()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Load-bearing tests for the `unsafe_without_safety` projection
    //! (bead `ley-line-open-85fb1f`). Every real-world false-positive
    //! shape observed while bootstrapping the gate is pinned here so
    //! future edits to `contains_unsafe_keyword` can't silently
    //! re-introduce them.
    use super::*;

    // ── contains_unsafe_keyword tokenizer ─────────────────────────────

    #[test]
    fn detects_bare_unsafe_block() {
        assert!(contains_unsafe_keyword("    unsafe { }"));
        assert!(contains_unsafe_keyword("unsafe fn foo() {}"));
        assert!(contains_unsafe_keyword("unsafe impl Send for Foo {}"));
        assert!(contains_unsafe_keyword("pub unsafe extern \"C\" fn f() {}"));
    }

    #[test]
    fn rejects_unsafe_as_identifier_prefix_or_suffix() {
        // `_unsafe`, `unsafe_foo` — not the keyword.
        assert!(!contains_unsafe_keyword("fn contains_unsafe_keyword() {}"));
        assert!(!contains_unsafe_keyword("let _unsafe = 1;"));
        assert!(!contains_unsafe_keyword("foo.unsafely_do(x);"));
    }

    #[test]
    fn rejects_unsafe_inside_double_quoted_string() {
        // False positive #1 from initial rollout — `end_col: (col + "unsafe".len())`.
        assert!(!contains_unsafe_keyword(
            "                end_col: (col + \"unsafe\".len()) as u32,"
        ));
    }

    #[test]
    fn rejects_unsafe_inside_byte_string_literal() {
        // False positive #2 — `let needle = b"unsafe";`.
        assert!(!contains_unsafe_keyword("    let needle = b\"unsafe\";"));
    }

    #[test]
    fn rejects_unsafe_inside_backticks_on_prose_line() {
        // False positive #3 — SQL-comment prose inside a raw string:
        // `-- Per-file \`unsafe\` sites ...`
        assert!(!contains_unsafe_keyword(
            "        -- Per-file `unsafe` sites in `rs/*/src/**/*.rs`."
        ));
    }

    #[test]
    fn rejects_unsafe_after_line_comment_marker() {
        // Prose after `//` is not real code.
        assert!(!contains_unsafe_keyword(
            "    let x = 1; // TODO wrap in unsafe { }"
        ));
    }

    #[test]
    fn detects_unsafe_before_line_comment_marker() {
        // Real unsafe + trailing comment on same line — must still fire.
        assert!(contains_unsafe_keyword(
            "    unsafe { ptr::read(p) } // read raw"
        ));
    }

    // ── contains_safety_heading ───────────────────────────────────────

    #[test]
    fn detects_rustdoc_safety_headings() {
        assert!(contains_safety_heading("/// # Safety"));
        assert!(contains_safety_heading("///  # Safety"));
        assert!(contains_safety_heading("//! # Safety"));
        assert!(contains_safety_heading("/// ## Safety"));
        assert!(contains_safety_heading("/// ### Safety"));
    }

    #[test]
    fn rejects_prose_containing_the_word_safety() {
        assert!(!contains_safety_heading("/// Some safety concerns exist"));
        assert!(!contains_safety_heading("/// SAFETY: real invariant"));
    }

    // ── is_type_level_unsafe_fn ───────────────────────────────────────

    #[test]
    fn detects_type_level_unsafe_extern_fn() {
        // sqlite3_auto_extension transmute pattern (vec_index.rs).
        assert!(is_type_level_unsafe_fn(
            "            unsafe extern \"C\" fn("
        ));
        assert!(is_type_level_unsafe_fn(
            "type Cb = unsafe extern \"C\" fn(u32) -> i32;"
        ));
    }

    #[test]
    fn detects_type_level_unsafe_fn_without_extern() {
        assert!(is_type_level_unsafe_fn("        unsafe fn(u32) -> i32"));
    }

    #[test]
    fn rejects_real_unsafe_fn_declarations() {
        // Real fn decls have a name between `fn` and `(`.
        assert!(!is_type_level_unsafe_fn(
            "pub unsafe extern \"C\" fn leyline_open(path: *const c_char) {}"
        ));
        assert!(!is_type_level_unsafe_fn("unsafe fn write_out() {}"));
    }

    #[test]
    fn rejects_non_fn_unsafe_uses() {
        assert!(!is_type_level_unsafe_fn("unsafe { libc::getuid() }"));
        assert!(!is_type_level_unsafe_fn("unsafe impl Send for X {}"));
    }

    // ── doc_block_ending_at ───────────────────────────────────────────

    #[test]
    fn doc_block_walks_consecutive_slash_slash_slash_lines() {
        let lines: Vec<&str> = vec![
            "/// Some func.",      // 0
            "///",                 // 1
            "/// # Safety",        // 2
            "/// All input ptrs.", // 3
            "pub unsafe fn foo()", // 4  <- idx
        ];
        let block = doc_block_ending_at(&lines, 4);
        assert_eq!(block.len(), 4);
        assert!(block.iter().any(|l| contains_safety_heading(l)));
    }

    #[test]
    fn doc_block_walks_through_attribute_lines_between_doc_and_item() {
        // Rustdoc convention: `#[unsafe(no_mangle)]` sits between the
        // doc-block and the `pub unsafe fn` line; the doc-block still
        // binds to the fn.
        let lines: Vec<&str> = vec![
            "/// # Safety",                   // 0
            "/// Caller must ensure X.",      // 1
            "#[unsafe(no_mangle)]",           // 2
            "pub unsafe extern \"C\" fn f()", // 3  <- idx
        ];
        let block = doc_block_ending_at(&lines, 3);
        assert!(block.iter().any(|l| contains_safety_heading(l)));
    }

    #[test]
    fn doc_block_empty_when_target_has_no_docs() {
        let lines: Vec<&str> = vec!["let x = 1;", "let y = 2;", "unsafe { }"];
        assert_eq!(doc_block_ending_at(&lines, 2).len(), 0);
    }

    #[test]
    fn doc_block_at_start_of_file_returns_empty() {
        let lines: Vec<&str> = vec!["pub unsafe fn f()"];
        assert_eq!(doc_block_ending_at(&lines, 0).len(), 0);
    }

    // ── find_test_module_line ─────────────────────────────────────────

    #[test]
    fn test_module_line_finds_unindented_mod_tests() {
        let lines: Vec<&str> = vec![
            "pub fn a() {}", // 0
            "#[cfg(test)]",  // 1
            "mod tests {",   // 2
            "    fn t() {}", // 3
            "}",             // 4
        ];
        assert_eq!(find_test_module_line(&lines), Some(2));
    }

    #[test]
    fn test_module_line_ignores_per_fn_cfg_test() {
        // Regression pin: control.rs:217 has `    #[cfg(test)]` on a
        // per-fn helper (`set_current_root_unfenced_test_only`). Line
        // 423 has the real module cutoff. Only 423 should fire.
        let lines: Vec<&str> = vec![
            "impl X {",                                                // 0
            "    #[cfg(test)]",                                        // 1  <- per-fn, ignore
            "    pub fn set_root_unfenced(&mut self, r: [u8; 32]) {}", // 2
            "    #[cfg(feature = \"interrupt\")]",                     // 3
            "    fn interrupt_flags(&self) -> u64 { unsafe { 0 } }",   // 4
            "}",                                                       // 5
            "#[cfg(test)]",                                            // 6  <- real cutoff
            "mod tests {",                                             // 7
            "    fn t() {}",                                           // 8
            "}",                                                       // 9
        ];
        assert_eq!(find_test_module_line(&lines), Some(7));
    }

    #[test]
    fn test_module_line_returns_none_if_no_module_cfg_test() {
        let lines: Vec<&str> = vec![
            "impl X {",
            "    #[cfg(test)]",
            "    pub fn helper() {}",
            "}",
        ];
        assert_eq!(find_test_module_line(&lines), None);
    }

    // ── unwrap_positions ──────────────────────────────────────────────

    #[test]
    fn detects_bare_unwrap_call() {
        assert_eq!(unwrap_positions("    x.unwrap()"), vec![5]);
        assert_eq!(
            unwrap_positions("let n: u32 = s.parse().unwrap();"),
            vec![22]
        );
    }

    #[test]
    fn detects_multiple_unwraps_on_one_line() {
        // Columns are 0-indexed byte offsets of the `.` prefix.
        assert_eq!(unwrap_positions("(a.unwrap(), b.unwrap())"), vec![2, 14]);
    }

    #[test]
    fn rejects_fallback_variants() {
        // The non-panicking `unwrap_*` family — never a bug, don't
        // flag.
        assert!(unwrap_positions("x.unwrap_or(0)").is_empty());
        assert!(unwrap_positions("x.unwrap_or_default()").is_empty());
        assert!(unwrap_positions("x.unwrap_or_else(|| 0)").is_empty());
        assert!(unwrap_positions("x.unwrap_err()").is_empty());
    }

    #[test]
    fn rejects_expect_with_message() {
        // `.expect("...")` IS the annotation convention; not a match.
        assert!(unwrap_positions("x.expect(\"non-empty invariant\")").is_empty());
    }

    #[test]
    fn rejects_unwrap_inside_string_literal() {
        assert!(unwrap_positions("let s = \".unwrap()\";").is_empty());
    }

    #[test]
    fn rejects_unwrap_in_line_comment() {
        // Anywhere after `//` on the same line is prose, not code.
        assert!(unwrap_positions("let x = 1; // .unwrap() elsewhere").is_empty());
    }

    #[test]
    fn detects_unwrap_before_trailing_line_comment() {
        assert_eq!(
            unwrap_positions("let x = y.unwrap(); // TODO expect(...)"),
            vec![9]
        );
    }

    // ── is_cfg_test_attr ──────────────────────────────────────────────

    // ── raw_string_line_mask ──────────────────────────────────────────

    #[test]
    fn raw_string_mask_marks_interior_lines() {
        // Raw string opens on line 0, closes on line 2 — line 1 is
        // fully interior and must be masked.
        let text = "let sql = r#\"\n    -- x.unwrap() prose\n\"#;\n";
        let mask = raw_string_line_mask(text);
        // Line 0: opens after `r#\"` — code before it, don't mask
        // Line 1: pure interior — mask
        // Line 2: closes at start — don't mask (byte 0 is `\"`, closer)
        assert!(mask.len() >= 3);
        assert!(!mask[0], "opening line has code before r#\", not masked");
        assert!(mask[1], "interior line must be masked");
    }

    #[test]
    fn raw_string_mask_does_not_mask_when_no_raw_string() {
        let text = "let x = 1;\nlet y = 2;\n";
        let mask = raw_string_line_mask(text);
        for (i, m) in mask.iter().enumerate() {
            assert!(
                !m,
                "line {i} must not be masked in code without raw strings"
            );
        }
    }

    #[test]
    fn cfg_test_attr_matches_bare_cfg_test() {
        assert!(is_cfg_test_attr("#[cfg(test)]"));
        assert!(is_cfg_test_attr("    #[cfg(test)]"));
    }

    #[test]
    fn cfg_test_attr_matches_all_and_any_wrappers() {
        // fs/src/validate.rs shape: `#[cfg(all(test, feature = "validate"))]`
        assert!(is_cfg_test_attr(
            "#[cfg(all(test, feature = \"validate\"))]"
        ));
        assert!(is_cfg_test_attr(
            "#[cfg(any(test, feature = \"validate\"))]"
        ));
        assert!(is_cfg_test_attr("#[cfg(all(unix, test))]"));
    }

    #[test]
    fn cfg_test_attr_rejects_test_as_ident_prefix_or_suffix() {
        assert!(!is_cfg_test_attr("#[cfg(feature = \"test_foo\")]"));
        assert!(!is_cfg_test_attr("#[cfg(feature = \"foo_test\")]"));
        assert!(!is_cfg_test_attr("#[cfg(not(target_os = \"testable\"))]"));
    }

    #[test]
    fn cfg_test_attr_rejects_non_cfg_attributes() {
        assert!(!is_cfg_test_attr("#[test]"));
        assert!(!is_cfg_test_attr("#[allow(dead_code)]"));
        assert!(!is_cfg_test_attr("let x = 1;"));
    }

    #[test]
    fn test_module_line_matches_cfg_all_test_wrapping() {
        // fs/src/validate.rs's real shape: `#[cfg(all(test, feature="validate"))] mod tests`.
        let lines: Vec<&str> = vec![
            "fn prod() {}",                              // 0
            "#[cfg(all(test, feature = \"validate\"))]", // 1
            "mod tests {",                               // 2
            "    fn t() {}",                             // 3
            "}",                                         // 4
        ];
        assert_eq!(find_test_module_line(&lines), Some(2));
    }

    #[test]
    fn test_module_line_walks_through_blank_and_attrs_before_mod() {
        let lines: Vec<&str> = vec![
            "#[cfg(test)]",          // 0
            "",                      // 1 blank
            "#[cfg(feature=\"x\")]", // 2 another attr
            "// a comment",          // 3
            "pub mod tests {",       // 4
            "}",                     // 5
        ];
        assert_eq!(find_test_module_line(&lines), Some(1));
    }
}
