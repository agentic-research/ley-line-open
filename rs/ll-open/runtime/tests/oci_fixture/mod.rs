//! Deterministic OCI image-layout builder for `tests/oci_rootfs.rs`.
//!
//! This module is the generator of the checked-in fixture at
//! `tests/fixtures/oci-multilayer` (bead `ley-line-open-7736ab`): the fixture
//! is never hand-edited. `checked_in_fixture_matches_its_generator` rebuilds
//! it and compares every byte; `task runtime:oci-fixture` rewrites it after a
//! deliberate change here.
//!
//! Every byte is a function of the entries alone: tar headers carry mtime,
//! uid and gid zero and no user or group names, gzip members carry mtime zero,
//! and JSON documents are serialized from `serde_json::json!` values whose
//! key order is the insertion order written here.

#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};

use flate2::{Compression, GzBuilder};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub const OCI_INDEX: &str = "application/vnd.oci.image.index.v1+json";
pub const OCI_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
pub const OCI_CONFIG: &str = "application/vnd.oci.image.config.v1+json";
pub const OCI_LAYER_TAR: &str = "application/vnd.oci.image.layer.v1.tar";
pub const OCI_LAYER_GZIP: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
pub const OCI_LAYER_ZSTD: &str = "application/vnd.oci.image.layer.v1.tar+zstd";

/// One tar entry. Paths and link targets are written into the header's name
/// fields verbatim, so a test can produce `../`, absolute, and other hostile
/// names a well-behaved archiver would refuse to write.
#[derive(Clone, Debug)]
pub enum Entry {
    Dir {
        path: String,
        mode: u32,
    },
    File {
        path: String,
        mode: u32,
        data: Vec<u8>,
    },
    Symlink {
        path: String,
        target: String,
    },
    Hardlink {
        path: String,
        target: String,
    },
    Fifo {
        path: String,
    },
}

pub fn dir(path: &str, mode: u32) -> Entry {
    Entry::Dir {
        path: path.into(),
        mode,
    }
}

pub fn file(path: &str, mode: u32, data: &[u8]) -> Entry {
    Entry::File {
        path: path.into(),
        mode,
        data: data.to_vec(),
    }
}

pub fn symlink(path: &str, target: &str) -> Entry {
    Entry::Symlink {
        path: path.into(),
        target: target.into(),
    }
}

pub fn hardlink(path: &str, target: &str) -> Entry {
    Entry::Hardlink {
        path: path.into(),
        target: target.into(),
    }
}

pub fn fifo(path: &str) -> Entry {
    Entry::Fifo { path: path.into() }
}

fn set_raw(field: &mut [u8], value: &str) {
    let bytes = value.as_bytes();
    assert!(
        bytes.len() < field.len(),
        "fixture name {value:?} does not fit a ustar header field"
    );
    field.fill(0);
    field[..bytes.len()].copy_from_slice(bytes);
}

/// An uncompressed layer tarball holding `entries` in the order given.
pub fn tar(entries: &[Entry]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for entry in entries {
        let mut header = tar::Header::new_ustar();
        header.set_mtime(0);
        header.set_uid(0);
        header.set_gid(0);
        header.set_size(0);
        let (path, data): (&str, &[u8]) = match entry {
            Entry::Dir { path, mode } => {
                header.set_entry_type(tar::EntryType::Directory);
                header.set_mode(*mode);
                (path, &[])
            }
            Entry::File { path, mode, data } => {
                header.set_entry_type(tar::EntryType::Regular);
                header.set_mode(*mode);
                header.set_size(data.len() as u64);
                (path, data)
            }
            Entry::Symlink { path, target } => {
                header.set_entry_type(tar::EntryType::Symlink);
                header.set_mode(0o777);
                set_raw(&mut header.as_old_mut().linkname, target);
                (path, &[])
            }
            Entry::Hardlink { path, target } => {
                header.set_entry_type(tar::EntryType::Link);
                header.set_mode(0o644);
                set_raw(&mut header.as_old_mut().linkname, target);
                (path, &[])
            }
            Entry::Fifo { path } => {
                header.set_entry_type(tar::EntryType::Fifo);
                header.set_mode(0o644);
                (path, &[])
            }
        };
        set_raw(&mut header.as_old_mut().name, path);
        header.set_cksum();
        builder.append(&header, data).expect("append tar entry");
    }
    builder.into_inner().expect("finish tar")
}

pub fn gzip(bytes: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut encoder = GzBuilder::new()
        .mtime(0)
        .write(Vec::new(), Compression::new(6));
    encoder.write_all(bytes).expect("gzip layer");
    encoder.finish().expect("finish gzip")
}

pub fn sha256(bytes: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(bytes)))
}

#[derive(Clone, Debug)]
pub struct Descriptor {
    pub media_type: String,
    pub digest: String,
    pub size: u64,
}

impl Descriptor {
    pub fn json(&self) -> Value {
        json!({
            "mediaType": self.media_type,
            "digest": self.digest,
            "size": self.size,
        })
    }

    pub fn hex(&self) -> &str {
        self.digest.strip_prefix("sha256:").expect("sha256 digest")
    }
}

/// A layer as an image manifest lists it.
pub struct Layer {
    pub media_type: &'static str,
    pub entries: Vec<Entry>,
}

impl Layer {
    pub fn gzip(entries: Vec<Entry>) -> Self {
        Self {
            media_type: OCI_LAYER_GZIP,
            entries,
        }
    }

    pub fn plain(entries: Vec<Entry>) -> Self {
        Self {
            media_type: OCI_LAYER_TAR,
            entries,
        }
    }
}

/// An OCI image layout directory under construction.
pub struct Layout {
    pub root: PathBuf,
}

impl Layout {
    pub fn create(root: &Path) -> Self {
        fs::create_dir_all(root.join("blobs/sha256")).expect("layout blob directory");
        fs::write(
            root.join("oci-layout"),
            b"{\"imageLayoutVersion\":\"1.0.0\"}",
        )
        .expect("oci-layout");
        Self {
            root: root.to_path_buf(),
        }
    }

    pub fn blob_path(&self, digest: &str) -> PathBuf {
        self.root
            .join("blobs/sha256")
            .join(digest.strip_prefix("sha256:").expect("sha256 digest"))
    }

    pub fn blob(&self, media_type: &str, bytes: &[u8]) -> Descriptor {
        let digest = sha256(bytes);
        fs::write(self.blob_path(&digest), bytes).expect("write blob");
        Descriptor {
            media_type: media_type.into(),
            digest,
            size: bytes.len() as u64,
        }
    }

    pub fn json_blob(&self, media_type: &str, document: &Value) -> Descriptor {
        self.blob(
            media_type,
            &serde_json::to_vec(document).expect("serialize OCI document"),
        )
    }

    /// Write `layers`, a config naming their diff_ids, and the manifest.
    /// `diff_ids` overrides the computed list when a test needs a lie.
    pub fn image(
        &self,
        os: &str,
        architecture: &str,
        layers: &[Layer],
        diff_ids: Option<Vec<String>>,
    ) -> Descriptor {
        let mut descriptors = Vec::new();
        let mut computed = Vec::new();
        for layer in layers {
            let uncompressed = tar(&layer.entries);
            computed.push(sha256(&uncompressed));
            let stored = if layer.media_type == OCI_LAYER_GZIP {
                gzip(&uncompressed)
            } else {
                uncompressed
            };
            descriptors.push(self.blob(layer.media_type, &stored).json());
        }
        let config = self.json_blob(
            OCI_CONFIG,
            &json!({
                "architecture": architecture,
                "os": os,
                "config": {
                    "Entrypoint": ["/bin/tool"],
                    "Cmd": ["--serve"],
                    "Env": ["PATH=/usr/bin:/bin"],
                    "WorkingDir": "/"
                },
                "rootfs": {
                    "type": "layers",
                    "diff_ids": diff_ids.unwrap_or(computed)
                }
            }),
        );
        self.json_blob(
            OCI_MANIFEST,
            &json!({
                "schemaVersion": 2,
                "mediaType": OCI_MANIFEST,
                "config": config.json(),
                "layers": descriptors
            }),
        )
    }

    /// An image index blob listing `manifests` with their platforms.
    pub fn index(&self, manifests: &[(Descriptor, Value)]) -> Descriptor {
        let listed: Vec<Value> = manifests
            .iter()
            .map(|(descriptor, platform)| {
                let mut value = descriptor.json();
                value["platform"] = platform.clone();
                value
            })
            .collect();
        self.json_blob(
            OCI_INDEX,
            &json!({
                "schemaVersion": 2,
                "mediaType": OCI_INDEX,
                "manifests": listed
            }),
        )
    }

    /// The layout's `index.json`, naming `top` by a mutable tag.
    pub fn tag(&self, top: &Descriptor, tag: &str) {
        let mut value = top.json();
        value["annotations"] = json!({"org.opencontainers.image.ref.name": tag});
        let index = json!({
            "schemaVersion": 2,
            "mediaType": OCI_INDEX,
            "manifests": [value]
        });
        fs::write(
            self.root.join("index.json"),
            serde_json::to_vec(&index).expect("serialize index.json"),
        )
        .expect("write index.json");
    }
}

pub fn platform(os: &str, architecture: &str, variant: Option<&str>) -> Value {
    match variant {
        Some(variant) => json!({"architecture": architecture, "os": os, "variant": variant}),
        None => json!({"architecture": architecture, "os": os}),
    }
}

/// The base layer every fixture image shares. Directories precede their
/// children, as an archiver walking a filesystem writes them.
pub fn base_layer() -> Vec<Entry> {
    vec![
        dir("bin/", 0o755),
        file("bin/tool", 0o755, b"#!/bin/sh\necho tool\n"),
        hardlink("bin/tool-hardlink", "bin/tool"),
        file("bin/su", 0o4755, b"setuid\n"),
        symlink("bin/abs-alias", "/usr/bin/real"),
        dir("etc/", 0o755),
        file("etc/config", 0o644, b"v1\n"),
        file("etc/secret", 0o600, b"secret\n"),
        file("etc/removed", 0o644, b"gone\n"),
        dir("usr/", 0o755),
        dir("usr/bin/", 0o755),
        file("usr/bin/real", 0o755, b"real\n"),
        symlink("usr/bin/alias", "real"),
        dir("var/", 0o755),
        dir("var/cache/", 0o755),
        file("var/cache/a", 0o644, b"a\n"),
        file("var/cache/b", 0o644, b"b\n"),
    ]
}

/// The second layer: a whiteout, an opaque whiteout, a file added under the
/// opaque directory, and an overwrite that changes content and mode.
pub fn change_layer() -> Vec<Entry> {
    vec![
        file("etc/.wh.removed", 0o644, b""),
        file("etc/config", 0o640, b"v2\n"),
        file("var/cache/.wh..wh..opq", 0o644, b""),
        file("var/cache/c", 0o644, b"c\n"),
    ]
}

pub fn arch_layer(architecture: &str) -> Vec<Entry> {
    vec![file(
        "etc/arch",
        0o644,
        format!("{architecture}\n").as_bytes(),
    )]
}

/// The digests of the canonical fixture's documents.
pub struct Fixture {
    pub index: Descriptor,
    pub amd64: Descriptor,
    pub arm64: Descriptor,
}

/// Build the canonical multi-platform, three-layer fixture into `root`.
/// `reorder` writes every layer's entries in reverse, children before their
/// directories: the same content an archiver walking the filesystem in a
/// different order would produce.
pub fn build_fixture(root: &Path, reorder: bool) -> Fixture {
    let layout = Layout::create(root);
    let order = |mut entries: Vec<Entry>| {
        if reorder {
            entries.reverse();
            // A hard link names an entry earlier in the archive; keep each
            // link after its target, as every archiver does.
            let (links, mut rest): (Vec<_>, Vec<_>) = entries
                .into_iter()
                .partition(|entry| matches!(entry, Entry::Hardlink { .. }));
            rest.extend(links);
            rest
        } else {
            entries
        }
    };
    let image = |architecture: &str| {
        layout.image(
            "linux",
            architecture,
            &[
                Layer::gzip(order(base_layer())),
                Layer::plain(order(change_layer())),
                Layer::gzip(order(arch_layer(architecture))),
            ],
            None,
        )
    };
    let amd64 = image("amd64");
    let arm64 = image("arm64");
    let index = layout.index(&[
        (amd64.clone(), platform("linux", "amd64", None)),
        (arm64.clone(), platform("linux", "arm64", Some("v8"))),
    ]);
    layout.tag(&index, "latest");
    Fixture {
        index,
        amd64,
        arm64,
    }
}

/// The checked-in fixture directory.
pub fn checked_in_fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/oci-multilayer")
}

/// Every file under `root`, relative, with its bytes, in path order.
pub fn snapshot(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
        let mut entries: Vec<_> = fs::read_dir(dir)
            .expect("read fixture directory")
            .map(|entry| entry.expect("fixture entry").path())
            .collect();
        entries.sort();
        for path in entries {
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                let bytes = fs::read(&path).expect("read fixture file");
                out.push((path.strip_prefix(root).unwrap().to_path_buf(), bytes));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out
}
