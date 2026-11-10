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
    /// instead of clamped at `/`. Dangling links, loops, and links whose
    /// expansion would contain themselves are refused. `max_nodes` bounds the
    /// copied tree. Returns the tree and how many links were replaced.
    pub(crate) fn resolve_symlinks(
        &self,
        max_nodes: usize,
    ) -> Result<(Final, usize), ResolveError> {
        let mut state = Expansion {
            tree: self,
            active: vec![Vec::new()],
            nodes: 0,
            max_nodes,
            resolved: 0,
        };
        let root = state.expand(&[])?;
        Ok((Final::Dir(root), state.resolved))
    }

    /// The real path `target` names, read from the directory `parent`.
    fn resolve(&self, parent: &[String], target: &str) -> Result<Vec<String>, &'static str> {
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
                        return Err("the target climbs above the image root");
                    }
                }
                _ => {
                    stack.push(component);
                    match self.lookup(&stack) {
                        None => return Err("the target does not exist in the image"),
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
        Ok(stack)
    }
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
    resolved: usize,
}

impl Expansion<'_> {
    fn expand(&mut self, real: &[String]) -> Result<BTreeMap<String, Final>, ResolveError> {
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
            let node = match node {
                Node::File(file) => Final::File(*file),
                Node::Dir(_) => Final::Dir(self.enter(child)?),
                Node::Symlink(target) => {
                    let unresolvable = |reason| {
                        ResolveError::Unresolvable(Unresolvable {
                            path: child.join("/"),
                            reason,
                        })
                    };
                    let resolved = self.tree.resolve(real, target).map_err(unresolvable)?;
                    self.resolved += 1;
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
                        _ => Final::Dir(self.enter(resolved)?),
                    }
                }
            };
            out.insert(name.clone(), node);
        }
        Ok(out)
    }

    fn enter(&mut self, real: Vec<String>) -> Result<BTreeMap<String, Final>, ResolveError> {
        self.active.push(real.clone());
        let expanded = self.expand(&real);
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
        assert_eq!(tree.resolve(&[], "y/f"), Ok(path("a/b/f")));
        assert_eq!(tree.resolve(&path("a/b"), "../../y/./f"), Ok(path("a/b/f")));
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
        assert_eq!(chain.resolve(&[], &last), Ok(path("end")));
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
}
