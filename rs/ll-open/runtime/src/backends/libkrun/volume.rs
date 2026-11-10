//! Per-run writable rootfs views for the embedded libkrun backend.
//!
//! The authenticated CAS tree remains immutable. A worker receives an empty,
//! parent-owned directory, copies the manifest's tree into it (the listed
//! files and the directories they imply, every directory at 0755: manifest
//! v1 names no directory, so the CAS entry's own directories are not
//! copied), then verifies the copy, its directories included, against the
//! same manifest before giving libkrun access to it.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, fstat, open, openat, statat};

use crate::ExecutionError;

use super::plan::{
    NORMALIZED_DIRECTORY_MODE, ROOTFS_MANIFEST, ResolvedRootfs, implied_directories,
    manifest_file_paths, verify_manifest,
};

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
    // Manifest v1 names files, not directories, so the copy is the
    // manifest's tree, not the CAS entry's: the listed files, the
    // directories they imply, each at 0755, and nothing else (bead
    // `ley-line-open-38ff59`). The CAS entry's own directory set and modes
    // are not part of the digest, so they never reach the guest.
    let files = manifest_file_paths(&source_path, &source.digest)?;
    let plan = CopyPlan {
        directories: implied_directories(&files),
        files,
    };
    copy_directory_contents(&source_path, &guest_root, &plan)?;
    let materialized = ResolvedRootfs {
        digest: source.digest.clone(),
        canonical_path: guest_root,
    };
    verify_ephemeral_rootfs(&materialized)?;
    Ok(materialized)
}

/// What a per-run copy holds: the manifest's files and the directories they
/// imply, rootfs-relative.
struct CopyPlan {
    files: BTreeSet<PathBuf>,
    directories: BTreeSet<PathBuf>,
}

fn copy_directory_contents(
    source: &Path,
    destination: &Path,
    plan: &CopyPlan,
) -> Result<(), ExecutionError> {
    let source = open(
        source,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| invalid_rustix("open immutable rootfs directory", error))?;
    copy_directory_fd(&source, Path::new(""), Some(destination), plan)?;
    fs::set_permissions(
        destination,
        fs::Permissions::from_mode(NORMALIZED_DIRECTORY_MODE),
    )
    .map_err(|error| invalid_io("set ephemeral guest rootfs permissions", error))
}

/// Copy the entries of `source` (at `relative` in the rootfs) that `plan`
/// lists into `destination`, which is `None` for a directory the manifest
/// does not imply: one is walked, so any file in it is refused as
/// undeclared, but never created.
fn copy_directory_fd(
    source: &impl AsFd,
    relative: &Path,
    destination: Option<&Path>,
    plan: &CopyPlan,
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
        if expected_type.is_symlink() {
            return Err(ExecutionError::invalid(
                "rootfs symbolic links are not supported",
            ));
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
        let entry_relative = relative.join(name);
        if expected_type.is_dir() {
            if !plan.directories.contains(&entry_relative) {
                // Not implied by the manifest: never created in the copy.
                copy_directory_fd(&opened, &entry_relative, None, plan)?;
                continue;
            }
            // Every ancestor of an implied directory is implied, so its
            // parent was created.
            let Some(destination) = destination else {
                return Err(ExecutionError::invalid(format!(
                    "rootfs directory {} is implied but its parent is not",
                    entry_relative.display()
                )));
            };
            let destination_path = destination.join(name);
            fs::create_dir(&destination_path)
                .map_err(|error| invalid_io("create ephemeral rootfs directory", error))?;
            copy_directory_fd(&opened, &entry_relative, Some(&destination_path), plan)?;
            fs::set_permissions(
                &destination_path,
                fs::Permissions::from_mode(NORMALIZED_DIRECTORY_MODE),
            )
            .map_err(|error| invalid_io("set ephemeral rootfs directory permissions", error))?;
        } else {
            let listed = plan.files.contains(&entry_relative)
                || entry_relative.as_os_str() == ROOTFS_MANIFEST;
            let (true, Some(destination)) = (listed, destination) else {
                return Err(ExecutionError::invalid(format!(
                    "rootfs file is not in the content-addressed manifest: {}",
                    entry_relative.display()
                )));
            };
            let destination_path = destination.join(name);
            let permissions = fs::Permissions::from_mode((before.st_mode as u32) & 0o7777);
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
/// its files against the manifest, and its directories against the
/// manifest's implied set, each at [`NORMALIZED_DIRECTORY_MODE`], the root
/// included. A directory planted or re-moded after materialization is
/// refused by name.
pub fn verify_ephemeral_rootfs(rootfs: &ResolvedRootfs) -> Result<(), ExecutionError> {
    let tree = verify_manifest(&rootfs.canonical_path, &rootfs.digest)?;
    let implied = implied_directories(&tree.files);
    for (directory, mode) in &tree.directories {
        if !implied.contains(directory) {
            return Err(ExecutionError::invalid(format!(
                "rootfs directory is not implied by the manifest: {}",
                directory.display()
            )));
        }
        if *mode != NORMALIZED_DIRECTORY_MODE {
            return Err(ExecutionError::invalid(format!(
                "rootfs directory mode {mode:o} is not the normalized {NORMALIZED_DIRECTORY_MODE:o}: /{}",
                directory.display()
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
