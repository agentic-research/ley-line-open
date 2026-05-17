//! Topology pre-pass module — Phases A-C of the LLO ingest topology blueprint.
//!
//! Given a flat `&[PathBuf]` list of source files and the repo root `source`,
//! this module derives **structural** signal that downstream parse/ingest
//! stages can exploit:
//!
//! - **P1 walk + stat** — per-file `FileMeta` (size, mtime, extension, depth)
//!   captured from `Path::metadata()`. The walk itself is performed *by the
//!   caller* (typically `cmd_parse::collect_files`); this module just consumes
//!   the resulting `&[PathBuf]`.
//! - **P2 manifest scan** — locate package manifests (`Cargo.toml`, `go.mod`,
//!   `package.json`, `pyproject.toml`, ...) and partition files into regions
//!   by nearest-ancestor manifest. Files without any ancestor manifest fall
//!   into a synthetic "root" region.
//! - **P3 regex import sweep** — language-keyed line scanning of each
//!   parseable file's first 4 KiB, recovering coarse-grained import edges
//!   without invoking tree-sitter. Specifiers are resolved against the known
//!   file list by suffix-match.
//!
//! Integration into `cmd_parse::parse_into_conn` is **NOT** done here — see
//! the sibling bead. This module is a standalone analyzer with no side
//! effects beyond reading the supplied files.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};

pub use leyline_sheaf::topology::RegionId;

// ---------------------------------------------------------------------------
// Public surface — exactly as specified by bead `ley-line-open-9d3208`.
// ---------------------------------------------------------------------------

/// Result of the topology pre-pass.
///
/// `parse_order` is an index-vec into the caller's `files` slice — the
/// recommended order to parse files in for cache-coherent ingest. Files
/// inside the same region are clustered together; regions themselves are
/// emitted in ascending `RegionId` order.
#[derive(Debug, Clone)]
pub struct TopologyOutput {
    pub parse_order: Vec<usize>,
    pub file_regions: Vec<FileRegion>,
    pub edge_estimates: Vec<EdgeEstimate>,
    pub region_edges: Vec<(RegionId, RegionId, f32)>,
    pub stats: TopologyStats,
}

/// Region assignment for a single file.
///
/// `file_index` is the index into the caller's `files` slice; `region` is
/// the `RegionId` of the nearest-ancestor manifest (or the synthetic root
/// region id `0` if no manifest is reachable).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileRegion {
    pub file_index: usize,
    pub region: RegionId,
    /// Directory depth from `source` (number of path components).
    pub depth: u32,
    /// File size in bytes (0 if metadata could not be read).
    pub size: u64,
}

/// A coarse-grained import edge recovered by the regex sweep.
///
/// `from` and `to` are indices into the caller's `files` slice.
/// `confidence` is a heuristic in `(0.0, 1.0]`: exact suffix matches with
/// long specifiers score higher than short ambiguous ones.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EdgeEstimate {
    pub from: usize,
    pub to: usize,
    pub confidence: f32,
    pub language: Lang,
}

/// Languages we can sweep imports for. `Other` is parseable but produces
/// no import edges from this pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Lang {
    Go,
    Rust,
    Python,
    Ts,
    CCpp,
    Other,
}

/// Summary statistics for the pre-pass run.
///
/// Serializable: required for Gate 4 (determinism). The order of fields
/// here defines the serialization order.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct TopologyStats {
    pub n_files: u64,
    pub n_regions: u64,
    pub n_manifests: u64,
    pub n_edges: u64,
    pub n_files_scanned: u64,
    /// Wall-clock duration of the run in microseconds. Non-deterministic
    /// across runs, so excluded from determinism comparisons (see
    /// `stats_for_determinism`).
    pub elapsed_us: u64,
}

impl TopologyStats {
    /// Determinism view — drops the timing field that legitimately
    /// varies between runs. Used by Gate 4.
    pub fn for_determinism(&self) -> StatsDeterministic {
        StatsDeterministic {
            n_files: self.n_files,
            n_regions: self.n_regions,
            n_manifests: self.n_manifests,
            n_edges: self.n_edges,
            n_files_scanned: self.n_files_scanned,
        }
    }
}

/// Timing-free projection of [`TopologyStats`] for byte-identical
/// determinism comparisons.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct StatsDeterministic {
    pub n_files: u64,
    pub n_regions: u64,
    pub n_manifests: u64,
    pub n_edges: u64,
    pub n_files_scanned: u64,
}

// ---------------------------------------------------------------------------
// Entry point.
// ---------------------------------------------------------------------------

/// Run Phases A-C on a pre-collected file list.
///
/// `source` is the repo root. `files` is the caller's flat walk result.
/// The returned `TopologyOutput` is purely descriptive — this function
/// performs no writes, no daemon ops, no SQL.
pub fn run(files: &[PathBuf], source: &Path) -> Result<TopologyOutput> {
    let started = std::time::Instant::now();

    // ---- P1: walk + stat -------------------------------------------------
    // Caller already produced `files`. We just stat each one and record
    // the meta we need for region assignment + parse-order clustering.
    let metas = stat_files(files, source);

    // ---- P2: manifest scan ----------------------------------------------
    let manifests = find_manifests(files);
    let n_manifests = manifests.len() as u64;
    let (file_regions, n_regions) = assign_regions(files, &metas, &manifests, source);

    // ---- P3: regex import sweep -----------------------------------------
    // `n_files_scanned` is the number of files actually OPENED for import
    // detection (i.e. files with a known language extension and a
    // non-empty head), NOT the number that produced resolved edges. A
    // file with `import "fmt"` where `fmt` doesn't resolve is still
    // scanned — that's the semantic we want for the cost ceiling.
    let (edge_estimates, n_files_scanned) = sweep_imports(files);

    // Aggregate file-edges into region-edges (mean confidence in [0, 1]
    // per region pair — see `aggregate_region_edges`). Stored sorted by
    // (a, b) for determinism (Gate 4).
    let region_edges = aggregate_region_edges(&edge_estimates, &file_regions);

    // Parse order: stable sort of `0..files.len()` by (region, depth,
    // path-suffix) — clusters same-region files together so the downstream
    // parser benefits from incremental locality.
    let parse_order = compute_parse_order(&file_regions, files);

    let elapsed_us = started.elapsed().as_micros() as u64;
    let stats = TopologyStats {
        n_files: files.len() as u64,
        n_regions,
        n_manifests,
        n_edges: edge_estimates.len() as u64,
        n_files_scanned,
        elapsed_us,
    };

    Ok(TopologyOutput {
        parse_order,
        file_regions,
        edge_estimates,
        region_edges,
        stats,
    })
}

// ---------------------------------------------------------------------------
// P1 — stat.
// ---------------------------------------------------------------------------

/// Captured by P1 (walk + stat); consumed by P2 (region assignment).
///
/// `extension` and `rel` aren't used after region assignment today but
/// are kept on the struct because they're zero-cost during the stat and
/// the sibling integration bead will need them. Marked `dead_code` to
/// silence the compiler until they're consumed.
#[derive(Debug, Clone, Default)]
#[allow(dead_code)]
struct FileMeta {
    size: u64,
    mtime: Option<SystemTime>,
    extension: Option<String>,
    /// Number of path components in the relative path. Top-level files
    /// have depth 1.
    depth: u32,
    /// Path relative to `source`. `None` if the path does not lie
    /// under `source`.
    rel: Option<PathBuf>,
}

fn stat_files(files: &[PathBuf], source: &Path) -> Vec<FileMeta> {
    files
        .iter()
        .map(|p| {
            let md = fs::metadata(p).ok();
            let size = md.as_ref().map(|m| m.len()).unwrap_or(0);
            let mtime = md.as_ref().and_then(|m| m.modified().ok());
            let extension = p
                .extension()
                .and_then(|e| e.to_str())
                .map(|s| s.to_ascii_lowercase());
            let rel = p.strip_prefix(source).ok().map(PathBuf::from);
            let depth = rel
                .as_ref()
                .map(|r| r.components().count() as u32)
                .unwrap_or(0);
            FileMeta {
                size,
                mtime,
                extension,
                depth,
                rel,
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// P2 — manifest scan + region assignment.
// ---------------------------------------------------------------------------

/// Names that anchor a region. Order is irrelevant — we just check
/// whether any of these sits at a directory boundary.
const MANIFEST_NAMES: &[&str] = &[
    "Cargo.toml",
    "go.mod",
    "package.json",
    "pyproject.toml",
    "setup.cfg",
    "BUILD",
    "BUILD.bazel",
    "WORKSPACE",
    "requirements.txt",
    "Pipfile",
];

fn is_manifest(name: &str) -> bool {
    MANIFEST_NAMES.contains(&name)
}

/// One entry per discovered manifest. `dir` is the *parent directory* of
/// the manifest file (i.e. the region's root). Sorted by `dir` lex order
/// at construction time so id assignment is deterministic.
#[derive(Debug, Clone)]
struct Manifest {
    dir: PathBuf,
    #[allow(dead_code)]
    kind: String,
}

fn find_manifests(files: &[PathBuf]) -> Vec<Manifest> {
    let mut out: Vec<Manifest> = files
        .iter()
        .filter_map(|p| {
            let name = p.file_name().and_then(|n| n.to_str())?;
            if !is_manifest(name) {
                return None;
            }
            let dir = p.parent()?.to_path_buf();
            Some(Manifest {
                dir,
                kind: name.to_string(),
            })
        })
        .collect();
    // Deterministic id assignment: sort by directory path, longer paths
    // (deeper manifests) take **larger** ids — but the region a file
    // belongs to is always the **deepest** ancestor manifest, so the id
    // ordering itself doesn't affect membership. Stable sort by path.
    out.sort_by(|a, b| a.dir.cmp(&b.dir));
    out.dedup_by(|a, b| a.dir == b.dir);
    out
}

/// Assign every file to the deepest ancestor manifest. Files with no
/// ancestor manifest go to region `0` (synthetic root).
///
/// Returns `(file_regions, n_regions)` where `n_regions` includes the
/// root region iff at least one file landed there.
///
/// Algorithmic note (Copilot finding 10): the assignment walks up each
/// file's ancestor directories looking for a hit in a `BTreeMap<PathBuf,
/// RegionId>` keyed by manifest dir, rather than iterating every
/// manifest for every file. Complexity is `O(n_files × avg_depth)`
/// instead of `O(n_files × n_manifests)`. `BTreeMap` (not `HashMap`)
/// matches the determinism discipline of the rest of this module.
fn assign_regions(
    files: &[PathBuf],
    metas: &[FileMeta],
    manifests: &[Manifest],
    source: &Path,
) -> (Vec<FileRegion>, u64) {
    // Build a lookup table: manifest dir → region id.
    // Manifests are already sorted + deduped by `find_manifests`; the
    // region id is the 1-indexed position in that sorted vec, with
    // `0` reserved for the synthetic root.
    let manifest_region: BTreeMap<&Path, RegionId> = manifests
        .iter()
        .enumerate()
        .map(|(mi, m)| (m.dir.as_path(), (mi as u32) + 1))
        .collect();

    let mut used_regions: BTreeSet<RegionId> = BTreeSet::new();
    let mut out: Vec<FileRegion> = Vec::with_capacity(files.len());

    for (idx, path) in files.iter().enumerate() {
        let start_dir = path.parent().unwrap_or(source);
        // Walk ancestor directories from the file's parent up to the
        // filesystem root. The FIRST hit is the deepest ancestor
        // manifest, since we walk outward — no need to enumerate all
        // manifests and pick the deepest.
        let region: RegionId = start_dir
            .ancestors()
            .find_map(|ancestor| manifest_region.get(ancestor).copied())
            .unwrap_or(0);
        used_regions.insert(region);
        out.push(FileRegion {
            file_index: idx,
            region,
            depth: metas[idx].depth,
            size: metas[idx].size,
        });
    }

    (out, used_regions.len() as u64)
}

// ---------------------------------------------------------------------------
// P3 — regex import sweep.
// ---------------------------------------------------------------------------

/// Maximum bytes read from each file's head for import detection. Bead
/// specifies 4 KiB.
const IMPORT_SCAN_HEAD_BYTES: usize = 4 * 1024;

fn lang_of_extension(ext: Option<&str>) -> Lang {
    match ext {
        Some("go") => Lang::Go,
        Some("rs") => Lang::Rust,
        Some("py") => Lang::Python,
        Some("ts") | Some("tsx") | Some("js") | Some("jsx") | Some("mjs") | Some("cjs") => Lang::Ts,
        Some("c") | Some("cc") | Some("cpp") | Some("cxx") | Some("h") | Some("hpp")
        | Some("hh") => Lang::CCpp,
        _ => Lang::Other,
    }
}

/// Read the first `IMPORT_SCAN_HEAD_BYTES` of `path`. Returns an empty
/// string on any IO error — the sweep is *advisory*; failed reads
/// silently produce zero edges rather than aborting the whole pass.
fn read_head(path: &Path) -> String {
    let mut f = match fs::File::open(path) {
        Ok(f) => f,
        Err(_) => return String::new(),
    };
    let mut buf = vec![0u8; IMPORT_SCAN_HEAD_BYTES];
    let n = f.read(&mut buf).unwrap_or(0);
    buf.truncate(n);
    String::from_utf8_lossy(&buf).into_owned()
}

/// Extract import specifiers from a single file's head text. Specifiers
/// are *strings* — resolution to file indices happens in
/// [`sweep_imports`].
fn extract_specifiers(text: &str, lang: Lang) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_go_block = false;
    for raw_line in text.lines() {
        let line = raw_line.trim_start();
        match lang {
            Lang::Go => {
                if in_go_block {
                    if line.starts_with(')') {
                        in_go_block = false;
                        continue;
                    }
                    if let Some(s) = extract_quoted(line) {
                        out.push(s);
                    }
                    continue;
                }
                if line.starts_with("import (") || line == "import (" {
                    in_go_block = true;
                    continue;
                }
                if let Some(rest) = line.strip_prefix("import ")
                    && let Some(s) = extract_quoted(rest)
                {
                    out.push(s);
                }
            }
            Lang::Rust => {
                // Match `use ...;`, `pub use ...;`, and visibility-scoped
                // re-exports like `pub(crate) use ...;`, `pub(super) use ...;`.
                // We do NOT attempt to match attribute-prefixed forms
                // (`#[allow(...)] pub use ...;`) — those land in the line
                // *after* the attribute, and our line-by-line scan picks
                // them up naturally so long as the `use` line itself
                // starts with one of the prefixes below.
                let use_body = if let Some(rest) = line.strip_prefix("use ") {
                    Some(rest)
                } else if let Some(rest) = line.strip_prefix("pub use ") {
                    Some(rest)
                } else if let Some(after_pub) = line.strip_prefix("pub(") {
                    // pub(crate) use ... / pub(super) use ... / pub(in path) use ...
                    after_pub
                        .find(')')
                        .and_then(|close| after_pub.get(close + 1..))
                        .and_then(|tail| tail.trim_start().strip_prefix("use "))
                } else {
                    None
                };
                if let Some(rest) = use_body {
                    // `use foo::bar::baz;` → specifier "foo::bar::baz"
                    let end = rest.find(';').unwrap_or(rest.len());
                    let mut spec = rest[..end].trim().to_string();
                    // Strip leading `crate::` / `self::` / `super::`
                    for prefix in ["crate::", "self::", "super::"] {
                        if let Some(stripped) = spec.strip_prefix(prefix) {
                            spec = stripped.to_string();
                            break;
                        }
                    }
                    if !spec.is_empty() {
                        out.push(spec);
                    }
                }
            }
            Lang::Python => {
                // from X import Y  /  import X
                if let Some(rest) = line.strip_prefix("from ") {
                    if let Some(end) = rest.find(" import") {
                        let module = rest[..end].trim();
                        if !module.is_empty() {
                            out.push(module.to_string());
                        }
                    }
                } else if let Some(rest) = line.strip_prefix("import ") {
                    let module = rest.split(&[' ', ',', ';'][..]).next().unwrap_or("").trim();
                    if !module.is_empty() {
                        out.push(module.to_string());
                    }
                }
            }
            Lang::Ts => {
                // Skip comment lines outright (Copilot finding 11). The
                // earlier `line.contains("import(")` clause matched the
                // commented-out specifier inside `// import('./x')` and
                // `/* import('./x') */`. We also skip lines starting
                // with `*` as a best-effort guard for block-comment
                // continuations; tracking enter/exit of `/* ... */`
                // blocks across lines is out of scope for this regex
                // sweep.
                if line.starts_with("//") || line.starts_with("/*") || line.starts_with('*') {
                    continue;
                }
                // Static `import ...`, dynamic `import(...)`, top-level
                // `import('./x')`, and `export { x } from './x'` all
                // carry the module specifier in the first quoted region
                // of the line. The dynamic-import branch enforces a
                // word boundary in front of `import(` so identifiers
                // ending in `import(` (e.g. `myimport(`) don't false-
                // match — and so the commented forms ruled out above
                // would also be ruled out even if the skip leaked.
                let take_first_quoted = line.starts_with("import ")
                    || line.starts_with("import(")
                    // `export { x } from './x'` / `export * from './x'`
                    || (line.starts_with("export") && line.contains(" from "))
                    // `await import('./x')`, `const x = await import('./x')`
                    || has_dynamic_import(line);
                if take_first_quoted {
                    if let Some(s) = extract_quoted(line) {
                        out.push(s);
                    }
                } else if (line.starts_with("const ") || line.starts_with("let "))
                    && line.contains("require(")
                    && let Some(s) = extract_quoted(line)
                {
                    // const x = require("foo")
                    out.push(s);
                }
            }
            Lang::CCpp => {
                if let Some(rest) = line.strip_prefix("#include") {
                    let rest = rest.trim_start();
                    let (open, close) = match rest.chars().next() {
                        Some('<') => ('<', '>'),
                        Some('"') => ('"', '"'),
                        _ => continue,
                    };
                    let _ = open; // open is implicit at rest[0]
                    if let Some(end) = rest[1..].find(close) {
                        let s = &rest[1..1 + end];
                        if !s.is_empty() {
                            out.push(s.to_string());
                        }
                    }
                }
            }
            Lang::Other => {}
        }
    }
    out
}

/// Detect a dynamic `import(...)` call in `line`. Requires a token
/// boundary in front of `import` so identifiers ending in `import(`
/// (e.g. `someimport(`) don't false-match (Copilot finding 11). The
/// boundary check looks at the byte immediately preceding the match:
/// any non-alphanumeric, non-`_`, non-`$` character qualifies (covers
/// `await import(`, ` import(`, `=import(`, etc.), and a match at
/// position 0 is also valid (handled by the upstream `starts_with`
/// branch but treated as a boundary here too for completeness).
fn has_dynamic_import(line: &str) -> bool {
    let needle = "import(";
    let bytes = line.as_bytes();
    let mut start = 0;
    while let Some(pos) = line[start..].find(needle) {
        let abs = start + pos;
        let boundary_ok = if abs == 0 {
            true
        } else {
            let prev = bytes[abs - 1];
            !prev.is_ascii_alphanumeric() && prev != b'_' && prev != b'$'
        };
        if boundary_ok {
            return true;
        }
        start = abs + 1;
    }
    false
}

/// Pull out the first quoted string from `line`. Handles single, double,
/// and backtick quotes. Returns the inner contents (no quote chars).
fn extract_quoted(line: &str) -> Option<String> {
    let bytes = line.as_bytes();
    let mut start: Option<usize> = None;
    let mut quote_char = b'\0';
    for (i, &b) in bytes.iter().enumerate() {
        match start {
            None => {
                if b == b'"' || b == b'\'' || b == b'`' {
                    start = Some(i + 1);
                    quote_char = b;
                }
            }
            Some(s) => {
                if b == quote_char {
                    return Some(line[s..i].to_string());
                }
            }
        }
    }
    None
}

/// Build a resolution index: tail-component lookup for `files`. Maps
/// the basename (and the module-style stem for `.rs`/`.py`) to the
/// list of file indices that match. Suffix matches against a specifier
/// are then computed by walking the candidate list.
struct ResolverIndex {
    /// basename (e.g. "util.ts", "baz.rs", "baz.py") → file indices
    by_basename: HashMap<String, Vec<usize>>,
    /// module stem (e.g. "baz", "util") → file indices. Lets Python's
    /// `from foo.bar import baz` and Rust's `use crate::baz::qux` both
    /// resolve.
    by_stem: HashMap<String, Vec<usize>>,
}

impl ResolverIndex {
    fn build(files: &[PathBuf]) -> Self {
        let mut by_basename: HashMap<String, Vec<usize>> = HashMap::new();
        let mut by_stem: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, p) in files.iter().enumerate() {
            if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                by_basename.entry(name.to_string()).or_default().push(i);
            }
            if let Some(stem) = p.file_stem().and_then(|n| n.to_str()) {
                by_stem.entry(stem.to_string()).or_default().push(i);
            }
        }
        ResolverIndex {
            by_basename,
            by_stem,
        }
    }

    /// Try to resolve a specifier to one or more file indices.
    ///
    /// Strategy (in order; first hit wins):
    /// 1. **Language-preferred basename hit** — try `{last}.{lang_ext}`
    ///    first, then fall back to other extensions.
    /// 2. **Bare basename hit** — `{last}` as-is (handles TS imports
    ///    that already include `.ts` or files imported with no ext).
    /// 3. **Stem hit (language-preferred)** — fall back to module stem
    ///    when no basename matches.
    /// 4. **Inner-component hit** — for chained specifiers like
    ///    `baz::qux` or `foo.bar` where the *last* component is the
    ///    item, not the module, try the second-to-last component too.
    ///
    /// Returns `(targets, base_confidence)`. Base confidence is scaled
    /// by specifier length in [`sweep_imports`].
    fn resolve(&self, spec: &str, lang: Lang) -> (Vec<usize>, f32) {
        // Normalize: strip surrounding whitespace, leading `./` / `../`.
        let mut s = spec.trim();
        while let Some(stripped) = s.strip_prefix("./").or_else(|| s.strip_prefix("../")) {
            s = stripped;
        }

        // Split into components by ::, /, or .  — preserves
        // order so we can try last-first then walk backwards.
        let parts: Vec<&str> = s.split([':', '/', '.']).filter(|p| !p.is_empty()).collect();
        if parts.is_empty() {
            return (Vec::new(), 0.0);
        }

        // Language-preferred extension list. The preferred ext comes
        // first so cross-language false matches (e.g. `bar` resolving
        // to `bar.py` from a Go import) are avoided.
        let exts: &[&str] = match lang {
            Lang::Go => &["go"],
            Lang::Rust => &["rs"],
            Lang::Python => &["py"],
            Lang::Ts => &["ts", "tsx", "js", "jsx", "mjs", "cjs"],
            Lang::CCpp => &["h", "hpp", "hh", "c", "cc", "cpp", "cxx"],
            Lang::Other => &[],
        };

        // Try each component from last to first; for each, try
        // language-preferred extensions then the bare name.
        for (depth, &part) in parts.iter().enumerate().rev() {
            for ext in exts {
                let cand = format!("{part}.{ext}");
                if let Some(hits) = self.by_basename.get(&cand) {
                    // Slightly higher confidence for the rightmost
                    // (most-specific) component that hit.
                    let base = if depth + 1 == parts.len() { 0.85 } else { 0.7 };
                    return (hits.clone(), base);
                }
            }
            if let Some(hits) = self.by_basename.get(part) {
                let base = if depth + 1 == parts.len() { 0.8 } else { 0.65 };
                return (hits.clone(), base);
            }
        }
        // Final fallback: stem-only match on the last component.
        if let Some(hits) = self.by_stem.get(parts[parts.len() - 1]) {
            return (hits.clone(), 0.5);
        }
        (Vec::new(), 0.0)
    }
}

/// Sweep imports across `files`.
///
/// Returns `(edges, n_files_scanned)`. `n_files_scanned` counts every
/// file that was opened for import detection — i.e. files with a
/// recognized language extension and a non-empty head. Files that were
/// scanned but produced zero resolved edges still count, which is what
/// we want for the cost ceiling (Gate 1).
fn sweep_imports(files: &[PathBuf]) -> (Vec<EdgeEstimate>, u64) {
    let index = ResolverIndex::build(files);
    let mut edges: Vec<EdgeEstimate> = Vec::new();
    let mut n_files_scanned: u64 = 0;

    for (i, p) in files.iter().enumerate() {
        let ext = p
            .extension()
            .and_then(|e| e.to_str())
            .map(|s| s.to_ascii_lowercase());
        let lang = lang_of_extension(ext.as_deref());
        if matches!(lang, Lang::Other) {
            continue;
        }
        let head = read_head(p);
        if head.is_empty() {
            continue;
        }
        // A file with a recognized language and a non-empty head was
        // scanned, regardless of whether any specifier resolves.
        n_files_scanned += 1;
        let specs = extract_specifiers(&head, lang);
        for spec in specs {
            let (targets, base_conf) = index.resolve(&spec, lang);
            // Specifier-length term: longer specifiers are less likely
            // to be ambiguous. Clamped so a single-char target doesn't
            // pin confidence at 0.
            let len_term = (spec.len() as f32 / 40.0).clamp(0.05, 0.3);
            // 1/N penalty when multiple files match — splits credit so
            // ambiguous hits don't dominate region edges.
            let split = if targets.is_empty() {
                0.0
            } else {
                1.0 / (targets.len() as f32)
            };
            for t in targets {
                if t == i {
                    continue; // self-edges are noise
                }
                let conf = ((base_conf + len_term) * split).min(1.0);
                if conf > 0.0 {
                    edges.push(EdgeEstimate {
                        from: i,
                        to: t,
                        confidence: conf,
                        language: lang,
                    });
                }
            }
        }
    }

    // Sort for determinism: by (from, to, lang as discriminant).
    edges.sort_by(|a, b| {
        a.from
            .cmp(&b.from)
            .then(a.to.cmp(&b.to))
            .then((a.language as u32).cmp(&(b.language as u32)))
    });
    (edges, n_files_scanned)
}

// ---------------------------------------------------------------------------
// Region edge aggregation.
// ---------------------------------------------------------------------------

/// Aggregate per-file edges into per-region-pair edges.
///
/// Output `(RegionId, RegionId, f32)` triples carry the **mean confidence**
/// of the contributing file-level edges, NOT their sum. This is the
/// downstream `SheafRestrictionInput::co_change_rate` contract: that field
/// is a probability-weight in `[0.0, 1.0]`, and `op_sheaf_set_topology`
/// treats values outside that range as out-of-spec.
///
/// Concretely: if there are 10 file edges between region 1 and region 2
/// with confidences `[0.85, 0.9, 0.75, ...]`, the emitted region edge
/// carries `mean(confidences)`, NOT `sum(confidences)` (which would
/// commonly land at ~8.0 — well outside [0, 1]).
///
/// Each per-file edge contributes at most 1.0 to the running mean (the
/// sweep clamps per-edge confidence to `(0.0, 1.0]`), so the mean is
/// guaranteed to lie in `(0.0, 1.0]`. A debug-mode assertion pins this.
fn aggregate_region_edges(
    edges: &[EdgeEstimate],
    file_regions: &[FileRegion],
) -> Vec<(RegionId, RegionId, f32)> {
    // Accumulate (sum, count) per region pair so we can emit the mean.
    let mut agg: BTreeMap<(RegionId, RegionId), (f32, u32)> = BTreeMap::new();
    for e in edges {
        let ra = file_regions[e.from].region;
        let rb = file_regions[e.to].region;
        if ra == rb {
            continue; // intra-region edges are not useful for the sheaf
        }
        // Canonicalize ordering so (a, b) and (b, a) merge — region
        // edges are undirected for the sheaf's restriction view.
        let key = if ra <= rb { (ra, rb) } else { (rb, ra) };
        let slot = agg.entry(key).or_insert((0.0, 0));
        slot.0 += e.confidence;
        slot.1 += 1;
    }
    // BTreeMap iteration is already sorted by key — emit in that order
    // as the mean (sum / count). `count` is always > 0 inside the loop.
    agg.into_iter()
        .map(|((a, b), (sum, count))| {
            let mean = sum / (count as f32);
            // Defensive clamp: per-edge confidences are already in
            // (0, 1] (see `sweep_imports`), so the mean must be too.
            // The clamp catches future drift where the per-edge cap
            // is relaxed.
            debug_assert!(
                (0.0..=1.0).contains(&mean),
                "aggregate_region_edges: mean {mean} out of [0, 1] for ({a}, {b}) — \
                 sum={sum} count={count}"
            );
            (a, b, mean.clamp(0.0, 1.0))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Parse-order clustering.
// ---------------------------------------------------------------------------

fn compute_parse_order(file_regions: &[FileRegion], files: &[PathBuf]) -> Vec<usize> {
    let mut order: Vec<usize> = (0..files.len()).collect();
    order.sort_by(|&a, &b| {
        file_regions[a]
            .region
            .cmp(&file_regions[b].region)
            .then(file_regions[a].depth.cmp(&file_regions[b].depth))
            .then(files[a].cmp(&files[b]))
    });
    order
}

// ---------------------------------------------------------------------------
// Convenience: walk a source tree the same way `cmd_parse::collect_files`
// does, applying `cmd_parse::is_bloat_dir` as the skip predicate. Provided
// as a helper so callers (tests, benches, the eventual sibling-bead
// integration) don't have to duplicate the walk.
// ---------------------------------------------------------------------------

/// Walk `source` recursively, returning every file path while skipping
/// well-known bloat directories. Mirrors the predicate used by
/// `cmd_parse::collect_files` — see [`crate::cmd_parse::is_bloat_dir`].
pub fn collect_files(source: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    collect_files_inner(source, &mut out)?;
    out.sort();
    Ok(out)
}

fn collect_files_inner(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = fs::read_dir(dir).with_context(|| format!("read_dir {}", dir.display()))?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        if let Some(name) = path.file_name().and_then(|n| n.to_str())
            && crate::cmd_parse::is_bloat_dir(name)
        {
            continue;
        }
        if path.is_dir() {
            collect_files_inner(&path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Unit tests — narrow, on the pure helpers. End-to-end gates live in the
// integration test file.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_quoted_handles_double_quotes() {
        assert_eq!(extract_quoted(r#""foo/bar""#).as_deref(), Some("foo/bar"));
    }

    #[test]
    fn extract_quoted_handles_single_quotes() {
        assert_eq!(extract_quoted("'./util'").as_deref(), Some("./util"));
    }

    #[test]
    fn extract_quoted_no_match_returns_none() {
        assert!(extract_quoted("no quotes here").is_none());
    }

    #[test]
    fn extract_specifiers_go_single() {
        let specs = extract_specifiers("package m\nimport \"github.com/foo/bar\"\n", Lang::Go);
        assert_eq!(specs, vec!["github.com/foo/bar"]);
    }

    #[test]
    fn extract_specifiers_go_block() {
        let src = r#"package m
import (
    "github.com/foo/bar"
    "fmt"
)"#;
        let specs = extract_specifiers(src, Lang::Go);
        assert_eq!(specs, vec!["github.com/foo/bar", "fmt"]);
    }

    #[test]
    fn extract_specifiers_rust_use() {
        let specs = extract_specifiers("use crate::baz::qux;\n", Lang::Rust);
        assert_eq!(specs, vec!["baz::qux"]);
    }

    #[test]
    fn extract_specifiers_rust_pub_use() {
        let specs = extract_specifiers("pub use foo::bar;\n", Lang::Rust);
        assert_eq!(specs, vec!["foo::bar"]);
    }

    #[test]
    fn extract_specifiers_rust_pub_crate_use() {
        let specs = extract_specifiers("pub(crate) use foo::bar;\n", Lang::Rust);
        assert_eq!(specs, vec!["foo::bar"]);
    }

    #[test]
    fn extract_specifiers_rust_pub_super_use() {
        let specs = extract_specifiers("pub(super) use crate::baz::qux;\n", Lang::Rust);
        assert_eq!(specs, vec!["baz::qux"]);
    }

    #[test]
    fn extract_specifiers_python_from() {
        let specs = extract_specifiers("from foo.bar import baz\n", Lang::Python);
        assert_eq!(specs, vec!["foo.bar"]);
    }

    #[test]
    fn extract_specifiers_python_import() {
        let specs = extract_specifiers("import foo.bar\n", Lang::Python);
        assert_eq!(specs, vec!["foo.bar"]);
    }

    #[test]
    fn extract_specifiers_ts_import_from() {
        let specs = extract_specifiers("import { x } from './util';\n", Lang::Ts);
        assert_eq!(specs, vec!["./util"]);
    }

    #[test]
    fn extract_specifiers_ts_require() {
        let specs = extract_specifiers(r#"const fs = require("./fs");"#, Lang::Ts);
        assert_eq!(specs, vec!["./fs"]);
    }

    #[test]
    fn extract_specifiers_ts_export_from() {
        let specs = extract_specifiers("export { x } from './foo';\n", Lang::Ts);
        assert_eq!(specs, vec!["./foo"]);
    }

    #[test]
    fn extract_specifiers_ts_export_star_from() {
        let specs = extract_specifiers("export * from './bar';\n", Lang::Ts);
        assert_eq!(specs, vec!["./bar"]);
    }

    #[test]
    fn extract_specifiers_ts_dynamic_import() {
        let specs = extract_specifiers("const x = await import('./bar');\n", Lang::Ts);
        assert_eq!(specs, vec!["./bar"]);
    }

    #[test]
    fn extract_specifiers_ts_line_comment_does_not_match() {
        // Copilot finding 11: commented-out dynamic imports must not leak.
        let specs = extract_specifiers("// import('./should-not-match')\n", Lang::Ts);
        assert!(
            specs.is_empty(),
            "line comment leaked specifiers: {specs:?}"
        );
    }

    #[test]
    fn extract_specifiers_ts_block_comment_does_not_match() {
        let specs = extract_specifiers("/* import('./also-should-not-match') */\n", Lang::Ts);
        assert!(
            specs.is_empty(),
            "block comment leaked specifiers: {specs:?}"
        );
    }

    #[test]
    fn extract_specifiers_ts_block_comment_continuation_does_not_match() {
        let specs = extract_specifiers(" * import('./should-not-match')\n", Lang::Ts);
        assert!(
            specs.is_empty(),
            "block comment continuation leaked specifiers: {specs:?}"
        );
    }

    #[test]
    fn has_dynamic_import_requires_token_boundary() {
        // True positives.
        assert!(has_dynamic_import("await import('./x')"));
        assert!(has_dynamic_import("const r = await import('./x')"));
        assert!(has_dynamic_import("foo = import('./x')"));
        assert!(has_dynamic_import("import('./x')"));
        // False positives ruled out by the boundary check.
        assert!(!has_dynamic_import("myimport('./x')"));
        assert!(!has_dynamic_import("someimport('./x')"));
        assert!(!has_dynamic_import("0import('./x')")); // not really TS but covers digit boundary
        assert!(!has_dynamic_import("_import('./x')"));
        assert!(!has_dynamic_import("$import('./x')"));
    }

    #[test]
    fn is_manifest_matches_known_names() {
        assert!(is_manifest("Cargo.toml"));
        assert!(is_manifest("go.mod"));
        assert!(is_manifest("package.json"));
        assert!(is_manifest("pyproject.toml"));
        assert!(!is_manifest("README.md"));
    }

    #[test]
    fn aggregate_region_edges_bounds_co_change_rate_to_unit_interval() {
        // 10 file-edges between region 1 and region 2, each with the
        // per-file cap (1.0). The mean must be 1.0, NOT 10.0.
        let edges: Vec<EdgeEstimate> = (0..10)
            .map(|i| EdgeEstimate {
                from: i,
                to: 10 + i,
                confidence: 1.0,
                language: Lang::Rust,
            })
            .collect();
        let mut file_regions: Vec<FileRegion> = Vec::with_capacity(20);
        for i in 0..10 {
            file_regions.push(FileRegion {
                file_index: i,
                region: 1,
                depth: 1,
                size: 0,
            });
        }
        for i in 10..20 {
            file_regions.push(FileRegion {
                file_index: i,
                region: 2,
                depth: 1,
                size: 0,
            });
        }

        let region_edges = aggregate_region_edges(&edges, &file_regions);
        assert_eq!(region_edges.len(), 1);
        let (a, b, conf) = region_edges[0];
        assert_eq!((a, b), (1, 2));
        assert!(
            (0.0..=1.0).contains(&conf),
            "mean confidence {conf} escaped [0, 1] — co_change_rate contract broken"
        );
        // With every per-edge confidence at 1.0, the mean must also be 1.0.
        assert!((conf - 1.0).abs() < 1e-6, "expected mean=1.0, got {conf}");
    }

    #[test]
    fn aggregate_region_edges_averages_mixed_confidences() {
        // Two file-edges in the same region pair with different
        // confidences — emitted region edge must carry their mean.
        let edges = vec![
            EdgeEstimate {
                from: 0,
                to: 2,
                confidence: 0.4,
                language: Lang::Rust,
            },
            EdgeEstimate {
                from: 1,
                to: 3,
                confidence: 0.8,
                language: Lang::Rust,
            },
        ];
        let file_regions = vec![
            FileRegion {
                file_index: 0,
                region: 1,
                depth: 1,
                size: 0,
            },
            FileRegion {
                file_index: 1,
                region: 1,
                depth: 1,
                size: 0,
            },
            FileRegion {
                file_index: 2,
                region: 2,
                depth: 1,
                size: 0,
            },
            FileRegion {
                file_index: 3,
                region: 2,
                depth: 1,
                size: 0,
            },
        ];
        let region_edges = aggregate_region_edges(&edges, &file_regions);
        assert_eq!(region_edges.len(), 1);
        let (_, _, conf) = region_edges[0];
        assert!((conf - 0.6).abs() < 1e-6, "expected mean=0.6, got {conf}");
    }
}
