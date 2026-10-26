//! ADR-0045 T9: derived paths (build output, tool state) are never
//! proposals. They live in this filesystem's scratch directory, outside the
//! store, its tree root and the checkout, and are gone with the filesystem.

use leyline_cli_lib::source_write::{Base, Grant, Proposed};
use leyline_cli_lib::tree::{self as store_tree, Entry as StoreEntry, Mode};
use tempfile::TempDir;

use super::open_flags::{O_RDONLY, O_RDWR, O_TRUNC};
use super::tests::{A, Fixture, count_passes, fixture_derived, ino, on_disk, passes, whole};
use super::*;

/// A filesystem with derived paths enabled, its scratch under a fresh
/// directory the test owns, limited to `limit` bytes; with a checkout.
fn derived_fixture(limit: u64) -> (Fixture, TempDir) {
    derived_fixture_scoped(whole(), limit)
}

fn derived_fixture_scoped(scope: Scope, limit: u64) -> (Fixture, TempDir) {
    let parent = tempfile::tempdir().unwrap();
    let f = fixture_derived(
        scope,
        true,
        A,
        Some(Derived {
            parent: parent.path().to_path_buf(),
            limit,
        }),
    );
    (f, parent)
}

const MIB: u64 = 1 << 20;

fn entry(f: &Fixture, path: &str) -> Option<StoreEntry> {
    f.fs.store()
        .with_read(|conn| store_tree::entry(conn, path))
        .unwrap()
}

/// Every `_tree` path, in order.
fn tree_paths(f: &Fixture) -> Vec<String> {
    f.fs.store()
        .with_read(|conn| {
            let mut stmt = conn.prepare("SELECT path FROM _tree ORDER BY path")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .unwrap()
}

fn tree_root(f: &Fixture) -> Option<String> {
    f.fs.store()
        .with_read(|conn| {
            Ok(leyline_cli_lib::tree_root::DerivedFrom::read(conn)?
                .map(|d| d.tree_root.to_string()))
        })
        .unwrap()
}

fn mkdir_p(f: &Fixture, path: &str) -> u64 {
    let mut parent = ROOT_INODE;
    for name in path.split('/') {
        parent = match f.fs.lookup(parent, name.as_bytes()) {
            Ok(e) => e.attr.inode,
            Err(_) => f.fs.mkdir(parent, name.as_bytes()).unwrap().attr.inode,
        };
    }
    parent
}

/// CREATE `name` in `parent`, WRITE `text`, FLUSH and RELEASE.
fn write_new(f: &Fixture, parent: u64, name: &str, text: &str) -> Result<u64, Errno> {
    let (entry, h) = f.fs.create(parent, name.as_bytes(), O_RDWR)?;
    f.fs.write(h, 0, text.as_bytes())?;
    f.fs.flush(h)?;
    f.fs.release(h);
    Ok(entry.attr.inode)
}

fn read_text(f: &Fixture, path: &str) -> String {
    let h = f.fs.open(ino(f, path), O_RDONLY).unwrap().handle;
    let bytes = f.fs.read(h, 0, u32::MAX).unwrap();
    f.fs.release(h);
    String::from_utf8(bytes).unwrap()
}

fn names(f: &Fixture, inode: u64) -> Vec<String> {
    f.fs.readdir(inode, 0)
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect()
}

fn scratch(f: &Fixture) -> std::path::PathBuf {
    f.fs.scratch_dir().expect("the filesystem made its scratch")
}

#[test]
fn build_output_under_target_and_node_modules_is_never_a_proposal() {
    let (f, _parent) = derived_fixture(64 * MIB);
    let root_before = tree_root(&f);
    let tree_before = tree_paths(&f);
    count_passes(&f);

    let debug = mkdir_p(&f, "target/debug");
    write_new(&f, debug, "app", "binary bytes").unwrap();
    let pkg = mkdir_p(&f, "node_modules/left-pad");
    write_new(&f, pkg, "index.js", "module.exports = 1;\n").unwrap();

    assert_eq!(passes(&f), 0, "no proposal landed");
    assert_eq!(tree_paths(&f), tree_before, "the store holds no new entry");
    assert_eq!(tree_root(&f), root_before);
    assert_eq!(
        on_disk(&f, "target/debug/app"),
        None,
        "the checkout is unchanged"
    );
    assert_eq!(read_text(&f, "target/debug/app"), "binary bytes");
    assert_eq!(
        read_text(&f, "node_modules/left-pad/index.js"),
        "module.exports = 1;\n"
    );
    let root = names(&f, ROOT_INODE);
    assert!(root.contains(&"target".to_string()), "{root:?}");
    assert!(root.contains(&"node_modules".to_string()), "{root:?}");
    assert!(
        root.contains(&"a.go".to_string()),
        "stored names stay listed: {root:?}"
    );
    assert!(scratch(&f).join("target/debug/app").is_file());
}

#[test]
fn an_authored_file_still_lands_beside_derived_ones() {
    let (f, _parent) = derived_fixture(64 * MIB);
    let target = mkdir_p(&f, "target");
    write_new(&f, target, "out", "x").unwrap();
    write_new(&f, ROOT_INODE, "notes.txt", "kept\n").unwrap();
    assert!(matches!(
        entry(&f, "notes.txt"),
        Some(StoreEntry::File { .. })
    ));
    assert_eq!(on_disk(&f, "notes.txt").as_deref(), Some("kept\n"));
}

#[test]
fn a_tracked_file_under_an_ignored_directory_stays_a_proposal() {
    let (f, _parent) = derived_fixture(64 * MIB);
    f.fs.store()
        .with_write(|conn| {
            leyline_cli_lib::source_write::propose(
                conn,
                &Grant::Whole,
                vec![Proposed::write(
                    "target/keep.txt",
                    Base::Absent,
                    b"v1\n".to_vec(),
                )],
            )
        })
        .unwrap();
    let h =
        f.fs.open(ino(&f, "target/keep.txt"), O_RDWR)
            .unwrap()
            .handle;
    f.fs.write(h, 0, b"v2\n").unwrap();
    f.fs.flush(h).unwrap();
    let Some(StoreEntry::File { blob, .. }) = entry(&f, "target/keep.txt") else {
        panic!("the tracked file is still stored");
    };
    let bytes =
        f.fs.store()
            .with_read(|conn| store_tree::blob_bytes(conn, blob))
            .unwrap()
            .unwrap();
    assert_eq!(bytes, b"v2\n");
    // A new name beside it is derived: ignore rules govern untracked paths.
    write_new(&f, ino(&f, "target"), "new.o", "o").unwrap();
    assert_eq!(entry(&f, "target/new.o"), None);
    assert_eq!(read_text(&f, "target/new.o"), "o");
    let listed = names(&f, ino(&f, "target"));
    assert_eq!(listed, vec!["keep.txt".to_string(), "new.o".to_string()]);
}

#[test]
fn a_gitignore_rule_makes_a_new_path_derived_and_a_whitelist_keeps_one_authored() {
    let (f, _parent) = derived_fixture(64 * MIB);
    write_new(&f, ROOT_INODE, ".gitignore", "build/\n*.log\n!keep.log\n").unwrap();
    assert!(matches!(
        entry(&f, ".gitignore"),
        Some(StoreEntry::File { .. })
    ));
    let build = mkdir_p(&f, "build");
    write_new(&f, build, "out.bin", "bin").unwrap();
    write_new(&f, ROOT_INODE, "run.log", "log").unwrap();
    write_new(&f, ROOT_INODE, "keep.log", "keep").unwrap();
    assert_eq!(entry(&f, "build"), None);
    assert_eq!(entry(&f, "build/out.bin"), None);
    assert_eq!(entry(&f, "run.log"), None);
    assert!(matches!(
        entry(&f, "keep.log"),
        Some(StoreEntry::File { .. })
    ));
    assert_eq!(read_text(&f, "build/out.bin"), "bin");
}

#[test]
fn a_gitignore_in_the_checkout_only_is_read() {
    let (f, _parent) = derived_fixture(64 * MIB);
    std::fs::write(f.src.path().join(".gitignore"), "dist/\n").unwrap();
    let dist = mkdir_p(&f, "dist");
    write_new(&f, dist, "bundle.js", "js").unwrap();
    assert_eq!(entry(&f, "dist"), None);
    assert_eq!(read_text(&f, "dist/bundle.js"), "js");
}

#[test]
fn macos_metadata_files_are_derived() {
    let (f, _parent) = derived_fixture(64 * MIB);
    write_new(&f, ROOT_INODE, "._a.go", "appledouble").unwrap();
    write_new(&f, ino(&f, "pkg"), ".DS_Store", "finder").unwrap();
    assert_eq!(entry(&f, "._a.go"), None);
    assert_eq!(entry(&f, "pkg/.DS_Store"), None);
    assert_eq!(read_text(&f, "._a.go"), "appledouble");
}

#[test]
fn a_rename_from_a_derived_path_to_an_authored_one_lands_a_create() {
    let (f, _parent) = derived_fixture(64 * MIB);
    let target = mkdir_p(&f, "target");
    write_new(&f, target, "gen.go", "package gen\n").unwrap();
    f.fs.rename(target, b"gen.go", ROOT_INODE, b"gen.go", 0)
        .unwrap();
    assert!(matches!(entry(&f, "gen.go"), Some(StoreEntry::File { .. })));
    assert_eq!(on_disk(&f, "gen.go").as_deref(), Some("package gen\n"));
    assert!(!scratch(&f).join("target/gen.go").exists());
    assert_eq!(f.fs.lookup(target, b"gen.go"), Err(Errno::ENOENT));
}

#[test]
fn a_rename_from_an_authored_path_to_a_derived_one_proposes_the_delete() {
    let (f, _parent) = derived_fixture(64 * MIB);
    let target = mkdir_p(&f, "target");
    f.fs.rename(ROOT_INODE, b"a.go", target, b"a.go", 0)
        .unwrap();
    assert_eq!(entry(&f, "a.go"), None);
    assert_eq!(on_disk(&f, "a.go"), None);
    assert_eq!(read_text(&f, "target/a.go"), A);
}

#[test]
fn a_rename_within_derived_paths_moves_the_scratch_file() {
    let (f, _parent) = derived_fixture(64 * MIB);
    let target = mkdir_p(&f, "target");
    write_new(&f, target, "x.tmp", "x").unwrap();
    f.fs.rename(target, b"x.tmp", target, b"x", 0).unwrap();
    assert_eq!(read_text(&f, "target/x"), "x");
    assert_eq!(f.fs.lookup(target, b"x.tmp"), Err(Errno::ENOENT));
}

#[test]
fn a_directory_rename_across_classes_is_exdev() {
    let (f, _parent) = derived_fixture(64 * MIB);
    mkdir_p(&f, "target/sub");
    let target = ino(&f, "target");
    assert_eq!(
        f.fs.rename(target, b"sub", ROOT_INODE, b"sub", 0),
        Err(Errno::EXDEV)
    );
    assert_eq!(
        f.fs.rename(ROOT_INODE, b"pkg", target, b"pkg", 0),
        Err(Errno::EXDEV)
    );
}

#[test]
fn unlink_and_rmdir_remove_derived_entries() {
    let (f, _parent) = derived_fixture(64 * MIB);
    let debug = mkdir_p(&f, "target/debug");
    write_new(&f, debug, "app", "a").unwrap();
    let target = ino(&f, "target");
    assert_eq!(f.fs.rmdir(target, b"debug"), Err(Errno::ENOTEMPTY));
    f.fs.unlink(debug, b"app").unwrap();
    f.fs.rmdir(target, b"debug").unwrap();
    assert_eq!(f.fs.lookup(target, b"debug"), Err(Errno::ENOENT));
    assert!(!scratch(&f).join("target/debug").exists());
}

#[test]
fn a_derived_file_keeps_its_mode_and_size() {
    let (f, _parent) = derived_fixture(64 * MIB);
    let target = mkdir_p(&f, "target");
    let app = write_new(&f, target, "app", "12345").unwrap();
    let (attr, _) =
        f.fs.setattr(
            app,
            None,
            SetAttr {
                mode: Some(0o755),
                ..SetAttr::default()
            },
        )
        .unwrap();
    assert_eq!(attr.perm & 0o111, 0o111);
    assert_eq!(attr.size, 5);
    assert_eq!(attr.kind, Kind::File);
    let (attr, _) =
        f.fs.setattr(
            app,
            None,
            SetAttr {
                size: Some(2),
                ..SetAttr::default()
            },
        )
        .unwrap();
    assert_eq!(attr.size, 2);
    assert_eq!(read_text(&f, "target/app"), "12");
}

#[test]
fn a_symlink_in_scratch_is_served_and_never_followed_on_the_host() {
    let (f, _parent) = derived_fixture(64 * MIB);
    let outside = tempfile::tempdir().unwrap();
    let target = mkdir_p(&f, "node_modules");
    let link =
        f.fs.symlink(
            target,
            b".bin",
            outside.path().as_os_str().as_encoded_bytes(),
        )
        .unwrap();
    assert_eq!(link.attr.kind, Kind::Symlink);
    assert_eq!(
        f.fs.readlink(link.attr.inode).unwrap(),
        outside.path().as_os_str().as_encoded_bytes()
    );
    // The guest's kernel resolves the link; a request naming a path
    // through it reaches storefs only as a lookup under a symlink, which
    // is not a directory here.
    assert_eq!(
        f.fs.create(link.attr.inode, b"escape", O_RDWR).map(|_| ()),
        Err(Errno::ENOTDIR)
    );
    assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    assert_eq!(entry(&f, "node_modules/.bin"), None);
}

#[test]
fn a_write_past_the_scratch_limit_is_enospc_and_nothing_reaches_the_store() {
    let (f, _parent) = derived_fixture(8);
    let target = mkdir_p(&f, "target");
    let (_, h) = f.fs.create(target, b"big", O_RDWR).unwrap();
    assert_eq!(f.fs.write(h, 0, b"12345678"), Ok(8));
    assert_eq!(f.fs.write(h, 8, b"9"), Err(Errno::ENOSPC));
    f.fs.flush(h).unwrap();
    f.fs.release(h);
    assert_eq!(entry(&f, "target/big"), None);
    // Removing the file frees its bytes.
    f.fs.unlink(target, b"big").unwrap();
    write_new(&f, target, "again", "12345678").unwrap();
}

#[test]
fn a_derived_write_outside_the_proposal_scope_is_eacces() {
    let scope = Scope {
        see: Grant::Whole,
        propose: Grant::Paths(["a.go".to_string()].into()),
    };
    let (f, _parent) = derived_fixture_scoped(scope, 64 * MIB);
    assert_eq!(
        f.fs.mkdir(ROOT_INODE, b"target").map(|_| ()),
        Err(Errno::EACCES)
    );
    assert_eq!(
        f.fs.create(ROOT_INODE, b"._a.go", O_RDWR).map(|_| ()),
        Err(Errno::EACCES)
    );
}

#[test]
fn the_scratch_is_gone_when_the_filesystem_drops() {
    let (f, parent) = derived_fixture(64 * MIB);
    let target = mkdir_p(&f, "target");
    write_new(&f, target, "app", "a").unwrap();
    let dir = scratch(&f);
    assert!(dir.starts_with(parent.path()));
    drop(f);
    assert!(!dir.exists());
    assert_eq!(std::fs::read_dir(parent.path()).unwrap().count(), 0);
}

#[test]
fn without_derived_options_build_output_is_a_proposal_as_before() {
    let f = super::tests::fixture_with(whole(), true, A);
    let target = mkdir_p(&f, "target");
    write_new(&f, target, "out", "o").unwrap();
    assert!(matches!(
        entry(&f, "target/out"),
        Some(StoreEntry::File { .. })
    ));
    assert_eq!(f.fs.scratch_dir(), None);
}

fn chmod(f: &Fixture, inode: u64, mode: u32) -> Attr {
    f.fs.setattr(
        inode,
        None,
        SetAttr {
            mode: Some(mode),
            ..SetAttr::default()
        },
    )
    .unwrap()
    .0
}

fn stored_mode(f: &Fixture, path: &str) -> Option<Mode> {
    match entry(f, path) {
        Some(StoreEntry::File { mode, .. }) => Some(mode),
        _ => None,
    }
}

#[test]
fn readdirplus_resumes_a_derived_listing_at_its_offsets() {
    let (f, _parent) = derived_fixture(64 * MIB);
    let target = mkdir_p(&f, "target");
    for name in ["a", "b", "c"] {
        write_new(&f, target, name, name).unwrap();
    }
    let all = f.fs.readdirplus(target, 0).unwrap();
    let listed: Vec<(String, u64)> = all
        .iter()
        .map(|(e, _)| (e.name.clone(), e.offset))
        .collect();
    assert_eq!(
        listed,
        vec![("a".into(), 1), ("b".into(), 2), ("c".into(), 3)]
    );
    let rest: Vec<String> =
        f.fs.readdirplus(target, 1)
            .unwrap()
            .into_iter()
            .map(|(e, _)| e.name)
            .collect();
    assert_eq!(rest, vec!["b".to_string(), "c".to_string()]);
    assert_eq!(all[0].1.attr.size, 1);
}

#[test]
fn a_derived_read_starts_at_its_offset_and_stops_at_the_end() {
    let (f, _parent) = derived_fixture(64 * MIB);
    let target = mkdir_p(&f, "target");
    write_new(&f, target, "x", "12345").unwrap();
    let h = f.fs.open(ino(&f, "target/x"), O_RDONLY).unwrap().handle;
    assert_eq!(f.fs.read(h, 2, 100).unwrap(), b"345");
    assert_eq!(f.fs.read(h, 1, 2).unwrap(), b"23");
    assert_eq!(f.fs.read(h, 5, 10).unwrap(), b"");
    assert_eq!(f.fs.read(h, 9, 10).unwrap(), b"");
}

#[test]
fn a_read_only_derived_handle_neither_writes_nor_truncates() {
    let (f, _parent) = derived_fixture(64 * MIB);
    let target = mkdir_p(&f, "target");
    let x = write_new(&f, target, "x", "12345").unwrap();
    let h = f.fs.open(x, O_RDONLY).unwrap().handle;
    assert_eq!(f.fs.write(h, 0, b"z"), Err(Errno::EBADF));
    assert_eq!(
        f.fs.setattr(
            x,
            Some(h),
            SetAttr {
                size: Some(1),
                ..SetAttr::default()
            }
        )
        .map(|_| ()),
        Err(Errno::EBADF)
    );
    assert_eq!(read_text(&f, "target/x"), "12345");
}

#[test]
fn a_derived_open_truncates_only_with_o_trunc() {
    let (f, _parent) = derived_fixture(64 * MIB);
    let target = mkdir_p(&f, "target");
    let x = write_new(&f, target, "x", "12345").unwrap();
    let h = f.fs.open(x, O_RDWR).unwrap().handle;
    f.fs.write(h, 0, b"a").unwrap();
    f.fs.release(h);
    assert_eq!(read_text(&f, "target/x"), "a2345");
    let h = f.fs.open(x, O_RDWR | O_TRUNC).unwrap().handle;
    f.fs.release(h);
    assert_eq!(read_text(&f, "target/x"), "");
}

#[test]
fn a_derived_files_attributes_carry_its_permission_bits() {
    let (f, _parent) = derived_fixture(64 * MIB);
    let target = mkdir_p(&f, "target");
    let x = write_new(&f, target, "x", "x").unwrap();
    assert_eq!(chmod(&f, x, 0o640).perm, 0o640);
    assert_eq!(f.fs.lookup(target, b"x").unwrap().attr.perm, 0o640);
    let link = f.fs.symlink(target, b"l", b"x").unwrap();
    assert_eq!(link.attr.perm, 0o777);
    assert_eq!(link.attr.kind, Kind::Symlink);
}

#[test]
fn a_file_renamed_out_of_scratch_lands_with_its_mode_and_over_a_stored_file() {
    let (f, _parent) = derived_fixture(64 * MIB);
    let target = mkdir_p(&f, "target");
    let run = write_new(&f, target, "run", "#!/bin/sh\n").unwrap();
    chmod(&f, run, 0o755);
    write_new(&f, target, "plain", "plain\n").unwrap();
    write_new(&f, target, "new_a", "package a\n").unwrap();
    f.fs.rename(target, b"run", ROOT_INODE, b"run.sh", 0)
        .unwrap();
    f.fs.rename(target, b"plain", ROOT_INODE, b"plain.txt", 0)
        .unwrap();
    f.fs.rename(target, b"new_a", ROOT_INODE, b"a.go", 0)
        .unwrap();
    assert_eq!(stored_mode(&f, "run.sh"), Some(Mode::Executable));
    assert_eq!(stored_mode(&f, "plain.txt"), Some(Mode::Regular));
    assert_eq!(on_disk(&f, "a.go").as_deref(), Some("package a\n"));
}

#[test]
fn a_file_named_like_a_generated_directory_is_authored() {
    let (f, _parent) = derived_fixture(64 * MIB);
    let target = mkdir_p(&f, "target");
    write_new(&f, target, "venv", "a file, not a directory\n").unwrap();
    f.fs.rename(target, b"venv", ROOT_INODE, b"venv", 0)
        .unwrap();
    assert!(matches!(entry(&f, "venv"), Some(StoreEntry::File { .. })));
}

#[test]
fn a_file_renamed_into_scratch_keeps_its_mode() {
    let (f, _parent) = derived_fixture(64 * MIB);
    let tool = write_new(&f, ROOT_INODE, "tool.sh", "#!/bin/sh\n").unwrap();
    chmod(&f, tool, 0o755);
    assert_eq!(stored_mode(&f, "tool.sh"), Some(Mode::Executable));
    let target = mkdir_p(&f, "target");
    f.fs.rename(ROOT_INODE, b"tool.sh", target, b"tool.sh", 0)
        .unwrap();
    f.fs.rename(ROOT_INODE, b"a.go", target, b"a.go", 0)
        .unwrap();
    assert_eq!(f.fs.lookup(target, b"tool.sh").unwrap().attr.perm, 0o755);
    assert_eq!(f.fs.lookup(target, b"a.go").unwrap().attr.perm, 0o644);
}

#[test]
fn renames_touching_scratch_need_the_scope_at_both_ends() {
    let scope = Scope {
        see: Grant::Whole,
        propose: Grant::Paths(["a.go", "target/a", "target/keep"].map(String::from).into()),
    };
    let (f, _parent) = derived_fixture_scoped(scope, 64 * MIB);
    let target = mkdir_p(&f, "target");
    write_new(&f, target, "a", "a").unwrap();
    assert_eq!(
        f.fs.rename(target, b"a", target, b"b", 0),
        Err(Errno::EACCES)
    );
    assert_eq!(
        f.fs.rename(ROOT_INODE, b"a.go", target, b"x", 0),
        Err(Errno::EACCES)
    );
    assert!(matches!(entry(&f, "a.go"), Some(StoreEntry::File { .. })));
}

#[test]
fn a_rename_within_scratch_is_refused_as_rename_2_refuses() {
    let (f, _parent) = derived_fixture(64 * MIB);
    mkdir_p(&f, "target/d/sub");
    mkdir_p(&f, "target/full/child");
    mkdir_p(&f, "target/empty");
    let target = ino(&f, "target");
    write_new(&f, target, "f", "f").unwrap();
    let sub = ino(&f, "target/d/sub");
    assert_eq!(
        f.fs.rename(target, b"d", sub, b"inside", 0),
        Err(Errno::EINVAL)
    );
    assert_eq!(
        f.fs.rename(target, b"d", target, b"f", 0),
        Err(Errno::ENOTDIR)
    );
    assert_eq!(
        f.fs.rename(target, b"f", target, b"d", 0),
        Err(Errno::EISDIR)
    );
    let over_full = f.fs.rename(target, b"empty", target, b"full", 0);
    assert!(
        matches!(over_full, Err(Errno::ENOTEMPTY) | Err(Errno::EEXIST)),
        "{over_full:?}"
    );
}

#[test]
fn a_nested_gitignore_applies_beneath_its_directory_only() {
    let (f, _parent) = derived_fixture(64 * MIB);
    let pkg = ino(&f, "pkg");
    // Anchored: the rule names `gen` directly in `pkg`, so it matches only
    // the path relative to `pkg` exactly.
    write_new(&f, pkg, ".gitignore", "/gen/\n").unwrap();
    let gen_dir = mkdir_p(&f, "pkg/gen");
    write_new(&f, gen_dir, "out.go", "generated\n").unwrap();
    assert_eq!(entry(&f, "pkg/gen"), None);
    assert_eq!(read_text(&f, "pkg/gen/out.go"), "generated\n");
    mkdir_p(&f, "pkg/sub/gen");
    assert_eq!(
        entry(&f, "pkg/sub/gen"),
        Some(StoreEntry::Dir),
        "anchored to pkg"
    );
    mkdir_p(&f, "gen");
    assert_eq!(entry(&f, "gen"), Some(StoreEntry::Dir));
}

/// The rule matcher alone.
#[test]
fn the_rules_name_generated_directories_and_macos_metadata_only() {
    for derived in [
        "target/x",
        "a/target/x",
        "node_modules/p/i.js",
        "__pycache__/m.pyc",
        "__pypackages__/x",
        "venv/bin/python",
        ".venv/bin/python",
        ".git/HEAD",
        "._README.md",
        "pkg/.DS_Store",
    ] {
        assert!(builtin_derived(derived, false), "{derived}");
    }
    for authored in [
        "target",
        "vendor/x.go",
        ".github/workflows/ci.yml",
        ".gitignore",
        "src/target.rs",
        "my_target/x",
        "README.md",
    ] {
        assert!(!builtin_derived(authored, false), "{authored}");
    }
    assert!(builtin_derived("target", true), "a directory named target");
}
