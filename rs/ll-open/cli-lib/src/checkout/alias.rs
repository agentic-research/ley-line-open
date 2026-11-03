//! Path aliasing in a checkout (bead `ley-line-open-23a26c`, ADR-0043 R3).
//!
//! `_tree` paths are byte-distinct; a checkout's file system need not keep
//! them apart. The default APFS volume folds case and Unicode
//! normalization, so `README.md` and `Readme.md`, or NFC `é` and NFD `é`,
//! name one file there. Two store entries on one file never agree with the
//! directory again, so a create the checkout would fold onto another entry
//! is refused.
//!
//! The decision is a pure function of a folding rule ([`Fold`]), the
//! directory's listing, and what the store holds ([`alias`]), so it is
//! tested on any file system with a simulated one. Which rule a checkout
//! uses is asked of its file system once ([`Fold::of`]), never assumed from
//! the platform.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::{Context, Result};
use unicode_normalization::UnicodeNormalization;

use crate::source_write::{Change, Proposed};

/// How a file system compares names: whether it folds case, and whether it
/// folds Unicode normalization (NFC and NFD spellings of one name).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Fold {
    pub case: bool,
    pub normalization: bool,
}

impl Fold {
    /// A file system that compares names byte for byte.
    pub const NONE: Fold = Fold {
        case: false,
        normalization: false,
    };

    /// The key two names share when this file system takes them for one
    /// name: NFC when it folds normalization, then lowercase when it folds
    /// case. Unicode lowercase approximates a file system's case table; a
    /// pair only the real table folds is not caught.
    pub fn key(self, name: &str) -> String {
        let normal: String = if self.normalization {
            name.nfc().collect()
        } else {
            name.to_string()
        };
        if self.case {
            normal.to_lowercase()
        } else {
            normal
        }
    }

    /// The rule the file system under `dir` uses, probed once per directory
    /// for the life of the process.
    pub fn of(dir: &Path) -> Result<Fold> {
        static PROBED: OnceLock<Mutex<HashMap<PathBuf, Fold>>> = OnceLock::new();
        let mut probed = PROBED
            .get_or_init(|| Mutex::new(HashMap::new()))
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(fold) = probed.get(dir) {
            return Ok(*fold);
        }
        let fold = Fold::probe(dir)?;
        probed.insert(dir.to_path_buf(), fold);
        Ok(fold)
    }

    /// Ask the file system under `dir`: names are made in a hidden scratch
    /// directory inside it (one file system; a directory walk skips it), and
    /// each folded spelling looked up.
    pub fn probe(dir: &Path) -> Result<Fold> {
        let scratch = tempfile::Builder::new()
            .prefix(".leyline-fold-probe")
            .tempdir_in(dir)
            .with_context(|| format!("make a scratch directory in {}", dir.display()))?;
        probe_with(
            |name| Ok(std::fs::write(scratch.path().join(name), b"")?),
            |name| scratch.path().join(name).exists(),
        )
    }
}

/// The rule a file system uses, from making names (`make`) and looking up
/// other spellings of them (`exists`).
pub fn probe_with(
    mut make: impl FnMut(&str) -> Result<()>,
    exists: impl Fn(&str) -> bool,
) -> Result<Fold> {
    make("A")?;
    let case = exists("a");
    make("\u{e9}")?;
    let normalization = exists("e\u{301}");
    Ok(Fold {
        case,
        normalization,
    })
}

/// The names the directory `rel` (relative to `dir`; `""` is `dir`)
/// holds, or `None` when it is not a directory.
pub fn listing(dir: &Path, rel: &str) -> Result<Option<Vec<String>>> {
    let at = dir.join(rel);
    let entries = match std::fs::read_dir(&at) {
        Ok(entries) => entries,
        Err(e)
            if matches!(
                e.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(None);
        }
        Err(e) => return Err(e).with_context(|| format!("list {}", at.display())),
    };
    let mut names = Vec::new();
    for entry in entries {
        names.push(entry?.file_name().to_string_lossy().into_owned());
    }
    Ok(Some(names))
}

/// The first path `writes` creates that `fold` makes another entry's,
/// with that entry: `(created, other)`. A created path is a create or mkdir
/// of a plain relative path the store does not hold (`stored`). It aliases
/// an entry of the directory (`list`) that one of its components folds onto
/// under another spelling, unless the same proposal deletes that entry (a
/// rename); and it aliases a path an earlier write of the proposal creates
/// when a prefix of each folds to one key under two spellings.
pub fn alias(
    fold: Fold,
    writes: &[Proposed],
    stored: impl Fn(&str) -> Result<bool>,
    list: impl Fn(&str) -> Result<Option<Vec<String>>>,
) -> Result<Option<(String, String)>> {
    let removed: BTreeSet<&str> = writes
        .iter()
        .filter(|w| matches!(w.change, Change::Delete | Change::Rmdir))
        .map(|w| w.path.as_str())
        .collect();
    let mut created = Vec::new();
    for w in writes {
        if matches!(w.change, Change::Write { .. } | Change::Mkdir)
            && super::inside(&w.path)
            && !stored(&w.path)?
        {
            created.push(w.path.as_str());
        }
    }
    for path in &created {
        if let Some(other) = alias_in_listing(fold, path, &list)?
            && !removed.contains(other.as_str())
        {
            return Ok(Some((path.to_string(), other)));
        }
    }
    let mut spelled: BTreeMap<String, String> = BTreeMap::new();
    for path in &created {
        let parts: Vec<&str> = path.split('/').collect();
        let keys: Vec<String> = parts.iter().map(|p| fold.key(p)).collect();
        for end in 1..=parts.len() {
            let prefix = parts[..end].join("/");
            match spelled.get(&keys[..end].join("/")) {
                Some(other) if *other != prefix => {
                    return Ok(Some((path.to_string(), other.clone())));
                }
                Some(_) => {}
                None => {
                    spelled.insert(keys[..end].join("/"), prefix);
                }
            }
        }
    }
    Ok(None)
}

/// The entry of the listing that a component of `path` folds onto under
/// another spelling, as the directory spells it; `None` when the path
/// reaches a component nothing in its directory folds onto.
fn alias_in_listing(
    fold: Fold,
    path: &str,
    list: &impl Fn(&str) -> Result<Option<Vec<String>>>,
) -> Result<Option<String>> {
    let parts: Vec<&str> = path.split('/').collect();
    for (i, part) in parts.iter().enumerate() {
        let parent = parts[..i].join("/");
        let Some(names) = list(&parent)? else {
            return Ok(None);
        };
        if names.iter().any(|n| n == part) {
            continue;
        }
        let key = fold.key(part);
        return Ok(names
            .into_iter()
            .find(|n| fold.key(n) == key)
            .map(|n| match parent.as_str() {
                "" => n,
                _ => format!("{parent}/{n}"),
            }));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source_write::Base;

    const NFC: &str = "\u{e9}.txt";
    const NFD: &str = "e\u{301}.txt";
    const CASE: Fold = Fold {
        case: true,
        normalization: false,
    };
    const NORMAL: Fold = Fold {
        case: false,
        normalization: true,
    };
    const APFS: Fold = Fold {
        case: true,
        normalization: true,
    };

    #[test]
    fn a_key_folds_exactly_what_the_rule_folds() {
        assert_eq!(Fold::NONE.key("Readme.md"), "Readme.md");
        assert_eq!(Fold::NONE.key(NFD), NFD);
        assert_eq!(CASE.key("Readme.md"), "readme.md");
        assert_eq!(CASE.key(NFD), NFD);
        assert_eq!(NORMAL.key("Readme.md"), "Readme.md");
        assert_eq!(NORMAL.key(NFD), NFC);
        assert_eq!(APFS.key("\u{c9}.TXT"), NFC);
        assert_eq!(APFS.key("E\u{301}.txt"), NFC);
    }

    /// A simulated file system that folds names by `fold`.
    fn simulated(fold: Fold) -> Fold {
        let made = std::cell::RefCell::new(Vec::<String>::new());
        probe_with(
            |name| {
                made.borrow_mut().push(name.to_string());
                Ok(())
            },
            |name| made.borrow().iter().any(|m| fold.key(m) == fold.key(name)),
        )
        .unwrap()
    }

    #[test]
    fn the_probe_reads_the_rule_the_file_system_folds_by() {
        for fold in [Fold::NONE, CASE, NORMAL, APFS] {
            assert_eq!(simulated(fold), fold);
        }
        let failed = probe_with(|_| anyhow::bail!("read-only"), |_| true);
        assert!(failed.is_err(), "a probe that cannot make a name fails");
    }

    /// The real probe agrees with the file system it runs on, and leaves
    /// nothing behind.
    #[test]
    fn the_probe_of_a_directory_matches_its_file_system() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("X"), b"").unwrap();
        std::fs::write(dir.path().join(NFC), b"").unwrap();
        let want = Fold {
            case: dir.path().join("x").exists(),
            normalization: dir.path().join(NFD).exists(),
        };
        assert_eq!(Fold::probe(dir.path()).unwrap(), want);
        assert_eq!(Fold::of(dir.path()).unwrap(), want);
        assert_eq!(Fold::of(dir.path()).unwrap(), want, "and from the cache");
        let left: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left.len(), 2, "{left:?}");
    }

    #[test]
    fn a_listing_names_a_directory_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("docs")).unwrap();
        std::fs::write(dir.path().join("docs/a.md"), b"").unwrap();
        assert_eq!(listing(dir.path(), "").unwrap(), Some(vec!["docs".into()]));
        assert_eq!(
            listing(dir.path(), "docs").unwrap(),
            Some(vec!["a.md".into()])
        );
        assert_eq!(listing(dir.path(), "none").unwrap(), None);
        assert_eq!(listing(dir.path(), "docs/a.md").unwrap(), None);
    }

    /// A checkout holding `README.md`, `docs/a.md` and NFC `é.txt`, and a
    /// store holding `stored`.
    fn decide(fold: Fold, stored: &[&str], writes: &[Proposed]) -> Option<(String, String)> {
        let tree: BTreeMap<&str, Vec<String>> = BTreeMap::from([
            ("", vec!["README.md".into(), "docs".into(), NFC.into()]),
            ("docs", vec!["a.md".into()]),
        ]);
        alias(
            fold,
            writes,
            |p| Ok(stored.contains(&p)),
            |d| Ok(tree.get(d).cloned()),
        )
        .unwrap()
    }

    fn new_file(path: &str) -> Proposed {
        Proposed::create(path, b"x\n".to_vec())
    }

    fn pair(a: &str, b: &str) -> Option<(String, String)> {
        Some((a.to_string(), b.to_string()))
    }

    #[test]
    fn a_create_that_folds_onto_an_entry_aliases_it() {
        let readme = [new_file("Readme.md")];
        assert_eq!(decide(CASE, &[], &readme), pair("Readme.md", "README.md"));
        assert_eq!(decide(APFS, &[], &readme), pair("Readme.md", "README.md"));
        assert_eq!(decide(NORMAL, &[], &readme), None);
        assert_eq!(decide(Fold::NONE, &[], &readme), None);

        let nfd = [new_file(NFD)];
        assert_eq!(decide(NORMAL, &[], &nfd), pair(NFD, NFC));
        assert_eq!(decide(APFS, &[], &nfd), pair(NFD, NFC));
        assert_eq!(decide(CASE, &[], &nfd), None);
        assert_eq!(decide(Fold::NONE, &[], &nfd), None);

        assert_eq!(
            decide(CASE, &[], &[new_file("Docs/b.md")]),
            pair("Docs/b.md", "docs")
        );
        assert_eq!(
            decide(CASE, &[], &[Proposed::mkdir("DOCS")]),
            pair("DOCS", "docs")
        );
        assert_eq!(
            decide(CASE, &[], &[new_file("docs/A.md")]),
            pair("docs/A.md", "docs/a.md")
        );
    }

    #[test]
    fn a_create_spelled_as_the_directory_spells_it_is_no_alias() {
        for path in ["docs/b.md", "other.md", "new/x.md", "docs/a.md/x"] {
            assert_eq!(decide(APFS, &[], &[new_file(path)]), None, "{path}");
        }
    }

    #[test]
    fn a_path_the_store_holds_is_its_own_name() {
        let write = [Proposed::write("Readme.md", Base::Any, b"y\n".to_vec())];
        assert_eq!(decide(APFS, &["Readme.md"], &write), None);
        assert_eq!(decide(APFS, &[], &write), pair("Readme.md", "README.md"));
    }

    #[test]
    fn a_case_only_rename_lands_and_another_delete_does_not_license_it() {
        let rename = |deleted: &str| [Proposed::delete(deleted, Base::Any), new_file("Readme.md")];
        assert_eq!(decide(APFS, &[], &rename("README.md")), None);
        assert_eq!(
            decide(APFS, &[], &rename("docs/a.md")),
            pair("Readme.md", "README.md")
        );
        assert_eq!(
            decide(APFS, &[], &[Proposed::rmdir("docs"), new_file("Docs/b.md")]),
            None
        );
    }

    #[test]
    fn a_delete_creates_nothing_and_a_malformed_path_is_left_to_the_proposal() {
        assert_eq!(
            decide(APFS, &[], &[Proposed::delete("Readme.md", Base::Any)]),
            None
        );
        assert_eq!(decide(APFS, &[], &[new_file("Readme.md/../x")]), None);
    }

    #[test]
    fn two_creates_of_one_proposal_that_fold_together_alias() {
        let both = |a: &str, b: &str| decide(APFS, &[], &[new_file(a), new_file(b)]);
        assert_eq!(both("New.md", "new.md"), pair("new.md", "New.md"));
        assert_eq!(both("Sub/a.md", "sub/b.md"), pair("sub/b.md", "Sub"));
        assert_eq!(both("Sub/a.md", "Sub/A.md"), pair("Sub/A.md", "Sub/a.md"));
        assert_eq!(
            both("n\u{e9}.md", "ne\u{301}.md"),
            pair("ne\u{301}.md", "n\u{e9}.md")
        );
        assert_eq!(both("sub/a.md", "sub/b.md"), None);
        assert_eq!(both("a/b", "ab"), None);
        let none = |a: &str, b: &str| decide(Fold::NONE, &[], &[new_file(a), new_file(b)]);
        assert_eq!(none("New.md", "new.md"), None);
        assert_eq!(none("Sub/a.md", "sub/b.md"), None);
    }

    #[test]
    fn a_listing_that_fails_fails_the_decision() {
        let failed = alias(
            APFS,
            &[new_file("x.md")],
            |_| Ok(false),
            |_| anyhow::bail!("unreadable"),
        );
        assert!(failed.is_err());
        let failed = alias(
            APFS,
            &[new_file("x.md")],
            |_| anyhow::bail!("no store"),
            |_| Ok(None),
        );
        assert!(failed.is_err());
    }
}
