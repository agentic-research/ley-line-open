//! Source-tree walker that honors `.gitignore` (bead
//! ley-line-open-25685d).
//!
//! Wraps the `ignore` crate (same primitive `rg` + mache use) so LLO's
//! ingest respects `.gitignore`, `.ignore`, `.git/info/exclude`, and
//! global git excludes. The existing `is_bloat_dir` skiplist is
//! layered on top via `filter_entry` so `node_modules`/`target`/
//! `__pycache__`/etc still get skipped even when there's no matching
//! gitignore entry.
//!
//! `require_git(false)` is set so a `.gitignore` in a tree that is
//! NOT a git repo (test fixtures, extracted tarballs) is still
//! honored — the semantics are "this file names things I don't want
//! indexed", not "this file is a git artifact".
//!
//! Both `cmd_parse::collect_files` and `topology_pass::collect_files`
//! delegate here so the walk policy lives in exactly one place.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

use crate::cmd_parse::is_bloat_dir;

/// Walk `root` recursively, returning every source file. Skips
/// gitignored entries + `is_bloat_dir` matches. Output is sorted for
/// deterministic ingest order.
pub fn walk_source_tree(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    walk_into(root, &mut out)?;
    out.sort();
    Ok(out)
}

/// Same walk as `walk_source_tree`, but appends to an existing buffer
/// (matches the `cmd_parse::collect_files(dir, &mut out)` shape).
pub fn walk_into(root: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let walker = ignore::WalkBuilder::new(root)
        .require_git(false)
        .filter_entry(|entry| {
            entry
                .file_name()
                .to_str()
                .map(|n| !is_bloat_dir(n))
                .unwrap_or(true)
        })
        .build();
    for entry in walker {
        let entry = entry.with_context(|| format!("walk under {}", root.display()))?;
        if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
            out.push(entry.into_path());
        }
    }
    Ok(())
}
