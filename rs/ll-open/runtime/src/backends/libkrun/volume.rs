//! Per-run writable rootfs views for the embedded libkrun backend.
//!
//! The authenticated CAS tree remains immutable. A worker receives an empty,
//! parent-owned directory, copies the manifest's tree into it, then verifies
//! the copy, its directories and links included, against the same manifest
//! before giving libkrun access to it. The copy is the manifest's tree, not
//! the CAS entry's:
//!
//! - **Manifest v2** names every directory with its mode, every file and
//!   every symbolic link: the copy holds exactly those, each directory at
//!   its declared mode, each link recreated from the manifest's target bytes
//!   (bead `ley-line-open-4f08ea`). A link is never followed on the host.
//! - **Manifest v1** names files only: the copy holds the listed files and
//!   the directories they imply, every directory at 0755, nothing else
//!   (bead `ley-line-open-38ff59`).

use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, fstat, open, openat, readlinkat, statat};

use crate::ExecutionError;
use crate::artifacts::rootfs_manifest::{MANIFEST_FILE_MODE as MANIFEST_MODE, RootfsSpec};

use super::plan::{ROOTFS_MANIFEST, ResolvedRootfs, read_manifest, verify_manifest};

/// Materialize a private writable view of an authenticated rootfs.
///
/// `destination` must already exist and be empty. Its lifecycle belongs to
/// the caller so cleanup does not depend on a worker after nono has removed
/// ambient filesystem authority.
pub fn materialize_ephemeral_rootfs(
    source: &ResolvedRootfs,
    destination: &Path,
) -> Result<ResolvedRootfs, ExecutionError> {
    let source_path = source
        .canonical_path
        .canonicalize()
        .map_err(|error| invalid_io("canonicalize immutable rootfs", error))?;
    let destination = destination
        .canonicalize()
        .map_err(|error| invalid_io("canonicalize ephemeral rootfs destination", error))?;
    if !source_path.is_dir() || !destination.is_dir() {
        return Err(ExecutionError::invalid(
            "rootfs source and ephemeral destination must be directories",
        ));
    }
    if destination.starts_with(&source_path) || source_path.starts_with(&destination) {
        return Err(ExecutionError::invalid(
            "ephemeral rootfs destination must not overlap the immutable rootfs",
        ));
    }
    if fs::read_dir(&destination)
        .map_err(|error| invalid_io("enumerate ephemeral rootfs destination", error))?
        .next()
        .is_some()
    {
        return Err(ExecutionError::invalid(
            "ephemeral rootfs destination must be empty",
        ));
    }

    fs::set_permissions(&destination, fs::Permissions::from_mode(0o700))
        .map_err(|error| invalid_io("make ephemeral run directory private", error))?;
    let guest_root = destination.join("rootfs");
    fs::create_dir(&guest_root)
        .map_err(|error| invalid_io("create ephemeral guest rootfs", error))?;
    fs::set_permissions(&guest_root, fs::Permissions::from_mode(0o700))
        .map_err(|error| invalid_io("make rootfs private during materialization", error))?;
    // The copy is the manifest's tree, not the CAS entry's: what the digest
    // names reaches the guest, and nothing else does.
    let plan = read_manifest(&source_path, &source.digest)?;
    copy_directory_contents(&source_path, &guest_root, &plan)?;
    let materialized = ResolvedRootfs {
        digest: source.digest.clone(),
        canonical_path: guest_root,
    };
    verify_ephemeral_rootfs(&materialized)?;
    Ok(materialized)
}

/// The mode of `rootfs.manifest.json` in a per-run view. Its bytes are the
/// digest; its inode mode in the CAS entry is not, so it is not carried.
pub const MANIFEST_FILE_MODE: u32 = MANIFEST_MODE;

/// The mode the manifest gives the directory at `relative` (the root is
/// `""`), or `None` when it names none there.
fn directory_mode(plan: &RootfsSpec, relative: &str) -> Option<u32> {
    plan.directories.get(relative).copied()
}

fn copy_directory_contents(
    source: &Path,
    destination: &Path,
    plan: &RootfsSpec,
) -> Result<(), ExecutionError> {
    let source = open(
        source,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| invalid_rustix("open immutable rootfs directory", error))?;
    copy_directory_fd(&source, "", Some(destination), plan)?;
    let mode = directory_mode(plan, "")
        .ok_or_else(|| ExecutionError::invalid("the rootfs manifest names no root directory"))?;
    fs::set_permissions(destination, fs::Permissions::from_mode(mode))
        .map_err(|error| invalid_io("set ephemeral guest rootfs permissions", error))
}

/// `name` beneath the rootfs-relative directory `relative`.
fn child_path(relative: &str, name: &str) -> String {
    if relative.is_empty() {
        name.to_string()
    } else {
        format!("{relative}/{name}")
    }
}

/// Copy the entries of `source` (at `relative` in the rootfs) that `plan`
/// names into `destination`, which is `None` for a directory the manifest
/// does not name: one is walked, so any file or link in it is refused as
/// undeclared, but never created.
fn copy_directory_fd(
    source: &impl AsFd,
    relative: &str,
    destination: Option<&Path>,
    plan: &RootfsSpec,
) -> Result<(), ExecutionError> {
    let mut entries = Dir::read_from(source)
        .map_err(|error| invalid_rustix("enumerate immutable rootfs", error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| invalid_rustix("enumerate immutable rootfs entry", error))?;
    entries.sort_by(|left, right| {
        left.file_name()
            .to_bytes()
            .cmp(right.file_name().to_bytes())
    });

    for entry in entries {
        let name = entry.file_name();
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        let before = statat(source, name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|error| invalid_rustix("inspect immutable rootfs entry", error))?;
        let expected_type = FileType::from_raw_mode(before.st_mode);
        let entry_relative = child_path(relative, &String::from_utf8_lossy(name.to_bytes()));
        if expected_type.is_symlink() {
            // A link is recreated from the manifest's bytes, never followed,
            // and only where the manifest names one with this target.
            let Some(target) = plan.symlinks.get(&entry_relative) else {
                return Err(ExecutionError::invalid(if plan.version == 1 {
                    "rootfs symbolic links are not supported by rootfs manifest v1".to_string()
                } else {
                    format!("rootfs symbolic link is not in the content-addressed manifest: {entry_relative}")
                }));
            };
            let on_disk = readlinkat(source, name, Vec::new())
                .map_err(|error| invalid_rustix("read immutable rootfs link", error))?;
            let (true, Some(destination)) = (on_disk.as_bytes() == target.as_bytes(), destination)
            else {
                return Err(ExecutionError::invalid(format!(
                    "rootfs symbolic link differs from the content-addressed manifest: {entry_relative}"
                )));
            };
            std::os::unix::fs::symlink(
                target,
                destination.join(OsStr::from_bytes(name.to_bytes())),
            )
            .map_err(|error| invalid_io("create ephemeral rootfs link", error))?;
            continue;
        }
        if !expected_type.is_dir() && !expected_type.is_file() {
            return Err(ExecutionError::invalid(
                "rootfs contains a non-regular filesystem entry",
            ));
        }

        let flags = OFlags::RDONLY
            | OFlags::NOFOLLOW
            | OFlags::CLOEXEC
            | OFlags::NONBLOCK
            | if expected_type.is_dir() {
                OFlags::DIRECTORY
            } else {
                OFlags::empty()
            };
        let opened = openat(source, name, flags, Mode::empty()).map_err(|error| {
            invalid_rustix("open immutable rootfs entry without following", error)
        })?;
        ensure_same_entry(&before, &opened)?;

        let name = OsStr::from_bytes(entry.file_name().to_bytes());
        if expected_type.is_dir() {
            let Some(mode) = directory_mode(plan, &entry_relative) else {
                // Not in the manifest: never created in the copy.
                copy_directory_fd(&opened, &entry_relative, None, plan)?;
                continue;
            };
            // Every directory the manifest names has a named parent, so its
            // parent was created.
            let Some(destination) = destination else {
                return Err(ExecutionError::invalid(format!(
                    "rootfs directory {entry_relative} is in the manifest but its parent is not"
                )));
            };
            let destination_path = destination.join(name);
            fs::create_dir(&destination_path)
                .map_err(|error| invalid_io("create ephemeral rootfs directory", error))?;
            copy_directory_fd(&opened, &entry_relative, Some(&destination_path), plan)?;
            fs::set_permissions(&destination_path, fs::Permissions::from_mode(mode))
                .map_err(|error| invalid_io("set ephemeral rootfs directory permissions", error))?;
        } else {
            let listed =
                plan.files.contains_key(&entry_relative) || entry_relative == ROOTFS_MANIFEST;
            let (true, Some(destination)) = (listed, destination) else {
                return Err(ExecutionError::invalid(format!(
                    "rootfs file is not in the content-addressed manifest: {entry_relative}"
                )));
            };
            let destination_path = destination.join(name);
            // The manifest's own mode is not in any digest: the copy gets a
            // fixed one.
            let mode = if entry_relative == ROOTFS_MANIFEST {
                MANIFEST_FILE_MODE
            } else {
                (before.st_mode as u32) & 0o7777
            };
            let permissions = fs::Permissions::from_mode(mode);
            let mut source_file = File::from(opened);
            let mut destination_file = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&destination_path)
                .map_err(|error| invalid_io("create ephemeral rootfs file", error))?;
            io::copy(&mut source_file, &mut destination_file)
                .map_err(|error| invalid_io("copy ephemeral rootfs file", error))?;
            fs::set_permissions(&destination_path, permissions)
                .map_err(|error| invalid_io("preserve ephemeral rootfs file permissions", error))?;
        }
    }
    Ok(())
}

fn ensure_same_entry(before: &rustix::fs::Stat, opened: &OwnedFd) -> Result<(), ExecutionError> {
    let after = fstat(opened)
        .map_err(|error| invalid_rustix("inspect opened immutable rootfs entry", error))?;
    if before.st_dev != after.st_dev
        || before.st_ino != after.st_ino
        || FileType::from_raw_mode(before.st_mode) != FileType::from_raw_mode(after.st_mode)
    {
        return Err(ExecutionError::invalid(
            "immutable rootfs entry changed during materialization",
        ));
    }
    Ok(())
}

/// Recheck a completed per-run view immediately before VM configuration:
/// its files and links against the manifest, and its directories, the root
/// included, exactly the manifest's (v2), or exactly the set v1 implies,
/// each at 0755. A directory planted or re-moded after materialization is
/// refused by name.
pub fn verify_ephemeral_rootfs(rootfs: &ResolvedRootfs) -> Result<(), ExecutionError> {
    let tree = verify_manifest(&rootfs.canonical_path, &rootfs.digest)?;
    let manifest_mode = fs::symlink_metadata(rootfs.canonical_path.join(ROOTFS_MANIFEST))
        .map_err(|error| invalid_io("inspect ephemeral rootfs manifest", error))?
        .permissions()
        .mode()
        & 0o7777;
    if manifest_mode != MANIFEST_FILE_MODE {
        return Err(ExecutionError::invalid(format!(
            "rootfs manifest file mode {manifest_mode:o} is not the normalized {MANIFEST_FILE_MODE:o}"
        )));
    }
    for (directory, mode) in &tree.directories {
        let Some(expected) = tree.spec.directories.get(directory) else {
            return Err(ExecutionError::invalid(format!(
                "rootfs directory is not in the content-addressed manifest: {directory}"
            )));
        };
        if mode != expected {
            return Err(ExecutionError::invalid(format!(
                "rootfs directory mode {mode:o} is not the manifest's {expected:o}: /{directory}"
            )));
        }
    }
    Ok(())
}

fn invalid_io(action: &str, error: std::io::Error) -> ExecutionError {
    ExecutionError::invalid(format!("{action}: {error}"))
}

fn invalid_rustix(action: &str, error: rustix::io::Errno) -> ExecutionError {
    ExecutionError::invalid(format!("{action}: {error}"))
}
