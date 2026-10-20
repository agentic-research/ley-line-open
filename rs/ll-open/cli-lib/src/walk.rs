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
    out.extend(walk_tree(root)?.files);
    Ok(())
}

/// Everything one walk of a source tree finds (ADR-0045 T3): regular
/// files, symlinks (never followed), and directories below the root, each
/// sorted. A directory is listed whether or not anything is walked under it.
#[derive(Debug, Default)]
pub struct Walked {
    pub files: Vec<PathBuf>,
    pub symlinks: Vec<PathBuf>,
    pub dirs: Vec<PathBuf>,
}

/// Walk `root` with the excludes of [`walk_source_tree`], keeping symlinks
/// and directories as well as files.
pub fn walk_tree(root: &Path) -> Result<Walked> {
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
    let mut walked = Walked::default();
    for entry in walker {
        let entry = entry.with_context(|| format!("walk under {}", root.display()))?;
        if entry.depth() == 0 {
            continue;
        }
        let Some(kind) = entry.file_type() else {
            continue;
        };
        if kind.is_symlink() {
            walked.symlinks.push(entry.into_path());
        } else if kind.is_dir() {
            walked.dirs.push(entry.into_path());
        } else if kind.is_file() {
            walked.files.push(entry.into_path());
        }
    }
    walked.files.sort();
    walked.symlinks.sort();
    walked.dirs.sort();
    Ok(walked)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn walk_tree_keeps_symlinks_and_directories_and_skips_excludes() {
        let td = tempfile::tempdir().unwrap();
        let root = td.path();
        std::fs::create_dir_all(root.join("empty")).unwrap();
        std::fs::create_dir_all(root.join("sub/deeper")).unwrap();
        std::fs::create_dir_all(root.join("node_modules/x")).unwrap();
        std::fs::write(root.join("sub/a.go"), "package a\n").unwrap();
        std::fs::write(root.join("README.md"), "hi\n").unwrap();
        std::fs::write(root.join("node_modules/x/y.js"), "x\n").unwrap();
        std::os::unix::fs::symlink("README.md", root.join("link")).unwrap();
        let w = walk_tree(root).unwrap();
        let rel = |v: &[PathBuf]| -> Vec<String> {
            v.iter()
                .map(|p| p.strip_prefix(root).unwrap().to_string_lossy().into_owned())
                .collect()
        };
        assert_eq!(rel(&w.files), ["README.md", "sub/a.go"]);
        assert_eq!(rel(&w.symlinks), ["link"]);
        assert_eq!(rel(&w.dirs), ["empty", "sub", "sub/deeper"]);
    }
}
