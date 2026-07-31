//! Per-run writable rootfs views for the embedded libkrun backend.
//!
//! The authenticated CAS tree remains immutable. A worker receives an empty,
//! parent-owned directory, copies the verified tree into it, then verifies the
//! copy against the same manifest before giving libkrun access to it.

use std::fs;
use std::path::{Path, PathBuf};

use crate::ExecutionError;

use super::plan::{ResolvedRootfs, verify_manifest};

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

    copy_directory_contents(&source_path, &destination)?;
    fs::set_permissions(
        &destination,
        fs::symlink_metadata(&source_path)
            .map_err(|error| invalid_io("read immutable rootfs metadata", error))?
            .permissions(),
    )
    .map_err(|error| invalid_io("preserve ephemeral rootfs permissions", error))?;
    verify_manifest(&destination, &source.digest)?;

    Ok(ResolvedRootfs {
        digest: source.digest.clone(),
        canonical_path: destination,
    })
}

fn copy_directory_contents(source: &Path, destination: &Path) -> Result<(), ExecutionError> {
    let mut entries = fs::read_dir(source)
        .map_err(|error| invalid_io("enumerate immutable rootfs", error))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| invalid_io("enumerate immutable rootfs entry", error))?;
    entries.sort_by_key(|entry| entry.file_name());

    for entry in entries {
        let source_path = entry.path();
        let destination_path: PathBuf = destination.join(entry.file_name());
        let metadata = fs::symlink_metadata(&source_path)
            .map_err(|error| invalid_io("read immutable rootfs entry metadata", error))?;
        if metadata.file_type().is_symlink() {
            return Err(ExecutionError::invalid(
                "rootfs symbolic links are not supported",
            ));
        }
        if metadata.is_dir() {
            fs::create_dir(&destination_path)
                .map_err(|error| invalid_io("create ephemeral rootfs directory", error))?;
            copy_directory_contents(&source_path, &destination_path)?;
            fs::set_permissions(&destination_path, metadata.permissions()).map_err(|error| {
                invalid_io("preserve ephemeral rootfs directory permissions", error)
            })?;
        } else if metadata.is_file() {
            fs::copy(&source_path, &destination_path)
                .map_err(|error| invalid_io("copy ephemeral rootfs file", error))?;
            fs::set_permissions(&destination_path, metadata.permissions())
                .map_err(|error| invalid_io("preserve ephemeral rootfs file permissions", error))?;
        } else {
            return Err(ExecutionError::invalid(
                "rootfs contains a non-regular filesystem entry",
            ));
        }
    }
    Ok(())
}

fn invalid_io(action: &str, error: std::io::Error) -> ExecutionError {
    ExecutionError::invalid(format!("{action}: {error}"))
}
