//! T1.1 stub: Σ — Merkle-CAS substrate types.
//!
//! See `docs/decades/2026-merkle-cas-substrate.md` for the formal substrate
//! definition. This module declares the type surface only — no behavior
//! is implemented here. Implementations land in T2 (controller migration)
//! and T3 (content-addressed blob store).
//!
//! The substrate is the six-tuple Σ = (𝓥, 𝓒, ρ, σ, R, S) where:
//!
//! | Symbol | This module                     | Meaning                         |
//! |--------|---------------------------------|---------------------------------|
//! | 𝓥     | `Vec<u8>` / `&[u8]`              | Content vocabulary (raw bytes)  |
//! | 𝓒     | [`Hash`]                         | Content addresses (256 bits)    |
//! | ρ      | [`BlobStore::get`]               | Content-addressed retrieval     |
//! | σ      | [`ContentAddressed::hash`]       | Content addressing function     |
//! | R      | [`RootPointer`]                  | Atomic root pointer             |
//! | S      | [`RootSigner`]                   | Signature scheme over roots     |
//!
//! The substrate axioms are documented per-item below. Bead T1.3 turns
//! each axiom into an executable falsification test.
//!
//! Bead: ley-line-open-9e3a5f (T1.1)
//! Decade: ley-line-open-9d30ac

use anyhow::Result;

/// Content address. 256-bit cryptographic hash of a blob.
///
/// Concrete hash function is BLAKE3 (decided in T1.1 design). The choice
/// is captured here so all callers share one address space — mixing hash
/// functions inside a single substrate breaks (DET) and (CR) at the
/// composition boundary.
///
/// **Axioms enforced by this type:**
/// - **(DET)** Determinism: implementing `ContentAddressed` for the same
///   bytes on any host produces the same `Hash`.
/// - **(CR)** Collision resistance: 256-bit BLAKE3 gives `2^{-128}`
///   collision probability under uniform sampling. Production use
///   assumes the bound holds; cryptographic break of BLAKE3 invalidates
///   the substrate's integrity guarantees and requires a hash migration.
/// - **(TR)** Second-preimage resistance: see `Hash::ASSUMED_BITS`.
///
/// Constructed only via [`ContentAddressed::hash`] or
/// [`Hash::from_bytes`]. Cannot be forged — but BEWARE: `from_bytes`
/// admits any 32-byte value. Verify hashes against a known source
/// (signed root, trusted store) before treating them as authoritative.
#[derive(Copy, Clone, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct Hash([u8; 32]);

impl Hash {
    /// Bits of preimage / collision resistance assumed by the substrate.
    /// Decreases by 1 for collision attacks; user code should plan for
    /// `Hash::ASSUMED_BITS - 1` headroom on cryptanalytic margin.
    pub const ASSUMED_BITS: u32 = 256;

    /// Zero hash. Used as the sentinel for "no current root" in
    /// [`RootPointer`] before the first advance.
    ///
    /// **Invariant:** No content `v ∈ 𝓥` should ever produce
    /// `ContentAddressed::hash(v) == Hash::ZERO` in practice. The
    /// 2^{-256} probability is below the substrate's assumed adversary
    /// budget; if it does occur, treat as a hash collision incident.
    pub const ZERO: Hash = Hash([0u8; 32]);

    /// Construct a `Hash` from raw bytes. Does NOT verify that the bytes
    /// are the actual hash of any content. Use [`ContentAddressed::hash`]
    /// to compute a hash from content; use this constructor only when
    /// deserializing a hash from a trusted source (signed root, on-disk
    /// header, peer-supplied reference that will be verified before use).
    pub const fn from_bytes(bytes: [u8; 32]) -> Hash {
        Hash(bytes)
    }

    /// Raw bytes view. The caller must NOT mutate these bytes — `Hash`
    /// equality is the substrate's identity primitive.
    pub const fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl std::fmt::Debug for Hash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Hex-prefix display, like git short hashes.
        for byte in &self.0[..8] {
            write!(f, "{byte:02x}")?;
        }
        write!(f, "…")
    }
}

impl std::fmt::Display for Hash {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in &self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

/// `σ : 𝓥 → 𝓒` — content addressing function.
///
/// Any value implementing this trait can be hashed into the substrate's
/// address space. Implementors must satisfy:
///
/// - **(CA)** Round-trip: if a `BlobStore` accepts `v` and returns its
///   hash `h`, then `store.get(h) == Some(v)`.
/// - **(DET)** Determinism: two values with identical serialized bytes
///   must hash to the same `Hash`. **Implementations of this trait for
///   structured types must canonicalize** (sorted keys, fixed field
///   order, NULL vs missing distinction) before hashing.
///
/// The default impl for `[u8]` and `Vec<u8>` (in T1.1 implementation
/// land) hashes the raw bytes via BLAKE3.
pub trait ContentAddressed {
    /// Compute `σ(self)`.
    fn hash(&self) -> Hash;
}

/// `ρ : 𝓒 → 𝓥 ∪ {⊥}` — content-addressed retrieval, plus immutable
/// insertion.
///
/// **Axioms enforced by implementations:**
///
/// - **(CA)** `put(v).then(get) == Some(v)`. Round-trip.
/// - **(IM)** Once `get(h)` returns `Some(_)`, it returns the same
///   `Some(_)` forever (or `None` after explicit GC of unreachable
///   blobs — see T3.3). No in-place mutation of stored blobs.
/// - **(CR)** Collision resistance is delegated to `Hash`, not the
///   store: a `BlobStore` indexed by `Hash` cannot store two distinct
///   bytes under the same key.
///
/// Implementations: `FsBlobStore` (T3.1), `MemBlobStore` (testing).
pub trait BlobStore {
    /// Insert `bytes` into the store. Returns the content address.
    /// Idempotent: inserting the same bytes twice returns the same hash
    /// and does not duplicate storage.
    fn put(&mut self, bytes: &[u8]) -> Result<Hash>;

    /// Retrieve the bytes stored under `h`, or `None` if absent.
    /// Implementations MUST verify `σ(retrieved) == h` before returning
    /// (T2.3 verify-on-read pattern). A returned `Some(v)` carries an
    /// implicit "the substrate vouches for `σ(v) == h`" guarantee.
    fn get(&self, h: Hash) -> Result<Option<Vec<u8>>>;

    /// True iff `h` is in the store. Useful for "should I fetch this
    /// over the network?" decisions without paying the full read.
    fn contains(&self, h: Hash) -> Result<bool>;
}

/// `R : Var<𝓒>` — atomic root pointer.
///
/// Single-CAS advancement primitive: `R := cas(R_old, R_new)` succeeds
/// iff the current value equals `R_old` at the moment of compare.
///
/// **Axioms enforced by implementations:**
///
/// - **(CAS)** `cas` is atomic across concurrent observers — at most
///   one of N concurrent `cas(R_old, R_*)` calls succeeds when all
///   start from the same observed `R_old`.
/// - **(IM)** Past values of `R` are NOT preserved by this type.
///   History retention is the [`BlobStore`]'s job (every prior `R_i`
///   has its blob in the store; the store retains them per its GC
///   policy).
///
/// Implementations: `MmapRootPointer` (T2.4 — replaces the existing
/// `Controller.generation` field), in-memory `AtomicRootPointer` for
/// testing.
pub trait RootPointer {
    /// Read the current root.
    fn current(&self) -> Hash;

    /// Atomic compare-and-swap. Returns `Ok(())` on success, or
    /// `Err(observed)` carrying the value that was actually present
    /// at compare time (so the loser can rebase against it without a
    /// second read).
    fn cas(&self, expected: Hash, new: Hash) -> std::result::Result<(), Hash>;
}

/// `S : (𝓒, SK) → Sig`, `(𝓒, Sig, PK) → 𝟚` — signature scheme over
/// content addresses.
///
/// **Axioms enforced by implementations:**
///
/// - **(SIG)** `verify(h, sign(h, sk), pk) = true` iff `pk` is the
///   public key matching `sk`; `false` otherwise.
/// - The signing key never sees raw blob content — it sees only `Hash`.
///   This is load-bearing: it lets composition seams (T5.1: signet
///   signs roots) operate on a 32-byte value rather than the entire
///   serialized arena.
///
/// Implementations: signet integration (T5.1), in-memory Ed25519 for
/// testing.
pub trait RootSigner {
    /// Opaque signature type. 64 bytes for Ed25519 in practice.
    type Signature;
    /// Opaque public key type. 32 bytes for Ed25519 in practice.
    type PublicKey;

    /// Sign a content address. The signing key is encapsulated by the
    /// implementor (e.g. held in HSM, or a held-in-process `SecretKey`).
    fn sign(&self, h: Hash) -> Result<Self::Signature>;

    /// Verify that `sig` is a valid signature of `h` under `pk`.
    /// Static method: verification doesn't require any state held by
    /// the signer.
    fn verify(h: Hash, sig: &Self::Signature, pk: &Self::PublicKey) -> bool;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Type-level sanity: `Hash` is `Copy` and `Eq`, the foundational
    /// substrate identity. A future refactor that boxed `Hash` for an
    /// "extensibility" reason would silently break every CAS comparison
    /// downstream — pin the size and the marker traits.
    #[test]
    fn hash_is_pod_sized() {
        assert_eq!(std::mem::size_of::<Hash>(), 32);
        assert_eq!(std::mem::align_of::<Hash>(), 1);
        // Compile-time assertion that Hash is Copy + Eq + Hash.
        fn assert_traits<T: Copy + Eq + std::hash::Hash + Ord>() {}
        assert_traits::<Hash>();
    }

    /// `Hash::ZERO` must be the all-zeros sentinel — readers and
    /// writers compare against `Hash::ZERO` to mean "no root yet."
    /// A future refactor that changed the zero-bytes invariant would
    /// silently de-sync every reader.
    #[test]
    fn zero_hash_is_zero_bytes() {
        assert_eq!(Hash::ZERO.as_bytes(), &[0u8; 32]);
    }

    /// `from_bytes` round-trips. Pin both directions so a refactor that
    /// e.g. introduced byte-order swapping inside `from_bytes` would
    /// surface here.
    #[test]
    fn hash_from_bytes_round_trip() {
        let bytes = [
            0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef,
            0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77,
            0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
            0xfe, 0xdc, 0xba, 0x98, 0x76, 0x54, 0x32, 0x10,
        ];
        let h = Hash::from_bytes(bytes);
        assert_eq!(h.as_bytes(), &bytes);
    }

    /// Display format is full hex (64 chars) — used in error messages,
    /// signed-root logging, and on-disk debug dumps. Pin so a future
    /// truncation refactor wouldn't silently change ergonomics.
    #[test]
    fn hash_display_is_full_hex() {
        let h = Hash::from_bytes([0u8; 32]);
        let s = format!("{h}");
        assert_eq!(s.len(), 64);
        assert_eq!(s, "0".repeat(64));
    }

    /// Debug format is short-hex (8 bytes / 16 chars + ellipsis), like
    /// git short SHAs. Quality-of-life pin for log-grep workflows.
    #[test]
    fn hash_debug_is_short_hex_with_ellipsis() {
        let h = Hash::from_bytes([0xab; 32]);
        let s = format!("{h:?}");
        assert!(s.starts_with("abababab"));
        assert!(s.ends_with('…'));
    }
}
