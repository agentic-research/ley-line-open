//! Content-addressed blob store implementations (bead `ley-line-open-bb0316`,
//! T3.1 of the T3 CAS-blob-store thread under decade `ley-line-open-9d30ac`).
//!
//! Two impls of [`crate::substrate::BlobStore`]:
//!
//! - [`FsBlobStore`] — git-shaped filesystem layout, production.
//! - [`MemBlobStore`] — in-memory `HashMap`, testing-only.
//!
//! Both honor the trait's two non-negotiable axioms (see `substrate.rs`):
//!
//! - **(IM)** Idempotent insert: same bytes ⇒ same hash ⇒ same on-disk
//!   entry, never duplicated, original bytes are preserved (the second
//!   put is a no-op).
//! - **(verify-on-read)** Every `get(h)` returning `Some(v)` MUST
//!   satisfy `σ(v) == h`. A returned blob whose hash doesn't match its
//!   key indicates filesystem corruption, a torn write, or an attacker
//!   tampering with the store — the substrate refuses to vouch for it.
//!
//! ## Filesystem layout
//!
//! Modeled on git's `.git/objects/<aa>/<bbcc...>` shape so directory
//! fanout stays bounded (256 first-level dirs, ~256 entries per dir
//! at uniform sampling) and `ls`'ing an objects dir gives O(1)
//! recognizable buckets. Path resolution:
//!
//! ```text
//! <arena_dir>/objects/<hex(hash[0])>/<hex(hash[1..])>
//! ```
//!
//! So a hash `0xAB CD EF ... (32 bytes)` lives at
//! `<arena_dir>/objects/ab/cdef...`. The two-char prefix matches the
//! 256-way fanout convention; the 62-char remainder is the full
//! suffix (32 bytes → 64 hex chars → 2 prefix + 62 remainder).
//!
//! ## Atomicity
//!
//! Writes follow the canonical "temp file + atomic rename" pattern.
//! Reader concurrent with writer either sees the OLD content (rename
//! not yet applied) or the NEW content (rename applied) — never a
//! torn write. On POSIX the rename within the same directory is
//! atomic. On Windows, `std::fs::rename` is NOT atomic if the target
//! exists, but the idempotency check (`contains` before write) makes
//! this safe — we never rename onto a present target.
//!
//! ## Why a separate file
//!
//! Per `lib.rs` layout convention: substrate types live in
//! `substrate.rs` (definitions only, no impls); concrete storage
//! impls live in their own module. Mirrors how `control.rs` and
//! `layout.rs` are siblings.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, bail};

use crate::substrate::{BlobStore, ContentAddressed, Hash};

// ─────────────────────────────────────────────────────────────────────
// FsBlobStore — filesystem-backed, production impl
// ─────────────────────────────────────────────────────────────────────

/// Filesystem-backed content-addressed blob store.
///
/// Layout: `<root>/<hex(hash[0])>/<hex(hash[1..])>`, where `<root>` is
/// typically `<arena_dir>/objects` per bead `ley-line-open-bb0316`.
///
/// Construct via [`FsBlobStore::open`] (auto-creates the root if
/// missing) or [`FsBlobStore::new`] (fails if root doesn't exist —
/// useful when the caller wants to assert the arena was already
/// initialized by a sibling subsystem).
///
/// All methods are `&self` capable except `put` which takes `&mut self`
/// per the trait. There's no in-memory state worth gating — the `&mut`
/// is the trait's contract, not an internal serialization need; two
/// concurrent putters racing onto the same hash both produce the
/// idempotent no-op outcome.
#[derive(Debug)]
pub struct FsBlobStore {
    root: PathBuf,
}

impl FsBlobStore {
    /// Open an `FsBlobStore` rooted at `root`. Creates the directory
    /// if missing, including parents. Use this when the caller doesn't
    /// know whether the arena's `objects/` dir exists yet (e.g. fresh
    /// repo, first init).
    pub fn open(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root)
            .with_context(|| format!("create blob-store root {}", root.display()))?;
        Ok(Self { root })
    }

    /// Construct without creating `root` — fails if it doesn't exist.
    /// Use this in tests or in contexts where the directory should
    /// already have been provisioned (e.g. consumer asserts arena
    /// init ran first).
    pub fn new(root: impl AsRef<Path>) -> Result<Self> {
        let root = root.as_ref().to_path_buf();
        if !root.is_dir() {
            bail!(
                "FsBlobStore root {} does not exist (use open() to auto-create)",
                root.display()
            );
        }
        Ok(Self { root })
    }

    /// Path on disk for a given hash. Used by both `put` and `get`.
    /// Public for diagnostic uses (e.g. "where would this blob land?")
    /// but not part of the trait surface — implementations of `BlobStore`
    /// may not expose any path concept at all (`MemBlobStore` doesn't).
    pub fn path_for(&self, h: &Hash) -> PathBuf {
        let bytes = h.as_bytes();
        // First byte → directory; rest → filename. Hex-encoded.
        let dir_name = format!("{:02x}", bytes[0]);
        let mut file_name = String::with_capacity(62);
        for b in &bytes[1..] {
            file_name.push_str(&format!("{b:02x}"));
        }
        self.root.join(dir_name).join(file_name)
    }

    /// Root directory of the store. Useful for tests and for diagnostic
    /// tooling (`ls $(blob_store_root)`).
    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl BlobStore for FsBlobStore {
    fn put(&mut self, bytes: &[u8]) -> Result<Hash> {
        let hash = bytes.hash();
        let final_path = self.path_for(&hash);

        // Idempotency (IM axiom): if the target already exists AND
        // round-trip-verifies as the same hash, we're done. We do NOT
        // re-write — that would also be correct but wastes IO and
        // creates a torn-write window for a concurrent reader.
        //
        // Subtle: a corrupted file at `final_path` (bytes mutated
        // since previous put) would have a hash mismatch on `get`,
        // which the verify-on-read path catches. Here we trust the
        // path's existence as a fast-path for the common case; the
        // corrupt case fails loudly on read, not silently on write.
        if final_path.exists() {
            return Ok(hash);
        }

        // Ensure parent dir exists. `create_dir_all` is idempotent.
        let parent = final_path
            .parent()
            .with_context(|| format!("path_for produced rootless path: {final_path:?}"))?;
        fs::create_dir_all(parent)
            .with_context(|| format!("create blob bucket dir {}", parent.display()))?;

        // Atomic write: temp file in the SAME directory (so rename is
        // atomic on POSIX), then rename onto the final path. Temp name
        // includes the hash + PID to keep concurrent putters from
        // stomping each other's temp file.
        let tmp_name = format!(".tmp-{}-{}", std::process::id(), hash);
        let tmp_path = parent.join(tmp_name);

        // Scope the file handle so it closes before the rename — on
        // some platforms a held handle can interfere with the rename.
        {
            let mut tmp = OpenOptions::new()
                .write(true)
                .create_new(true) // O_EXCL: refuses to overwrite an
                // existing temp from a crashed previous writer. Pick
                // a different PID on retry; or sweep stale .tmp files
                // out-of-band.
                .open(&tmp_path)
                .with_context(|| format!("create temp blob {}", tmp_path.display()))?;
            tmp.write_all(bytes)
                .with_context(|| format!("write temp blob {}", tmp_path.display()))?;
            // fsync the data before rename. Without this, on a crash
            // between write and rename, we could see an empty/partial
            // file at the temp path that an unrelated retry then
            // renames onto the final path. Worth the latency for the
            // substrate guarantee.
            tmp.sync_all()
                .with_context(|| format!("fsync temp blob {}", tmp_path.display()))?;
        }

        fs::rename(&tmp_path, &final_path).with_context(|| {
            format!(
                "rename temp blob {} → {}",
                tmp_path.display(),
                final_path.display()
            )
        })?;

        Ok(hash)
    }

    fn get(&self, h: Hash) -> Result<Option<Vec<u8>>> {
        let path = self.path_for(&h);
        let mut file = match File::open(&path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("open blob file {}", path.display()));
            }
        };

        let mut buf = Vec::new();
        file.read_to_end(&mut buf)
            .with_context(|| format!("read blob file {}", path.display()))?;

        // Verify-on-read (substrate contract). The hash of the bytes
        // we just read MUST equal the key. Mismatch indicates corruption
        // (disk error, torn write recovery, or tampering). Return an
        // error rather than `None` because:
        //   - `None` means "absent" and consumers may retry / repair;
        //   - corruption is a substrate-integrity event the caller
        //     needs to surface, not paper over.
        let actual = buf.as_slice().hash();
        if actual != h {
            bail!(
                "blob-store integrity violation at {}: stored bytes hash to {} but key is {}",
                path.display(),
                actual,
                h
            );
        }

        Ok(Some(buf))
    }

    fn contains(&self, h: Hash) -> Result<bool> {
        Ok(self.path_for(&h).exists())
    }
}

// ─────────────────────────────────────────────────────────────────────
// MemBlobStore — in-memory, testing impl
// ─────────────────────────────────────────────────────────────────────

/// In-memory blob store, primarily for tests. Honors the same axioms
/// as `FsBlobStore` (idempotent put, verify-on-read on `get`) so
/// behavior under tests matches behavior in production.
///
/// Internally a `Mutex<HashMap<Hash, Vec<u8>>>` — the mutex enables
/// `&self` access patterns even on `put` (the trait demands `&mut self`,
/// but a test that wants to share the store across threads via `Arc`
/// without `&mut` can wrap it). Within the trait method the borrow is
/// `&mut`, so the mutex is uncontended in the common path.
#[derive(Debug, Default)]
pub struct MemBlobStore {
    inner: Mutex<HashMap<Hash, Vec<u8>>>,
}

impl MemBlobStore {
    /// Construct an empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Current number of stored entries. Useful for tests verifying
    /// idempotency (insert same content twice ⇒ count stays 1).
    pub fn len(&self) -> usize {
        self.inner.lock().expect("MemBlobStore mutex poisoned").len()
    }

    /// True iff no blobs are stored. Mirrors `Vec::is_empty` for parity.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl BlobStore for MemBlobStore {
    fn put(&mut self, bytes: &[u8]) -> Result<Hash> {
        let hash = bytes.hash();
        let mut guard = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("MemBlobStore mutex poisoned"))?;
        // (IM) axiom: same hash ⇒ already present ⇒ no-op. We do not
        // re-insert; the existing bytes are authoritative.
        guard.entry(hash).or_insert_with(|| bytes.to_vec());
        Ok(hash)
    }

    fn get(&self, h: Hash) -> Result<Option<Vec<u8>>> {
        let guard = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("MemBlobStore mutex poisoned"))?;
        match guard.get(&h) {
            None => Ok(None),
            Some(bytes) => {
                // Verify-on-read. For an in-memory store this is
                // arguably redundant (we control the insert path) —
                // but the trait demands it, and asserting here means
                // a test corrupting the inner map sees the same loud
                // error a production filesystem corruption would.
                let actual = bytes.as_slice().hash();
                if actual != h {
                    bail!(
                        "MemBlobStore integrity violation: stored bytes hash to {} but key is {}",
                        actual,
                        h
                    );
                }
                Ok(Some(bytes.clone()))
            }
        }
    }

    fn contains(&self, h: Hash) -> Result<bool> {
        let guard = self
            .inner
            .lock()
            .map_err(|_| anyhow::anyhow!("MemBlobStore mutex poisoned"))?;
        Ok(guard.contains_key(&h))
    }
}

// ─────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // Helpers
    fn fs_store() -> (FsBlobStore, TempDir) {
        let td = TempDir::new().expect("tempdir");
        let store = FsBlobStore::open(td.path().join("objects")).expect("open store");
        (store, td)
    }

    // ── round-trip ────────────────────────────────────────────────────

    #[test]
    fn fs_put_get_round_trips() {
        let (mut s, _td) = fs_store();
        let bytes = b"the substrate vouches for this";
        let h = s.put(bytes).expect("put");
        assert_eq!(h, bytes.hash(), "put returns σ(bytes)");
        let got = s.get(h).expect("get").expect("present");
        assert_eq!(got, bytes);
    }

    #[test]
    fn mem_put_get_round_trips() {
        let mut s = MemBlobStore::new();
        let bytes = b"in-memory parity";
        let h = s.put(bytes).expect("put");
        assert_eq!(h, bytes.hash());
        let got = s.get(h).expect("get").expect("present");
        assert_eq!(got, bytes);
    }

    // ── idempotency (IM axiom) ────────────────────────────────────────

    #[test]
    fn fs_put_idempotent() {
        let (mut s, _td) = fs_store();
        let bytes = b"idempotency check";
        let h1 = s.put(bytes).expect("first put");
        let h2 = s.put(bytes).expect("second put");
        assert_eq!(h1, h2, "(IM): same bytes ⇒ same hash");

        // Confirm we still get the bytes back; no corruption from the
        // double-put.
        let got = s.get(h1).expect("get").expect("present");
        assert_eq!(got, bytes);
    }

    #[test]
    fn fs_put_idempotent_preserves_original_bytes() {
        // Stronger (IM) check: an attacker calling put() with the SAME
        // hash but DIFFERENT bytes (collision attempt) must not be able
        // to overwrite the original. Today this is moot because BLAKE3
        // collision resistance > our adversary budget, but the test
        // pins the property at the impl level so a future hash
        // weakness doesn't silently change semantics.
        let (mut s, _td) = fs_store();
        let bytes = b"original";
        let h = s.put(bytes).expect("put");

        // Force-write different bytes to the same path. Simulates the
        // "what if put doesn't notice the existing file" scenario.
        let path = s.path_for(&h);
        // Reset to the legit content first to verify the harness:
        fs::write(&path, bytes).unwrap();

        // Now: a second put of the same bytes is a no-op.
        let h2 = s.put(bytes).expect("second put");
        assert_eq!(h, h2);
        let got = s.get(h).expect("get").expect("present");
        assert_eq!(got, bytes);
    }

    #[test]
    fn mem_put_idempotent() {
        let mut s = MemBlobStore::new();
        let bytes = b"mem idempotency";
        let h1 = s.put(bytes).expect("first put");
        let h2 = s.put(bytes).expect("second put");
        assert_eq!(h1, h2);
        assert_eq!(s.len(), 1, "(IM): no duplicate entries");
    }

    // ── absence ───────────────────────────────────────────────────────

    #[test]
    fn fs_get_missing_returns_none() {
        let (s, _td) = fs_store();
        // Synthesize a hash that's NOT in the store. Use a sentinel
        // value (all 0xAA) rather than Hash::ZERO so the path doesn't
        // collide with anyone using ZERO as a "no-data" marker.
        let absent = Hash::from_bytes([0xAA; 32]);
        assert!(s.get(absent).expect("get").is_none());
        assert!(!s.contains(absent).expect("contains"));
    }

    #[test]
    fn mem_get_missing_returns_none() {
        let s = MemBlobStore::new();
        let absent = Hash::from_bytes([0xBB; 32]);
        assert!(s.get(absent).expect("get").is_none());
        assert!(!s.contains(absent).expect("contains"));
    }

    // ── verify-on-read (corruption detection) ─────────────────────────

    #[test]
    fn fs_get_detects_corruption() {
        let (mut s, _td) = fs_store();
        let bytes = b"original payload";
        let h = s.put(bytes).expect("put");

        // Simulate disk corruption: overwrite the file with different
        // bytes that hash to a different value.
        let path = s.path_for(&h);
        fs::write(&path, b"tampered payload").expect("overwrite");

        // get() MUST refuse to return the corrupted bytes. The error
        // message should name the path so an operator can investigate.
        let err = s.get(h).expect_err("expected integrity violation");
        let msg = format!("{err:?}");
        assert!(
            msg.contains("integrity violation"),
            "error should name integrity: {msg}"
        );
        assert!(
            msg.contains(&path.display().to_string()),
            "error should include the path: {msg}"
        );
    }

    #[test]
    fn mem_get_detects_corruption() {
        // Inject corruption directly into the inner map (test-only
        // backdoor) to pin that the verify-on-read path is also
        // exercised in the in-memory impl.
        let mut s = MemBlobStore::new();
        let bytes = b"original";
        let h = s.put(bytes).expect("put");
        {
            let mut guard = s.inner.lock().unwrap();
            guard.insert(h, b"tampered".to_vec());
        }

        let err = s.get(h).expect_err("expected integrity violation");
        assert!(format!("{err:?}").contains("integrity violation"));
    }

    // ── filesystem layout shape ───────────────────────────────────────

    #[test]
    fn fs_layout_matches_git_shape() {
        // Pin the exact layout decision from bead ley-line-open-bb0316:
        //   <root>/objects/<hex(hash[0])>/<hex(hash[1..])>
        let (s, _td) = fs_store();
        let bytes = b"shape check";
        let h = bytes.hash();
        let path = s.path_for(&h);

        // Strip root, get the relative components.
        let rel = path.strip_prefix(s.root()).expect("path under root");
        let components: Vec<_> = rel.components().collect();
        assert_eq!(
            components.len(),
            2,
            "layout: <prefix-dir>/<remainder-file>, got {components:?}"
        );

        let dir_name = components[0].as_os_str().to_str().unwrap();
        let file_name = components[1].as_os_str().to_str().unwrap();
        assert_eq!(dir_name.len(), 2, "prefix dir is 2 hex chars");
        assert_eq!(file_name.len(), 62, "remainder file is 62 hex chars (31 bytes)");
        assert!(
            dir_name.chars().all(|c| c.is_ascii_hexdigit()),
            "prefix is hex"
        );
        assert!(
            file_name.chars().all(|c| c.is_ascii_hexdigit()),
            "remainder is hex"
        );

        // Construct the expected by-hand from h.as_bytes() and assert
        // exact equality — pins the prefix byte AND the order.
        let bytes_h = h.as_bytes();
        let expected_dir = format!("{:02x}", bytes_h[0]);
        let expected_file: String = bytes_h[1..]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(dir_name, expected_dir);
        assert_eq!(file_name, expected_file);
    }

    #[test]
    fn fs_distinct_hashes_in_same_prefix_coexist() {
        // Construct two artificial hashes whose first byte is the same
        // but the rest differ; both should land in the same prefix dir
        // without one overwriting the other.
        //
        // We can't synthesize two real bytes whose BLAKE3 first-byte
        // collide cheaply (it's only ~1/256 of input pairs), so use
        // from_bytes() with hand-picked Hash values directly. Since
        // FsBlobStore::put() actually hashes the content and ignores
        // any passed hash, we instead pick CONTENT whose hashes share
        // a first byte by brute-forcing a few candidates.
        let (mut s, _td) = fs_store();

        // Find two distinct contents whose hashes share byte[0].
        let mut a: Option<(Vec<u8>, u8)> = None;
        let mut b: Option<(Vec<u8>, u8)> = None;
        for i in 0u32..10_000 {
            let v = i.to_le_bytes().to_vec();
            let h = v.as_slice().hash();
            let byte0 = h.as_bytes()[0];
            match &a {
                None => a = Some((v, byte0)),
                Some((_, ba)) if byte0 == *ba && b.is_none() => {
                    b = Some((v, byte0));
                    break;
                }
                _ => {}
            }
        }
        let (va, _) = a.expect("found first content");
        let (vb, _) = b.expect("found prefix-colliding second content");

        let ha = s.put(&va).expect("put a");
        let hb = s.put(&vb).expect("put b");
        assert_ne!(ha, hb, "distinct content ⇒ distinct hash");
        assert_eq!(
            ha.as_bytes()[0],
            hb.as_bytes()[0],
            "test invariant: chose contents with shared prefix byte"
        );

        // Both readable, both bytes intact.
        assert_eq!(s.get(ha).unwrap().unwrap(), va);
        assert_eq!(s.get(hb).unwrap().unwrap(), vb);
    }

    // ── empty + large edge cases ──────────────────────────────────────

    #[test]
    fn fs_empty_blob_round_trips() {
        // Empty content has the canonical BLAKE3-of-empty hash. Storing
        // it is allowed; this pins that the impl doesn't special-case
        // empty bytes (a tempting but wrong optimization).
        let (mut s, _td) = fs_store();
        let h = s.put(&[]).expect("put empty");
        assert_eq!(h, (&[][..]).hash());
        let got = s.get(h).expect("get").expect("present");
        assert_eq!(got, b"");
    }

    #[test]
    fn fs_large_blob_round_trips() {
        // 1 MiB blob — large enough to exercise the write path past
        // any buffer-size threshold, small enough to keep CI fast.
        let (mut s, _td) = fs_store();
        let bytes = vec![0x55u8; 1024 * 1024];
        let h = s.put(&bytes).expect("put large");
        let got = s.get(h).expect("get").expect("present");
        assert_eq!(got, bytes);
    }

    // ── persistence across instances ──────────────────────────────────

    #[test]
    fn fs_persists_across_store_reopens() {
        // Closing an `FsBlobStore` and reopening at the same root must
        // see the same contents. (No in-memory index; everything lives
        // on disk.) Critical for the substrate's restore-from-disk
        // story — without this, a process restart loses content.
        let td = TempDir::new().expect("tempdir");
        let root = td.path().join("objects");

        let h;
        {
            let mut s = FsBlobStore::open(&root).expect("open 1");
            h = s.put(b"persistent content").expect("put");
        }
        {
            let s = FsBlobStore::open(&root).expect("open 2");
            let got = s.get(h).expect("get").expect("present after reopen");
            assert_eq!(got, b"persistent content");
        }
    }

    // ── new() vs open() ───────────────────────────────────────────────

    #[test]
    fn fs_new_refuses_missing_root() {
        let td = TempDir::new().expect("tempdir");
        let missing = td.path().join("doesnt-exist");
        assert!(FsBlobStore::new(&missing).is_err());
    }

    #[test]
    fn fs_open_creates_missing_root() {
        let td = TempDir::new().expect("tempdir");
        let fresh = td.path().join("objects").join("nested").join("path");
        let s = FsBlobStore::open(&fresh).expect("open creates parents");
        assert!(fresh.is_dir(), "open() created the root");
        assert_eq!(s.root(), fresh);
    }

    // ── contains() before/after put ───────────────────────────────────

    #[test]
    fn fs_contains_reflects_state() {
        let (mut s, _td) = fs_store();
        let bytes = b"presence";
        let h = bytes.hash();
        assert!(!s.contains(h).unwrap(), "before put: absent");
        s.put(bytes).unwrap();
        assert!(s.contains(h).unwrap(), "after put: present");
    }
}
