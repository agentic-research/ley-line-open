//! cargo-toml-projector — projects `rs/**/Cargo.toml` into a SQLite schema
//! (`workspace_deps` + `crate_deps`) so mache-shaped smell rules can query
//! it. Implements the workspace-deps drift gate (bead ley-line-open-3b2f55
//! Phase 3): fail on any crate that pins a literal version for a dep that
//! is already declared in `[workspace.dependencies]` (should be
//! `{ workspace = true }` instead).
//!
//! Deliberately mirrors mache's rule + baseline format so the projector
//! is drop-in replaceable by mache proper if we ever want to swap the
//! engine — same JSON rule shape (`ID`, `Description`, `Requires`,
//! `ScopeColumn`, `Query` with `%s` scope placeholder), same
//! `docs/smell-baseline.json` shape (`version: 1`, `counts: [{rule_id,
//! source_id, count}]`).

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
    about = "Project rs/**/Cargo.toml into SQLite + run mache-shaped smell rules"
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

        let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let lines: Vec<&str> = text.lines().collect();

        // Per-file cutoff: first line that begins with `#[cfg(test)]`.
        // Anything at or below this line is considered test code.
        let cfg_test_cutoff = lines
            .iter()
            .enumerate()
            .find(|(_, l)| l.trim_start().starts_with("#[cfg(test)]"))
            .map(|(i, _)| i + 1)
            .unwrap_or(usize::MAX);

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

            // Preceding-5-lines SAFETY check.
            let start_ctx = idx.saturating_sub(5);
            let has_safety = lines[start_ctx..idx]
                .iter()
                .any(|l| l.contains("SAFETY:") || contains_safety_heading(l));

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
}
