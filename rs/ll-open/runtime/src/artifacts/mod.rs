//! First-party runtime artifact resolvers (bead `ley-line-open-7736ab`).
//!
//! A container image is a build-input and distribution format here, never
//! the isolation boundary: libkrun (or the native nono tier) is. These
//! resolvers turn an artifact into the authenticated content-addressed rootfs
//! the backends already consume — a directory `<cas-root>/<blake3 hex>`
//! holding the files and `rootfs.manifest.json`, whose BLAKE3 is the
//! directory's name and the `rootfs` digest an execution catalog names.
//! `backends::libkrun::plan::DirectoryRootfsResolver` verifies it on every
//! resolve. No resolver here needs Docker, Buildah, Podman or krunvm; those
//! tools may produce the OCI image layout an import reads, and nothing else.

pub mod oci;
pub mod registry;
mod rootfs_tree;
