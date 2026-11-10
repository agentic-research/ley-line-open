//! The in-memory filesystem an OCI import applies its layers to.
//!
//! Nothing here touches the host filesystem. Layers are applied to this tree,
//! symbolic links are refused or resolved against it, and only the finished
//! tree — directories and regular files, nothing else — is written out. A
//! hostile archive therefore never gets the chance to make the host follow a
//! link it planted: there are no links on disk while the import runs.

use std::collections::{BTreeMap, VecDeque};

/// Linux's `MAXSYMLINKS`: how many links one resolution may traverse.
const MAX_SYMLINK_HOPS: usize = 40;

/// The top-level directories libkrun's guest init mounts a kernel
/// filesystem over before the workload runs (`mount_filesystems` in
/// `rs/third_party/libkrun/src/init_blob/init/init.c`): devtmpfs on `/dev`
/// (init.c:550, then devpts on `/dev/pts` at :594 and tmpfs on `/dev/shm` at
/// :601), proc on `/proc` (:566), sysfs on `/sys` (:573, then cgroup2 on
/// `/sys/fs/cgroup` at :580). Each mount either succeeds, is already there
/// (`EBUSY`), or makes init exit before anything runs (:1495-1498). Nothing
/// an image holds beneath these is visible to the workload, and nothing a
/// link names beneath them is the image's to supply. `/tmp` is not one:
/// init mounts there only in TDX builds (:1467-1487), and then an ext4
/// volume, not a kernel filesystem.
pub(crate) const KERNEL_MOUNTED: [&str; 3] = ["dev", "proc", "sys"];

/// Whether `path`, from the image root, lies strictly beneath one of
/// [`KERNEL_MOUNTED`] (the mount point itself is not beneath it).
pub(crate) fn under_kernel_mount(path: &[String]) -> bool {
    path.len() >= 2 && KERNEL_MOUNTED.contains(&path[0].as_str())
}

/// A regular file's content (by BLAKE3, held in the import's content store)
/// and permission bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FileNode {
    pub content: blake3::Hash,
    pub size: u64,
    pub mode: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Node {
    Dir(Dir),
    File(FileNode),
    Symlink(String),
}

pub(crate) type Dir = BTreeMap<String, Node>;

/// Why a layer entry cannot be applied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PathError {
    TraversesSymlink,
    ParentNotDirectory,
    HardlinkTargetMissing,
    HardlinkTargetNotFile,
}

impl PathError {
    pub(crate) fn reason(self) -> &'static str {
        match self {
            Self::TraversesSymlink => "the path passes through a symbolic link",
            Self::ParentNotDirectory => "a parent of the path is not a directory",
            Self::HardlinkTargetMissing => "the hard link's target does not exist",
            Self::HardlinkTargetNotFile => "the hard link's target is not a regular file",
        }
    }
}

/// The finished tree: what the rootfs manifest can express.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Final {
    Dir(BTreeMap<String, Final>),
    File(FileNode),
}

impl Final {
    /// The tree without any regular file whose owner-read bit is clear, and
    /// those files as `(path, mode)` in path order. Nothing unprivileged can
    /// read one: not the import's verifier, not the CAS resolver, not the
    /// guest's virtio-fs server, which runs as the same unprivileged user.
    pub(crate) fn without_unreadable_files(self) -> (Final, Vec<(String, u32)>) {
        fn filter(
            entries: BTreeMap<String, Final>,
            prefix: &str,
            dropped: &mut Vec<(String, u32)>,
        ) -> BTreeMap<String, Final> {
            entries
                .into_iter()
                .filter_map(|(name, node)| match node {
                    Final::File(file) if file.mode & 0o400 == 0 => {
                        dropped.push((join(prefix, &name), file.mode));
                        None
                    }
                    Final::File(file) => Some((name, Final::File(file))),
                    Final::Dir(children) => {
                        let children = filter(children, &join(prefix, &name), dropped);
                        Some((name, Final::Dir(children)))
                    }
                })
                .collect()
        }
        let mut dropped = Vec::new();
        let tree = match self {
            Final::Dir(entries) => Final::Dir(filter(entries, "", &mut dropped)),
            file => file,
        };
        (tree, dropped)
    }

    /// The tree without any directory that holds no regular file, at any
    /// depth. The root directory is kept.
    pub(crate) fn without_empty_directories(self) -> Final {
        fn prune(entries: BTreeMap<String, Final>) -> BTreeMap<String, Final> {
            entries
                .into_iter()
                .filter_map(|(name, node)| match node {
                    Final::File(file) => Some((name, Final::File(file))),
                    Final::Dir(children) => {
                        let children = prune(children);
                        (!children.is_empty()).then_some((name, Final::Dir(children)))
                    }
                })
                .collect()
        }
        match self {
            Final::Dir(entries) => Final::Dir(prune(entries)),
            file => file,
        }
    }
}

/// Why a symbolic link has no in-root regular file or directory to stand for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Unresolvable {
    pub path: String,
    pub reason: &'static str,
}

#[derive(Debug, Default)]
pub(crate) struct Tree {
    root: Dir,
}

impl Tree {
    /// The directory at `path`, if it exists and is one. A symbolic link
    /// anywhere on the way is an error, never followed.
    fn dir_mut(&mut self, path: &[String]) -> Result<Option<&mut Dir>, PathError> {
        let mut dir = &mut self.root;
        for name in path {
            match dir.get_mut(name) {
                Some(Node::Dir(child)) => dir = child,
                Some(Node::Symlink(_)) => return Err(PathError::TraversesSymlink),
                Some(Node::File(_)) | None => return Ok(None),
            }
        }
        Ok(Some(dir))
    }

    /// The directory at `path`, creating any that are missing, as an archive
    /// that omits a parent's own entry implies it.
    fn ensure_dir(&mut self, path: &[String]) -> Result<&mut Dir, PathError> {
        let mut dir = &mut self.root;
        for name in path {
            let child = dir
                .entry(name.clone())
                .or_insert_with(|| Node::Dir(Dir::new()));
            dir = match child {
                Node::Dir(child) => child,
                Node::Symlink(_) => return Err(PathError::TraversesSymlink),
                Node::File(_) => return Err(PathError::ParentNotDirectory),
            };
        }
        Ok(dir)
    }

    /// `.wh.<name>` in `parent`: remove a lower layer's `<name>`, whatever it
    /// is. Absent is not an error.
    pub(crate) fn whiteout(&mut self, parent: &[String], name: &str) -> Result<(), PathError> {
        if let Some(dir) = self.dir_mut(parent)? {
            dir.remove(name);
        }
        Ok(())
    }

    /// `.wh..wh..opq` in `dir`: remove every child a lower layer gave it.
    pub(crate) fn opaque(&mut self, dir: &[String]) -> Result<(), PathError> {
        if let Some(dir) = self.dir_mut(dir)? {
            dir.clear();
        }
        Ok(())
    }

    /// Put `node` at `path` (non-empty). A directory entry over an existing
    /// directory keeps that directory's children; anything else replaces
    /// what was there.
    pub(crate) fn insert(&mut self, path: &[String], node: Node) -> Result<(), PathError> {
        let (name, parent) = path.split_last().expect("insert names a non-root path");
        let dir = self.ensure_dir(parent)?;
        if matches!(node, Node::Dir(_)) && matches!(dir.get(name), Some(Node::Dir(_))) {
            return Ok(());
        }
        dir.insert(name.clone(), node);
        Ok(())
    }

    /// Remove whatever is at `path` (non-empty), as an entry that cannot be
    /// written replaces it: the parent must be reachable exactly as
    /// [`Tree::insert`] requires.
    pub(crate) fn remove(&mut self, path: &[String]) -> Result<(), PathError> {
        let (name, parent) = path.split_last().expect("remove names a non-root path");
        self.ensure_dir(parent)?.remove(name);
        Ok(())
    }

    /// A hard link at `path` to the regular file at `target`: a second file
    /// with the target's content and mode (they share one inode's mode).
    pub(crate) fn hardlink(&mut self, path: &[String], target: &[String]) -> Result<(), PathError> {
        let (name, parent) = target
            .split_last()
            .ok_or(PathError::HardlinkTargetNotFile)?;
        let file = match self.dir_mut(parent)?.and_then(|dir| dir.get(name)) {
            Some(Node::File(file)) => *file,
            Some(Node::Symlink(_)) | Some(Node::Dir(_)) => {
                return Err(PathError::HardlinkTargetNotFile);
            }
            None => return Err(PathError::HardlinkTargetMissing),
        };
        self.insert(path, Node::File(file))
    }

    /// The node at `path`, walking directories only.
    fn lookup(&self, path: &[String]) -> Option<&Node> {
        let mut dir = &self.root;
        let (last, parents) = path.split_last()?;
        for name in parents {
            match dir.get(name) {
                Some(Node::Dir(child)) => dir = child,
                _ => return None,
            }
        }
        dir.get(last)
    }

    fn dir_at(&self, path: &[String]) -> Option<&Dir> {
        if path.is_empty() {
            return Some(&self.root);
        }
        match self.lookup(path) {
            Some(Node::Dir(dir)) => Some(dir),
            _ => None,
        }
    }

    /// The first symbolic link in path order, as `(path, target)`.
    pub(crate) fn first_symlink(&self) -> Option<(String, String)> {
        fn find(dir: &Dir, prefix: &str) -> Option<(String, String)> {
            for (name, node) in dir {
                let path = join(prefix, name);
                match node {
                    Node::Symlink(target) => return Some((path, target.clone())),
                    Node::Dir(child) => {
                        if let Some(found) = find(child, &path) {
                            return Some(found);
                        }
                    }
                    Node::File(_) => {}
                }
            }
            None
        }
        find(&self.root, "")
    }

    /// The tree with no symbolic links, which must have none.
    pub(crate) fn into_final(self) -> Final {
        fn convert(dir: Dir) -> BTreeMap<String, Final> {
            dir.into_iter()
                .map(|(name, node)| {
                    let node = match node {
                        Node::Dir(child) => Final::Dir(convert(child)),
                        Node::File(file) => Final::File(file),
                        Node::Symlink(_) => unreachable!("caller refused symbolic links"),
                    };
                    (name, node)
                })
                .collect()
        }
        Final::Dir(convert(self.root))
    }

    /// The tree with every symbolic link replaced by a copy of what it
    /// resolves to inside the image: a regular file, or a directory (copied
    /// recursively, its own links resolved from its real location).
    ///
    /// Resolution follows the guest's path semantics with one deliberate
    /// difference: a `..` that would climb above the image root is refused
    /// instead of clamped at `/`, including in the part of a target after a
    /// missing component or a kernel mount point. Two kinds of link are
    /// dropped, never written, and reported: one whose resolution goes
    /// beneath (or ends at) one of [`KERNEL_MOUNTED`] (the guest kernel
    /// decides what is there, not the image; a `..` straight back out of the
    /// mount point is followed as the guest follows it), and one whose
    /// target the image does not hold. A link that reaches `/work`, where a
    /// run's workspace is mounted, loops, a target that continues through a
    /// regular file, an empty target, and links whose expansion would
    /// contain themselves are refused. `max_nodes` bounds the copied tree.
    pub(crate) fn resolve_symlinks(&self, max_nodes: usize) -> Result<Resolved, ResolveError> {
        let mut state = Expansion {
            tree: self,
            active: vec![Vec::new()],
            nodes: 0,
            max_nodes,
            copied: 0,
            kernel_mount: Vec::new(),
            dangling: Vec::new(),
        };
        let root = state.expand(&[])?;
        Ok(Resolved {
            tree: Final::Dir(root),
            copied: state.copied,
            kernel_mount: state.kernel_mount,
            dangling: state.dangling,
        })
    }

    /// What `target` names, read from the directory `parent`: the real path
    /// of a node in the image, or why it names none the image supplies.
    fn resolve(&self, parent: &[String], target: &str) -> Result<Resolution, &'static str> {
        if target.is_empty() {
            return Err("the link's target is empty");
        }
        let mut stack: Vec<String> = if target.starts_with('/') {
            Vec::new()
        } else {
            parent.to_vec()
        };
        let mut pending: VecDeque<String> = components(target);
        let mut hops = 0;
        while let Some(component) = pending.pop_front() {
            match component.as_str() {
                "." => {}
                ".." => {
                    if stack.pop().is_none() {
                        return Err(CLIMBS_ABOVE_ROOT);
                    }
                }
                _ => {
                    stack.push(component);
                    // A run's workspace is mounted here when one is granted:
                    // neither the image nor its absence decides what a link
                    // into it reaches.
                    if stack[0] == workspace_mount_point() {
                        return Err(INTO_WORKSPACE);
                    }
                    if KERNEL_MOUNTED.contains(&stack[0].as_str()) {
                        // The mount point itself is the kernel's directory,
                        // whatever the image holds there; `..` from it is
                        // the root again, so keep walking.
                        if stack.len() == 1 {
                            continue;
                        }
                        // Beneath it, the guest's kernel decides what the
                        // rest of the walk reaches (its own links included:
                        // `/proc/self/root`), never the image. What remains
                        // of the target must still stay inside the root.
                        stay_in_root(stack.len(), &pending)?;
                        return Ok(Resolution::KernelMount);
                    }
                    match self.lookup(&stack) {
                        // The guest's walk stops here with ENOENT, whatever
                        // follows; what follows must still stay in the root.
                        None => {
                            stay_in_root(stack.len(), &pending)?;
                            return Ok(Resolution::Dangling);
                        }
                        Some(Node::Dir(_)) => {}
                        Some(Node::File(_)) => {
                            if !pending.is_empty() {
                                return Err("the target continues through a regular file");
                            }
                        }
                        Some(Node::Symlink(next)) => {
                            hops += 1;
                            if hops > MAX_SYMLINK_HOPS {
                                return Err("too many levels of symbolic links");
                            }
                            stack.pop();
                            if next.starts_with('/') {
                                stack.clear();
                            }
                            let mut spliced = components(next);
                            spliced.extend(pending);
                            pending = spliced;
                        }
                    }
                }
            }
        }
        // The walk ended at a kernel mount point.
        if stack.len() == 1 && KERNEL_MOUNTED.contains(&stack[0].as_str()) {
            return Ok(Resolution::KernelMount);
        }
        Ok(Resolution::Found(stack))
    }
}

const CLIMBS_ABOVE_ROOT: &str = "the target climbs above the image root";
const INTO_WORKSPACE: &str = "the target reaches /work, where the guest mounts a run's workspace, which the image cannot supply";

/// `/work` without its slash: the top-level directory the libkrun worker has
/// the guest init mount a granted workspace over
/// (`backends::libkrun::worker::WORKSPACE_DIR`; init.c `mount_virtiofs_list`).
fn workspace_mount_point() -> &'static str {
    crate::backends::libkrun::worker::WORKSPACE_DIR.trim_start_matches('/')
}

/// Whether the lexical remainder of a target, read from a directory `depth`
/// components below the root, never climbs above it.
fn stay_in_root(mut depth: usize, pending: &VecDeque<String>) -> Result<(), &'static str> {
    for component in pending {
        match component.as_str() {
            "." => {}
            ".." => depth = depth.checked_sub(1).ok_or(CLIMBS_ABOVE_ROOT)?,
            _ => depth += 1,
        }
    }
    Ok(())
}

/// What a link's target names.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Resolution {
    /// The real path of a node in the image (the root if empty).
    Found(Vec<String>),
    /// A path at or beneath `dev`, `proc` or `sys`: the guest kernel's.
    KernelMount,
    /// A path the image does not hold, inside its root.
    Dangling,
}

/// A link the expansion dropped instead of copying, as
/// `(path written, target)`.
pub(crate) type Dropped = (String, String);

/// What [`Tree::resolve_symlinks`] produced.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Resolved {
    pub tree: Final,
    /// Links replaced by copies of their targets.
    pub copied: usize,
    pub kernel_mount: Vec<Dropped>,
    pub dangling: Vec<Dropped>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ResolveError {
    Unresolvable(Unresolvable),
    TooManyNodes,
}

struct Expansion<'a> {
    tree: &'a Tree,
    /// Real paths of the directories being expanded, outermost first.
    active: Vec<Vec<String>>,
    nodes: usize,
    max_nodes: usize,
    copied: usize,
    kernel_mount: Vec<Dropped>,
    dangling: Vec<Dropped>,
}

impl Expansion<'_> {
    fn expand(&mut self, real: &[String]) -> Result<BTreeMap<String, Final>, ResolveError> {
        self.expand_at(real, "")
    }

    /// The finished entries of the directory whose real path is `real`,
    /// written at `written` (which differs from `real` inside a copied
    /// directory). A refusal names the link's real path; a drop names where
    /// it would have been written.
    fn expand_at(
        &mut self,
        real: &[String],
        written: &str,
    ) -> Result<BTreeMap<String, Final>, ResolveError> {
        let dir = self
            .tree
            .dir_at(real)
            .expect("expand is only called on a resolved directory");
        let mut out = BTreeMap::new();
        for (name, node) in dir {
            self.nodes += 1;
            if self.nodes > self.max_nodes {
                return Err(ResolveError::TooManyNodes);
            }
            let mut child = real.to_vec();
            child.push(name.clone());
            let path = join(written, name);
            let node = match node {
                Node::File(file) => Final::File(*file),
                Node::Dir(_) => Final::Dir(self.enter(child, &path)?),
                Node::Symlink(target) => {
                    let unresolvable = |reason| {
                        ResolveError::Unresolvable(Unresolvable {
                            path: child.join("/"),
                            reason,
                        })
                    };
                    let resolved = match self.tree.resolve(real, target).map_err(unresolvable)? {
                        Resolution::Found(resolved) => resolved,
                        Resolution::KernelMount => {
                            self.kernel_mount.push((path, target.clone()));
                            continue;
                        }
                        Resolution::Dangling => {
                            self.dangling.push((path, target.clone()));
                            continue;
                        }
                    };
                    self.copied += 1;
                    match self.tree.lookup(&resolved) {
                        Some(Node::File(file)) => Final::File(*file),
                        // An empty path is the image root, a directory.
                        _ if self
                            .active
                            .iter()
                            .any(|active| active.starts_with(&resolved)) =>
                        {
                            return Err(unresolvable(
                                "the link names a directory that contains it",
                            ));
                        }
                        _ => Final::Dir(self.enter(resolved, &path)?),
                    }
                }
            };
            out.insert(name.clone(), node);
        }
        Ok(out)
    }

    fn enter(
        &mut self,
        real: Vec<String>,
        written: &str,
    ) -> Result<BTreeMap<String, Final>, ResolveError> {
        self.active.push(real.clone());
        let expanded = self.expand_at(&real, written);
        self.active.pop();
        expanded
    }
}

fn components(path: &str) -> VecDeque<String> {
    path.split('/')
        .filter(|component| !component.is_empty())
        .map(str::to_string)
        .collect()
}

pub(crate) fn join(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix}/{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(value: &str) -> Vec<String> {
        components(value).into_iter().collect()
    }

    fn file(byte: u8) -> Node {
        Node::File(FileNode {
            content: blake3::hash(&[byte]),
            size: 1,
            mode: 0o644,
        })
    }

    #[test]
    fn a_directory_entry_keeps_an_existing_directory_and_replaces_anything_else() {
        let mut tree = Tree::default();
        tree.insert(&path("d/x"), file(1)).unwrap();
        tree.insert(&path("d"), Node::Dir(Dir::new())).unwrap();
        assert_eq!(tree.lookup(&path("d/x")), Some(&file(1)));
        tree.insert(&path("f"), file(2)).unwrap();
        tree.insert(&path("f"), Node::Dir(Dir::new())).unwrap();
        assert_eq!(tree.lookup(&path("f")), Some(&Node::Dir(Dir::new())));
        tree.insert(&path("d"), file(3)).unwrap();
        assert_eq!(tree.lookup(&path("d")), Some(&file(3)));
    }

    #[test]
    fn whiteouts_never_follow_a_link() {
        let mut tree = Tree::default();
        tree.insert(&path("l"), Node::Symlink("/".into())).unwrap();
        assert_eq!(
            tree.whiteout(&path("l"), "x"),
            Err(PathError::TraversesSymlink)
        );
        assert_eq!(tree.opaque(&path("l")), Err(PathError::TraversesSymlink));
        // Under a regular file there is nothing to white out.
        tree.insert(&path("f"), file(1)).unwrap();
        assert_eq!(tree.whiteout(&path("f"), "x"), Ok(()));
        assert_eq!(tree.opaque(&path("f")), Ok(()));
        assert_eq!(tree.lookup(&path("f")), Some(&file(1)));
    }

    #[test]
    fn a_hard_link_through_a_link_is_refused() {
        let mut tree = Tree::default();
        tree.insert(&path("real/f"), file(1)).unwrap();
        tree.insert(&path("l"), Node::Symlink("real".into()))
            .unwrap();
        assert_eq!(
            tree.hardlink(&path("h"), &path("l/f")),
            Err(PathError::TraversesSymlink)
        );
        assert_eq!(
            tree.hardlink(&path("h"), &path("l")),
            Err(PathError::HardlinkTargetNotFile)
        );
        assert_eq!(
            tree.hardlink(&path("h"), &[]),
            Err(PathError::HardlinkTargetNotFile)
        );
        tree.hardlink(&path("h"), &path("real/f")).unwrap();
        assert_eq!(tree.lookup(&path("h")), Some(&file(1)));
    }

    #[test]
    fn resolution_counts_hops_and_follows_intermediate_links() {
        let mut tree = Tree::default();
        tree.insert(&path("a/b/f"), file(1)).unwrap();
        tree.insert(&path("x"), Node::Symlink("a".into())).unwrap();
        tree.insert(&path("y"), Node::Symlink("/x/b".into()))
            .unwrap();
        let found = |value| Ok(Resolution::Found(path(value)));
        assert_eq!(tree.resolve(&[], "y/f"), found("a/b/f"));
        assert_eq!(tree.resolve(&path("a/b"), "../../y/./f"), found("a/b/f"));
        // Exactly MAX_SYMLINK_HOPS links resolve; one more does not.
        let mut chain = Tree::default();
        chain.insert(&path("end"), file(1)).unwrap();
        for hop in 0..MAX_SYMLINK_HOPS {
            let next = if hop == 0 {
                "end".to_string()
            } else {
                format!("l{}", hop - 1)
            };
            chain
                .insert(&[format!("l{hop}")], Node::Symlink(next))
                .unwrap();
        }
        let last = format!("l{}", MAX_SYMLINK_HOPS - 1);
        assert_eq!(chain.resolve(&[], &last), found("end"));
        chain
            .insert(&path("over"), Node::Symlink(last.clone()))
            .unwrap();
        assert_eq!(
            chain.resolve(&[], "over"),
            Err("too many levels of symbolic links")
        );
    }

    #[test]
    fn expansion_is_bounded() {
        let mut tree = Tree::default();
        tree.insert(&path("a"), file(1)).unwrap();
        tree.insert(&path("b"), file(2)).unwrap();
        assert!(tree.resolve_symlinks(2).is_ok());
        assert_eq!(tree.resolve_symlinks(1), Err(ResolveError::TooManyNodes));
    }

    #[test]
    fn only_paths_strictly_beneath_dev_proc_and_sys_are_kernel_mounted() {
        for beneath in ["dev/console", "proc/1/mounts", "sys/fs/cgroup"] {
            assert!(under_kernel_mount(&path(beneath)), "{beneath}");
        }
        for not in ["dev", "proc", "sys", "devices/x", "tmp/x", "etc/dev", ""] {
            assert!(!under_kernel_mount(&path(not)), "{not}");
        }
    }

    #[test]
    fn a_missing_or_kernel_target_is_named_unless_its_remainder_escapes() {
        let mut tree = Tree::default();
        tree.insert(&path("etc/f"), file(1)).unwrap();
        for (parent, target) in [
            ("", "missing"),
            ("", "/missing/./.."),
            ("etc", "missing/../../etc/f"),
            ("etc", "f-missing/x/../.."),
        ] {
            assert_eq!(
                tree.resolve(&path(parent), target),
                Ok(Resolution::Dangling),
                "{parent} {target}"
            );
        }
        for (parent, target) in [
            ("", "/dev"),
            ("", "proc/self/mounts"),
            ("etc", "../sys/./x/.."),
            // Beneath the mount the kernel's own links decide (`self` is
            // one), so the image never answers, even where the guest's
            // walk would come back out.
            ("etc", "../proc/self/../.."),
            ("", "proc/self/root/etc/f"),
            ("", "/proc/."),
        ] {
            assert_eq!(
                tree.resolve(&path(parent), target),
                Ok(Resolution::KernelMount),
                "{parent} {target}"
            );
        }
        for (parent, target) in [
            ("", "missing/../.."),
            ("etc", "missing/../../.."),
            ("", "/proc/../.."),
            ("", "dev/./../.."),
        ] {
            assert_eq!(
                tree.resolve(&path(parent), target),
                Err(CLIMBS_ABOVE_ROOT),
                "{parent} {target}"
            );
        }
        // `..` straight out of a mount point is the root again, as in the
        // guest: the image answers.
        for (parent, target) in [("etc", "/dev/../etc/f"), ("", "sys/./../etc/f")] {
            assert_eq!(
                tree.resolve(&path(parent), target),
                Ok(Resolution::Found(path("etc/f"))),
                "{parent} {target}"
            );
        }
        // The workspace mount point is refused, present in the image or not.
        tree.insert(&path("work/f"), file(2)).unwrap();
        for target in ["/work", "/work/f", "work/missing", "/work/.."] {
            assert_eq!(tree.resolve(&[], target), Err(INTO_WORKSPACE), "{target}");
        }
        assert_eq!(workspace_mount_point(), "work");
        // A regular file in the way is not "missing": refused as before.
        assert_eq!(
            tree.resolve(&[], "etc/f/x"),
            Err("the target continues through a regular file")
        );
    }

    #[test]
    fn remove_takes_out_what_is_there_and_never_follows_a_link() {
        let mut tree = Tree::default();
        tree.insert(&path("d/x"), file(1)).unwrap();
        tree.remove(&path("d/x")).unwrap();
        assert_eq!(tree.lookup(&path("d/x")), None);
        // Absent is not an error; the parents are created as insert would.
        tree.remove(&path("e/y")).unwrap();
        assert_eq!(tree.lookup(&path("e")), Some(&Node::Dir(Dir::new())));
        tree.insert(&path("l"), Node::Symlink("d".into())).unwrap();
        assert_eq!(tree.remove(&path("l/x")), Err(PathError::TraversesSymlink));
        tree.insert(&path("f"), file(2)).unwrap();
        assert_eq!(
            tree.remove(&path("f/x")),
            Err(PathError::ParentNotDirectory)
        );
    }
}
