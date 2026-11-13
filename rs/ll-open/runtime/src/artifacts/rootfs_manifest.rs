//! The rootfs manifest: what a rootfs digest names (beads
//! `ley-line-open-4f08ea` and `ley-line-open-faba8f`).
//!
//! A rootfs is a directory beneath a CAS root holding
//! `rootfs.manifest.json` and the tree it describes. The rootfs digest is
//! BLAKE3-256 over the manifest's exact bytes (σ_bytes, ADR-0047). Two
//! manifest versions are read; a reader that does not know a version
//! refuses it.
//!
//! **Version 2** authenticates the whole tree: every directory with its
//! mode, every regular file with its mode and content, and every symbolic
//! link with its target bytes.
//!
//! ```text
//! {"version":2,"entries":[
//!   {"type":"dir","path":"etc","mode":493},
//!   {"type":"file","path":"etc/hostname","mode":420,"blake3":"<64 hex>"},
//!   {"type":"symlink","path":"etc/mtab","target":"/proc/mounts"}]}
//! ```
//!
//! (shown wrapped; the bytes hold no whitespace). Its bytes are canonical,
//! and a v2 manifest in any other byte form is refused, so one tree has one
//! digest:
//!
//! - one compact JSON object, keys in the order `version`, `entries`; each
//!   entry's keys in the order shown; strings escaped one way: `\"` and
//!   `\\`, the short forms `\b \f \n \r \t`, any other character below
//!   U+0020 as `\u00xx` in lowercase hex, and everything else raw (U+007F,
//!   `/` and non-ASCII UTF-8 included), as `serde_json` writes them (the
//!   parsed document, written again, must be the bytes read; the shared
//!   vector's `escapes-v2` case pins it);
//! - `path` is relative, `/`-separated UTF-8, without empty, `.` or `..`
//!   components or NUL, and at the root never `rootfs.manifest.json` or
//!   [`RUN_EXEC_SPEC`];
//! - only a `dir` may be at the top-level `dev`, `proc`, `sys`, `tmp` or
//!   `work`: the guest init mounts over each, and `mount(2)` follows a link;
//! - entries are in component-wise byte order of their paths, so a
//!   directory precedes what it holds, and no path appears twice;
//! - every entry's parent is a `dir` entry (the root, which has no entry,
//!   is a directory of mode `0755`): no directory is implied;
//! - a `dir` mode is permission bits plus, at most, the sticky bit, with
//!   owner read, write and search (`0700`) set: the unprivileged host user
//!   writes each run's copy and removes it, and the guest's root ignores
//!   owner bits; a `file` mode is permission bits with owner read (`0400`)
//!   set; `blake3` is 64 lowercase hex digits;
//! - a `symlink` target is 1 to 4096 bytes without NUL. It is opaque: never
//!   followed on the host, compared byte for byte, recreated verbatim.
//!
//! **Version 1** (`{"version":1,"files":[{"path","mode","blake3"},…]}`)
//! lists regular files only. It stays readable and keeps its rules: its
//! bytes are not required to be canonical (hand-built producers wrote other
//! key orders, and tightening it would orphan every such CAS entry), its
//! directories are the listed files' ancestors at `0755`, and it can
//! express no link. One narrowing from the reader it shipped with: each
//! path must be relative and `/`-separated without empty, `.` or `..`
//! components (the old reader normalized `a//b`, `a/./b` and a trailing
//! `/`, which no LLO producer wrote) and not [`RUN_EXEC_SPEC`]. New
//! producers write version 2.

use std::collections::{BTreeMap, VecDeque};

use serde::{Deserialize, Serialize};

use super::rootfs_tree::KERNEL_MOUNTED;

/// The manifest's file name, at the rootfs root.
pub const ROOTFS_MANIFEST: &str = "rootfs.manifest.json";

/// The one path the runtime itself writes into each run's rootfs copy: the
/// libkrun worker creates it (`O_EXCL | O_NOFOLLOW`) after the copy is
/// verified and before the VM starts, and the guest init unlinks it once
/// read. No image and no manifest may name it, so a manifest that lists it
/// at the root, as any kind of entry, is refused, and so is an import that
/// would write it. The worker's exclusive create is the backstop.
pub const RUN_EXEC_SPEC: &str = ".leyline-exec.json";

/// The mode a directory the manifest declares at `declared` has in the
/// shared CAS entry: group and other write and the sticky bit cleared, so
/// no other local user can plant an entry in a CAS directory (and make it
/// fail verification forever) whatever the image declared (bead
/// `ley-line-open-10efee`). Each run's copy, which only its run reaches,
/// has the declared mode.
pub fn cas_directory_mode(declared: u32) -> u32 {
    declared & 0o755
}

/// The mode of a rootfs's root directory: it has no manifest entry.
pub const ROOT_DIRECTORY_MODE: u32 = 0o755;

/// The mode of `rootfs.manifest.json` itself. Its bytes are the digest; its
/// inode mode is not, so it is fixed.
pub const MANIFEST_FILE_MODE: u32 = 0o644;

/// The mode of every directory a version 1 manifest implies.
pub const V1_DIRECTORY_MODE: u32 = 0o755;

/// Linux's `PATH_MAX`: the longest link target a manifest may hold.
pub const MAX_LINK_TARGET_BYTES: usize = 4096;

/// Linux's `MAXSYMLINKS`: how many links one resolution may traverse.
const MAX_SYMLINK_HOPS: usize = 40;

/// A regular file's permission bits and content digest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSpec {
    pub mode: u32,
    /// Lowercase hex BLAKE3-256 of the file's bytes.
    pub blake3: String,
}

/// The tree a manifest names. Paths are relative and `/`-separated; the
/// root directory is the empty path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootfsSpec {
    /// The manifest version the tree was read from (or is written as).
    pub version: u32,
    /// Every directory with its mode, the root included. For version 1,
    /// the listed files' ancestors at [`V1_DIRECTORY_MODE`].
    pub directories: BTreeMap<String, u32>,
    pub files: BTreeMap<String, FileSpec>,
    /// Every symbolic link with its target. Empty for version 1.
    pub symlinks: BTreeMap<String, String>,
}

#[derive(Deserialize)]
struct VersionProbe {
    version: u32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestV1 {
    #[allow(dead_code)]
    version: u32,
    files: Vec<FileV1>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileV1 {
    path: String,
    mode: u32,
    blake3: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestV2 {
    version: u32,
    entries: Vec<EntryV2>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
enum EntryV2 {
    Dir {
        path: String,
        mode: u32,
    },
    File {
        path: String,
        mode: u32,
        blake3: String,
    },
    Symlink {
        path: String,
        target: String,
    },
}

impl EntryV2 {
    fn path(&self) -> &str {
        match self {
            Self::Dir { path, .. } | Self::File { path, .. } | Self::Symlink { path, .. } => path,
        }
    }
}

/// `path`'s parent directory: the empty path for a top-level name.
pub fn parent(path: &str) -> &str {
    path.rsplit_once('/').map_or("", |(parent, _)| parent)
}

/// Whether `path` is a relative, `/`-separated path the guest can name:
/// no empty, `.` or `..` component, and no NUL.
fn guest_relative(path: &str) -> bool {
    !path.is_empty()
        && !path.contains('\0')
        && path
            .split('/')
            .all(|component| !matches!(component, "" | "." | ".."))
}

fn check_path(path: &str) -> Result<(), String> {
    if !guest_relative(path) {
        return Err(format!(
            "rootfs manifest path is not guest-relative: {path}"
        ));
    }
    if path == RUN_EXEC_SPEC {
        return Err(format!(
            "rootfs manifest lists /{path}, which is reserved: the runtime writes it into each run's rootfs"
        ));
    }
    Ok(())
}

fn lowercase_hex_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

impl RootfsSpec {
    /// The tree the manifest `bytes` names, or why they are no manifest
    /// this reader accepts. The caller has already checked `bytes` against
    /// the rootfs digest.
    pub fn parse(bytes: &[u8]) -> Result<Self, String> {
        let probe: VersionProbe = serde_json::from_slice(bytes)
            .map_err(|error| format!("invalid rootfs manifest: {error}"))?;
        match probe.version {
            1 => Self::parse_v1(bytes),
            2 => Self::parse_v2(bytes),
            other => Err(format!("unsupported rootfs manifest version: {other}")),
        }
    }

    fn parse_v1(bytes: &[u8]) -> Result<Self, String> {
        let manifest: ManifestV1 = serde_json::from_slice(bytes)
            .map_err(|error| format!("invalid rootfs manifest: {error}"))?;
        let mut spec = Self {
            version: 1,
            directories: BTreeMap::from([(String::new(), V1_DIRECTORY_MODE)]),
            files: BTreeMap::new(),
            symlinks: BTreeMap::new(),
        };
        for file in manifest.files {
            check_path(&file.path)?;
            let mut ancestor = parent(&file.path);
            while !ancestor.is_empty() {
                spec.directories
                    .insert(ancestor.to_string(), V1_DIRECTORY_MODE);
                ancestor = parent(ancestor);
            }
            let previous = spec.files.insert(
                file.path.clone(),
                FileSpec {
                    mode: file.mode,
                    blake3: file.blake3,
                },
            );
            if previous.is_some() {
                return Err(format!("duplicate rootfs manifest path: {}", file.path));
            }
        }
        Ok(spec)
    }

    fn parse_v2(bytes: &[u8]) -> Result<Self, String> {
        let manifest: ManifestV2 = serde_json::from_slice(bytes)
            .map_err(|error| format!("invalid rootfs manifest: {error}"))?;
        // One tree, one digest: the only bytes accepted are the ones this
        // document serializes to.
        let canonical = serde_json::to_vec(&manifest).map_err(|error| error.to_string())?;
        if canonical != bytes {
            return Err(
                "rootfs manifest v2 is not in canonical form (compact JSON, fixed key order): its bytes are not the ones its content serializes to"
                    .to_string(),
            );
        }
        let mut spec = Self::empty_v2();
        let mut previous: Option<&str> = None;
        for entry in &manifest.entries {
            let path = entry.path();
            check_path(path)?;
            if path == ROOTFS_MANIFEST {
                return Err(format!(
                    "rootfs manifest lists its own name as an entry: {path}"
                ));
            }
            if previous.is_some_and(|previous| previous.split('/').cmp(path.split('/')).is_ge()) {
                return Err(format!(
                    "rootfs manifest v2 entries are not in strict path order at: {path}"
                ));
            }
            previous = Some(path);
            if !spec.directories.contains_key(parent(path)) {
                return Err(format!(
                    "rootfs manifest v2 entry's parent is not a listed directory: {path}"
                ));
            }
            match entry {
                EntryV2::Dir { mode, .. } => {
                    if mode & !0o1777 != 0 || mode & 0o700 != 0o700 {
                        return Err(format!(
                            "rootfs manifest v2 directory mode {mode:o} is not permission bits (and sticky) with owner read, write and search: {path}"
                        ));
                    }
                    spec.directories.insert(path.to_string(), *mode);
                }
                EntryV2::File { .. } | EntryV2::Symlink { .. } if guest_mount_point(path) => {
                    return Err(mount_point_refusal(path));
                }
                EntryV2::File { mode, blake3, .. } => {
                    if mode & !0o777 != 0 || mode & 0o400 == 0 {
                        return Err(format!(
                            "rootfs manifest v2 file mode {mode:o} is not permission bits with owner read: {path}"
                        ));
                    }
                    if !lowercase_hex_digest(blake3) {
                        return Err(format!(
                            "rootfs manifest v2 content digest is not 64 lowercase hex digits: {path}"
                        ));
                    }
                    spec.files.insert(
                        path.to_string(),
                        FileSpec {
                            mode: *mode,
                            blake3: blake3.clone(),
                        },
                    );
                }
                EntryV2::Symlink { target, .. } => {
                    if target.is_empty()
                        || target.len() > MAX_LINK_TARGET_BYTES
                        || target.contains('\0')
                    {
                        return Err(format!(
                            "rootfs manifest v2 link target is not 1 to {MAX_LINK_TARGET_BYTES} bytes without NUL: {path}"
                        ));
                    }
                    spec.symlinks.insert(path.to_string(), target.clone());
                }
            }
        }
        Ok(spec)
    }

    /// A version 2 tree holding only its root directory.
    pub fn empty_v2() -> Self {
        Self {
            version: 2,
            directories: BTreeMap::from([(String::new(), ROOT_DIRECTORY_MODE)]),
            files: BTreeMap::new(),
            symlinks: BTreeMap::new(),
        }
    }

    /// The canonical version 2 manifest bytes of this tree: what a producer
    /// writes, and the only bytes [`RootfsSpec::parse`] accepts for it.
    pub fn to_canonical_v2(&self) -> Vec<u8> {
        let mut entries: Vec<EntryV2> = self
            .directories
            .iter()
            .filter(|(path, _)| !path.is_empty())
            .map(|(path, mode)| EntryV2::Dir {
                path: path.clone(),
                mode: *mode,
            })
            .chain(self.files.iter().map(|(path, file)| EntryV2::File {
                path: path.clone(),
                mode: file.mode,
                blake3: file.blake3.clone(),
            }))
            .chain(self.symlinks.iter().map(|(path, target)| EntryV2::Symlink {
                path: path.clone(),
                target: target.clone(),
            }))
            .collect();
        entries.sort_by(|left, right| left.path().split('/').cmp(right.path().split('/')));
        serde_json::to_vec(&ManifestV2 {
            version: 2,
            entries,
        })
        .expect("a rootfs manifest serializes")
    }

    /// The regular file the guest reaches by opening `path` (relative to
    /// the rootfs root), following this manifest's links as the guest's
    /// kernel does: an absolute target restarts at the root, and `..` at
    /// the root stays there. Nothing on the host is read. Refused: a path
    /// that names no file entry, one that leaves the manifest for a
    /// directory the guest init mounts over (`dev`, `proc`, `sys`, `tmp`)
    /// or for the workspace mount, and a walk of more than 40 links.
    pub fn resolve_file(&self, path: &str) -> Result<String, String> {
        let components = |path: &str| -> VecDeque<String> {
            path.split('/')
                .filter(|component| !component.is_empty())
                .map(str::to_string)
                .collect()
        };
        let mut stack: Vec<String> = Vec::new();
        let mut pending = components(path);
        let mut hops = 0;
        while let Some(component) = pending.pop_front() {
            match component.as_str() {
                "." => continue,
                ".." => {
                    stack.pop();
                    continue;
                }
                _ => stack.push(component),
            }
            if guest_mount_point(&stack[0]) {
                return Err(format!(
                    "{path} reaches /{}, which the guest mounts over: the rootfs does not decide what is there",
                    stack[0]
                ));
            }
            let here = stack.join("/");
            if let Some(target) = self.symlinks.get(&here) {
                hops += 1;
                if hops > MAX_SYMLINK_HOPS {
                    return Err(format!("{path}: too many levels of symbolic links"));
                }
                stack.pop();
                if target.starts_with('/') {
                    stack.clear();
                }
                let mut spliced = components(target);
                spliced.extend(pending);
                pending = spliced;
            } else if self.files.contains_key(&here) {
                if !pending.is_empty() {
                    return Err(format!("{path} continues through the regular file {here}"));
                }
                return Ok(here);
            } else if !self.directories.contains_key(&here) {
                return Err(format!(
                    "{path} names nothing in the rootfs manifest ({here})"
                ));
            }
        }
        Err(format!("{path} names a directory, not a regular file"))
    }
}

/// `/work` without its slash: where the guest init mounts a run's
/// workspace.
fn workspace_mount_point() -> &'static str {
    crate::backends::libkrun::worker::WORKSPACE_DIR.trim_start_matches('/')
}

/// Whether the top-level `name` is one the guest init mounts a filesystem
/// over: [`KERNEL_MOUNTED`] and the workspace's `work`. `mount(2)` follows a
/// link at its target and cannot mount over a file, so the rootfs may hold
/// only a directory there (bead `ley-line-open-10b516`).
pub(crate) fn guest_mount_point(name: &str) -> bool {
    KERNEL_MOUNTED.contains(&name) || name == workspace_mount_point()
}

fn mount_point_refusal(path: &str) -> String {
    format!(
        "rootfs manifest v2 holds a non-directory at /{path}, which the guest mounts over: only a directory may be there"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEX: &str = "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262";

    fn sample() -> RootfsSpec {
        let mut spec = RootfsSpec::empty_v2();
        for (path, mode) in [
            ("bin", 0o755),
            ("etc", 0o755),
            ("var", 0o755),
            ("var/tmp", 0o1777),
        ] {
            spec.directories.insert(path.into(), mode);
        }
        for (path, mode) in [("bin/busybox", 0o755), ("etc/hostname", 0o644)] {
            spec.files.insert(
                path.into(),
                FileSpec {
                    mode,
                    blake3: HEX.into(),
                },
            );
        }
        for (path, target) in [
            ("bin/sh", "busybox"),
            ("etc/mtab", "/proc/mounts"),
            ("sbin", "bin"),
        ] {
            spec.symlinks.insert(path.into(), target.into());
        }
        spec
    }

    fn refusal(manifest: &str) -> String {
        RootfsSpec::parse(manifest.as_bytes()).expect_err(manifest)
    }

    #[test]
    fn the_canonical_v2_bytes_are_pinned_and_round_trip() {
        let bytes = sample().to_canonical_v2();
        assert_eq!(
            String::from_utf8(bytes.clone()).unwrap(),
            format!(
                "{{\"version\":2,\"entries\":[\
                 {{\"type\":\"dir\",\"path\":\"bin\",\"mode\":493}},\
                 {{\"type\":\"file\",\"path\":\"bin/busybox\",\"mode\":493,\"blake3\":\"{HEX}\"}},\
                 {{\"type\":\"symlink\",\"path\":\"bin/sh\",\"target\":\"busybox\"}},\
                 {{\"type\":\"dir\",\"path\":\"etc\",\"mode\":493}},\
                 {{\"type\":\"file\",\"path\":\"etc/hostname\",\"mode\":420,\"blake3\":\"{HEX}\"}},\
                 {{\"type\":\"symlink\",\"path\":\"etc/mtab\",\"target\":\"/proc/mounts\"}},\
                 {{\"type\":\"symlink\",\"path\":\"sbin\",\"target\":\"bin\"}},\
                 {{\"type\":\"dir\",\"path\":\"var\",\"mode\":493}},\
                 {{\"type\":\"dir\",\"path\":\"var/tmp\",\"mode\":1023}}]}}"
            )
        );
        assert_eq!(RootfsSpec::parse(&bytes).unwrap(), sample());
        assert_eq!(
            RootfsSpec::empty_v2().to_canonical_v2(),
            b"{\"version\":2,\"entries\":[]}"
        );
        assert_eq!(
            RootfsSpec::parse(b"{\"version\":2,\"entries\":[]}").unwrap(),
            RootfsSpec::empty_v2()
        );
    }

    /// Component-wise order, not byte order of the whole path: `a/b` sorts
    /// before `a-b` although `-` (0x2d) is less than `/` (0x2f).
    #[test]
    fn entries_are_in_component_wise_order() {
        let mut spec = RootfsSpec::empty_v2();
        spec.directories.insert("a".into(), 0o755);
        for path in ["a/b", "a-b"] {
            spec.files.insert(
                path.into(),
                FileSpec {
                    mode: 0o644,
                    blake3: HEX.into(),
                },
            );
        }
        let text = String::from_utf8(spec.to_canonical_v2()).unwrap();
        let (dir, nested, sibling) = (
            text.find("\"path\":\"a\"").unwrap(),
            text.find("\"path\":\"a/b\"").unwrap(),
            text.find("\"path\":\"a-b\"").unwrap(),
        );
        assert!(dir < nested && nested < sibling, "{text}");
        assert_eq!(RootfsSpec::parse(text.as_bytes()).unwrap(), spec);
        // The byte-order arrangement of the same entries is refused.
        let swapped = format!(
            "{{\"version\":2,\"entries\":[{{\"type\":\"file\",\"path\":\"a-b\",\"mode\":420,\"blake3\":\"{HEX}\"}},{{\"type\":\"dir\",\"path\":\"a\",\"mode\":493}}]}}"
        );
        assert!(refusal(&swapped).contains("strict path order"));
    }

    /// Bead ley-line-open-faba8f: a v2 manifest whose bytes are not the
    /// canonical ones names the same tree under another digest, so it is
    /// refused, whatever the difference.
    #[test]
    fn a_v2_manifest_in_any_other_byte_form_is_refused() {
        let canonical =
            "{\"version\":2,\"entries\":[{\"type\":\"dir\",\"path\":\"etc\",\"mode\":493}]}";
        RootfsSpec::parse(canonical.as_bytes()).expect("the canonical form");
        for other in [
            // Whitespace, inside and after.
            "{\"version\":2, \"entries\":[{\"type\":\"dir\",\"path\":\"etc\",\"mode\":493}]}",
            "{\"version\":2,\"entries\":[{\"type\":\"dir\",\"path\":\"etc\",\"mode\":493}]}\n",
            // Key order, of the document and of an entry.
            "{\"entries\":[{\"type\":\"dir\",\"path\":\"etc\",\"mode\":493}],\"version\":2}",
            "{\"version\":2,\"entries\":[{\"path\":\"etc\",\"type\":\"dir\",\"mode\":493}]}",
            "{\"version\":2,\"entries\":[{\"type\":\"dir\",\"mode\":493,\"path\":\"etc\"}]}",
            // Another spelling of one string.
            "{\"version\":2,\"entries\":[{\"type\":\"dir\",\"path\":\"\\u0065tc\",\"mode\":493}]}",
        ] {
            assert!(refusal(other).contains("canonical form"), "{other}");
        }
        for invalid in [
            // Another spelling of one number, an unknown key, an unknown
            // type, a repeated key, a missing key.
            "{\"version\":2,\"entries\":[{\"type\":\"dir\",\"path\":\"etc\",\"mode\":493.0}]}",
            "{\"version\":2,\"entries\":[{\"type\":\"dir\",\"path\":\"etc\",\"mode\":493,\"uid\":0}]}",
            "{\"version\":2,\"entries\":[{\"type\":\"fifo\",\"path\":\"etc\"}]}",
            "{\"version\":2,\"entries\":[],\"entries\":[]}",
            "{\"version\":2,\"entries\":[],\"files\":[]}",
            "{\"version\":2,\"entries\":[{\"type\":\"dir\",\"path\":\"etc\"}]}",
            "{\"version\":2}",
        ] {
            assert!(
                refusal(invalid).contains("invalid rootfs manifest"),
                "{invalid}"
            );
        }
    }

    #[test]
    fn an_unknown_version_is_refused_by_number() {
        for (manifest, version) in [
            ("{\"version\":3,\"entries\":[]}", "3"),
            ("{\"version\":0,\"files\":[]}", "0"),
            ("{\"version\":3,\"anything\":{\"at\":\"all\"}}", "3"),
        ] {
            assert_eq!(
                refusal(manifest),
                format!("unsupported rootfs manifest version: {version}")
            );
        }
        assert!(refusal("{\"files\":[]}").contains("invalid rootfs manifest"));
        assert!(refusal("[]").contains("invalid rootfs manifest"));
        assert!(refusal("{\"version\":\"2\",\"entries\":[]}").contains("invalid rootfs manifest"));
    }

    fn v2(entries: &str) -> String {
        format!("{{\"version\":2,\"entries\":[{entries}]}}")
    }

    #[test]
    fn v2_entries_are_validated() {
        let file = |path: &str, mode: u32, blake3: &str| {
            format!(
                "{{\"type\":\"file\",\"path\":\"{path}\",\"mode\":{mode},\"blake3\":\"{blake3}\"}}"
            )
        };
        let dir = |path: &str, mode: u32| {
            format!("{{\"type\":\"dir\",\"path\":\"{path}\",\"mode\":{mode}}}")
        };
        let link = |path: &str, target: &str| {
            format!("{{\"type\":\"symlink\",\"path\":\"{path}\",\"target\":\"{target}\"}}")
        };
        let long = "t".repeat(MAX_LINK_TARGET_BYTES + 1);
        let longest = "t".repeat(MAX_LINK_TARGET_BYTES);
        let upper = HEX.to_uppercase();
        let cases: Vec<(String, &str)> = vec![
            // Paths.
            (dir("", 0o755), "not guest-relative"),
            (dir("/etc", 0o755), "not guest-relative"),
            (dir("etc/", 0o755), "not guest-relative"),
            (dir("a//b", 0o755), "not guest-relative"),
            (dir(".", 0o755), "not guest-relative"),
            (dir("..", 0o755), "not guest-relative"),
            (
                format!("{},{}", dir("a", 0o755), dir("a/..", 0o755)),
                "not guest-relative",
            ),
            (dir("a\\u0000b", 0o755), "not guest-relative"),
            (file(ROOTFS_MANIFEST, 0o644, HEX), "its own name"),
            // Order and duplicates.
            (
                format!("{},{}", dir("b", 0o755), dir("a", 0o755)),
                "strict path order",
            ),
            (
                format!("{},{}", dir("a", 0o755), dir("a", 0o755)),
                "strict path order",
            ),
            (
                format!("{},{}", dir("a", 0o755), link("a", "x")),
                "strict path order",
            ),
            // Parents are listed directories.
            (file("a/b", 0o644, HEX), "parent is not a listed directory"),
            (
                format!("{},{}", file("a", 0o644, HEX), file("a/b", 0o644, HEX)),
                "parent is not a listed directory",
            ),
            (
                format!("{},{}", link("a", "x"), file("a/b", 0o644, HEX)),
                "parent is not a listed directory",
            ),
            // Directory modes.
            (dir("a", 0o2755), "directory mode"),
            (dir("a", 0o4755), "directory mode"),
            (dir("a", 0o10755), "directory mode"),
            (dir("a", 0o355), "directory mode"),
            (dir("a", 0o655), "directory mode"),
            (dir("a", 0o555), "directory mode"),
            (dir("a", 0o500), "directory mode"),
            // File modes and digests.
            (file("a", 0o1644, HEX), "file mode"),
            (file("a", 0o4755, HEX), "file mode"),
            (file("a", 0o244, HEX), "file mode"),
            (file("a", 0o644, &HEX[1..]), "content digest"),
            (file("a", 0o644, &upper), "content digest"),
            (
                file("a", 0o644, &format!("{}g", &HEX[1..])),
                "content digest",
            ),
            // Link targets.
            (link("a", ""), "link target"),
            (link("a", &long), "link target"),
            (link("a", "x\\u0000y"), "link target"),
        ];
        for (entries, reason) in cases {
            let error = refusal(&v2(&entries));
            assert!(error.contains(reason), "{entries}: {error}");
        }
        // The boundaries that are accepted.
        for entries in [
            dir("a", 0o700),
            dir("a", 0o1777),
            file("a", 0o400, HEX),
            file("a", 0o777, HEX),
            link("a", &longest),
            link("a", "../../../outside"),
            link("a", "/"),
        ] {
            RootfsSpec::parse(v2(&entries).as_bytes())
                .unwrap_or_else(|error| panic!("{entries}: {error}"));
        }
    }

    /// The v2 string escaping, as a second implementation must write it
    /// (identity review of 4f08ea, F2): `"` and `\` escaped, the short
    /// forms `\b \f \n \r \t`, any other control character below U+0020 as
    /// `\u00xx` in lowercase hex, and everything else raw: U+007F, `/` and
    /// non-ASCII UTF-8 included. Any other spelling of the same strings is
    /// refused. The same bytes are the shared vector's escapes case.
    #[test]
    fn v2_strings_are_escaped_one_way() {
        let mut spec = RootfsSpec::empty_v2();
        spec.directories.insert("\u{e9}".into(), 0o755);
        spec.files.insert(
            "\u{e9}/q\"b\\s\tt".into(),
            FileSpec {
                mode: 0o644,
                blake3: HEX.into(),
            },
        );
        spec.symlinks.insert(
            "\u{e9}/l".into(),
            "\u{1}x\u{7f}/\u{e9}\u{8}\u{c}\n\r".into(),
        );
        let expected = format!(
            "{{\"version\":2,\"entries\":[\
             {{\"type\":\"dir\",\"path\":\"\u{e9}\",\"mode\":493}},\
             {{\"type\":\"symlink\",\"path\":\"\u{e9}/l\",\"target\":\"\\u0001x\u{7f}/\u{e9}\\b\\f\\n\\r\"}},\
             {{\"type\":\"file\",\"path\":\"\u{e9}/q\\\"b\\\\s\\tt\",\"mode\":420,\"blake3\":\"{HEX}\"}}]}}"
        );
        assert_eq!(String::from_utf8(spec.to_canonical_v2()).unwrap(), expected);
        assert_eq!(RootfsSpec::parse(expected.as_bytes()).unwrap(), spec);
        for (raw, other) in [
            ("\u{e9}/l", "\\u00e9/l"),
            ("\\t", "\\u0009"),
            ("\\b", "\\u0008"),
            ("\u{7f}", "\\u007f"),
            ("/\u{e9}\\b", "\\/\u{e9}\\b"),
        ] {
            let respelled = expected.replacen(raw, other, 1);
            assert_ne!(respelled, expected, "{raw} -> {other}");
            assert!(
                refusal(&respelled).contains("canonical"),
                "{raw} -> {other}: {respelled}"
            );
        }
    }

    /// The guest init mounts over `dev`, `proc`, `sys`, `tmp` and (with a
    /// workspace) `work`, and `mount(2)` follows a link at its target: a
    /// link there would move the mount over the tree the digest names (a
    /// `work -> usr/bin` puts the workspace over the executable), and a file
    /// there never mounts. So a v2 manifest holds only a directory at each,
    /// whoever wrote it; below one, and elsewhere, the names are ordinary.
    #[test]
    fn a_mount_point_is_a_directory_or_absent() {
        for name in ["dev", "proc", "sys", "tmp", "work"] {
            for entry in [
                format!("{{\"type\":\"symlink\",\"path\":\"{name}\",\"target\":\"usr/bin\"}}"),
                format!(
                    "{{\"type\":\"file\",\"path\":\"{name}\",\"mode\":420,\"blake3\":\"{HEX}\"}}"
                ),
            ] {
                let error = refusal(&v2(&entry));
                assert!(
                    error.contains("mounts over") && error.contains(&format!("/{name}")),
                    "{entry}: {error}"
                );
            }
            RootfsSpec::parse(
                v2(&format!(
                    "{{\"type\":\"dir\",\"path\":\"{name}\",\"mode\":493}},{{\"type\":\"symlink\",\"path\":\"{name}/x\",\"target\":\"/\"}}"
                ))
                .as_bytes(),
            )
            .unwrap_or_else(|error| panic!("{name}: a directory there: {error}"));
            RootfsSpec::parse(
                v2(&format!(
                    "{{\"type\":\"dir\",\"path\":\"etc\",\"mode\":493}},{{\"type\":\"symlink\",\"path\":\"etc/{name}\",\"target\":\"/\"}}"
                ))
                .as_bytes(),
            )
            .unwrap_or_else(|error| panic!("{name}: the name below the root: {error}"));
        }
    }

    /// The runtime's own per-run file is no image's to name: a manifest
    /// listing it at the root, as any kind of entry and under either
    /// version, is refused by name. The same name elsewhere is ordinary.
    #[test]
    fn the_run_exec_spec_path_is_reserved_at_the_root() {
        let entries = [
            format!(
                "{{\"type\":\"file\",\"path\":\"{RUN_EXEC_SPEC}\",\"mode\":420,\"blake3\":\"{HEX}\"}}"
            ),
            format!("{{\"type\":\"dir\",\"path\":\"{RUN_EXEC_SPEC}\",\"mode\":493}}"),
            format!("{{\"type\":\"symlink\",\"path\":\"{RUN_EXEC_SPEC}\",\"target\":\"etc/x\"}}"),
        ];
        let v1 = format!(
            "{{\"version\":1,\"files\":[{{\"path\":\"{RUN_EXEC_SPEC}\",\"mode\":420,\"blake3\":\"{HEX}\"}}]}}"
        );
        for manifest in entries.iter().map(|entry| v2(entry)).chain([v1]) {
            let error = refusal(&manifest);
            assert!(
                error.contains("reserved") && error.contains(RUN_EXEC_SPEC),
                "{manifest}: {error}"
            );
        }
        let nested = v2(&format!(
            "{{\"type\":\"dir\",\"path\":\"etc\",\"mode\":493}},{{\"type\":\"file\",\"path\":\"etc/{RUN_EXEC_SPEC}\",\"mode\":420,\"blake3\":\"{HEX}\"}}"
        ));
        RootfsSpec::parse(nested.as_bytes()).expect("the name below the root");
    }

    /// Version 1 is frozen as it shipped: any byte form of the document
    /// verifies (bead ley-line-open-faba8f records why), and its
    /// directories are implied.
    #[test]
    fn v1_stays_readable_in_any_byte_form() {
        let importer = format!(
            "{{\"version\":1,\"files\":[{{\"path\":\"usr/bin/probe\",\"mode\":493,\"blake3\":\"{HEX}\"}},{{\"path\":\"top\",\"mode\":420,\"blake3\":\"{HEX}\"}}]}}"
        );
        // serde_json's `json!` order (keys sorted), as the guest harness
        // writes it, and with whitespace.
        let hand_built = format!(
            "{{ \"files\": [{{\"blake3\":\"{HEX}\",\"mode\":493,\"path\":\"usr/bin/probe\"}}, {{\"blake3\":\"{HEX}\",\"mode\":420,\"path\":\"top\"}}], \"version\": 1 }}\n"
        );
        let spec = RootfsSpec::parse(importer.as_bytes()).unwrap();
        assert_eq!(RootfsSpec::parse(hand_built.as_bytes()).unwrap(), spec);
        assert_eq!(spec.version, 1);
        assert_eq!(
            spec.directories,
            BTreeMap::from([
                (String::new(), 0o755),
                ("usr".to_string(), 0o755),
                ("usr/bin".to_string(), 0o755),
            ])
        );
        assert_eq!(spec.files.len(), 2);
        assert_eq!(spec.files["usr/bin/probe"].mode, 0o755);
        assert!(spec.symlinks.is_empty());
        // Still refused, as before: a path that is not guest-relative, a
        // duplicate, an unknown key, and v2's `entries` under version 1.
        for (manifest, reason) in [
            (
                format!(
                    "{{\"version\":1,\"files\":[{{\"path\":\"../x\",\"mode\":420,\"blake3\":\"{HEX}\"}}]}}"
                ),
                "not guest-relative",
            ),
            (
                format!(
                    "{{\"version\":1,\"files\":[{{\"path\":\"/x\",\"mode\":420,\"blake3\":\"{HEX}\"}}]}}"
                ),
                "not guest-relative",
            ),
            (
                format!(
                    "{{\"version\":1,\"files\":[{{\"path\":\"x\",\"mode\":420,\"blake3\":\"{HEX}\"}},{{\"path\":\"x\",\"mode\":420,\"blake3\":\"{HEX}\"}}]}}"
                ),
                "duplicate rootfs manifest path: x",
            ),
            (
                "{\"version\":1,\"files\":[],\"entries\":[]}".to_string(),
                "invalid rootfs manifest",
            ),
            (
                "{\"version\":1,\"entries\":[]}".to_string(),
                "invalid rootfs manifest",
            ),
            // The narrowing from the reader v1 shipped with (identity
            // review of 4f08ea, F4): it normalized these; none came from an
            // LLO producer.
            (
                format!(
                    "{{\"version\":1,\"files\":[{{\"path\":\"a//b\",\"mode\":420,\"blake3\":\"{HEX}\"}}]}}"
                ),
                "not guest-relative",
            ),
            (
                format!(
                    "{{\"version\":1,\"files\":[{{\"path\":\"a/./b\",\"mode\":420,\"blake3\":\"{HEX}\"}}]}}"
                ),
                "not guest-relative",
            ),
            (
                format!(
                    "{{\"version\":1,\"files\":[{{\"path\":\"a/\",\"mode\":420,\"blake3\":\"{HEX}\"}}]}}"
                ),
                "not guest-relative",
            ),
        ] {
            let error = refusal(&manifest);
            assert!(error.contains(reason), "{manifest}: {error}");
        }
    }

    #[test]
    fn parents_are_the_path_before_the_last_component() {
        assert_eq!(parent("a/b/c"), "a/b");
        assert_eq!(parent("a"), "");
        assert_eq!(parent(""), "");
    }

    #[test]
    fn a_file_is_resolved_through_the_manifests_links_as_the_guest_would() {
        let mut spec = sample();
        spec.directories.insert("usr".into(), 0o755);
        spec.directories.insert("usr/bin".into(), 0o755);
        for (path, target) in [
            ("usr/bin/env", "../../bin/busybox"),
            ("usr/bin/abs", "/bin/sh"),
            ("usr/bin/climb", "../../../../bin/busybox"),
            ("loop-a", "loop-b"),
            ("loop-b", "loop-a"),
            ("dangling", "nowhere"),
            ("through-file", "etc/hostname/x"),
            ("into-proc", "/proc/self/exe"),
            ("into-work", "work/tool"),
            ("to-dir", "bin"),
        ] {
            spec.symlinks.insert(path.into(), target.into());
        }
        for (path, real) in [
            ("bin/busybox", "bin/busybox"),
            ("/bin/busybox", "bin/busybox"),
            ("bin/./busybox", "bin/busybox"),
            ("bin/sh", "bin/busybox"),
            // Through a directory link, then a file link.
            ("sbin/sh", "bin/busybox"),
            ("usr/bin/env", "bin/busybox"),
            ("usr/bin/abs", "bin/busybox"),
            // `..` at the root stays at the root, as in the guest.
            ("usr/bin/climb", "bin/busybox"),
            ("../../bin/sh", "bin/busybox"),
            ("to-dir/busybox", "bin/busybox"),
        ] {
            assert_eq!(spec.resolve_file(path).as_deref(), Ok(real), "{path}");
        }
        for (path, reason) in [
            ("loop-a", "too many levels"),
            ("dangling", "names nothing"),
            ("bin/absent", "names nothing"),
            (
                "through-file",
                "continues through the regular file etc/hostname",
            ),
            ("etc/hostname/x", "continues through the regular file"),
            ("bin", "names a directory"),
            ("to-dir", "names a directory"),
            ("", "names a directory"),
            ("etc/mtab", "reaches /proc"),
            ("into-proc", "reaches /proc"),
            ("into-work", "reaches /work"),
            ("dev/null", "reaches /dev"),
            ("tmp/x", "reaches /tmp"),
            ("sys", "reaches /sys"),
        ] {
            let error = spec.resolve_file(path).expect_err(path);
            assert!(error.contains(reason), "{path}: {error}");
        }
    }

    /// Exactly 40 links resolve; the 41st is refused.
    #[test]
    fn a_resolution_may_traverse_forty_links() {
        let chain = |links: usize| {
            let mut spec = RootfsSpec::empty_v2();
            spec.files.insert(
                "end".into(),
                FileSpec {
                    mode: 0o755,
                    blake3: HEX.into(),
                },
            );
            for index in 0..links {
                let target = if index + 1 == links {
                    "end".to_string()
                } else {
                    format!("l{}", index + 1)
                };
                spec.symlinks.insert(format!("l{index}"), target);
            }
            spec.resolve_file("l0")
        };
        assert_eq!(chain(40).as_deref(), Ok("end"));
        assert!(chain(41).unwrap_err().contains("too many levels"));
    }
}
