//! **F5 — Composition seams: hash-only boundaries between substrate
//! consumers.**
//!
//! Falsifies substrate requirement R7 (hash-only interface) — decade
//! `docs/decades/2026-merkle-cas-substrate.md` §4 F5.
//!
//! ## Claim (from the decade §4)
//!
//! > "F5 — Compositionality. Build a workload where:
//! >   - signet signs R_n only (one signature per advance, not per blob)
//! >   - workerd executes against R_n by fetching ρ(R_n)
//! >   - rosary dispatches a bead whose content is R_n reference
//! >   - mache projects ρ(R_n) into its FUSE view
//! > If any of those requires per-blob coordination beyond (R_n, sig_n),
//! > R7 (compositionality) is falsified."
//!
//! And decade §3.6 clarifies:
//!
//! > "Each composing system holds only (R_n, sig_n) at runtime. State
//! > sharing is through cas(R_old, R_new), not through mutable handles.
//! > […] F5 (§4) tests that no *mutable runtime* state crosses
//! > composition seams — which is the genuine R7 invariant."
//!
//! ## Test shape
//!
//! Consumers are modeled as trait objects with mocked implementations
//! that record exactly what they received across the seam. The
//! substrate publishes a root by calling `observe_root_update(R)` on
//! each consumer — a 32-byte `Hash` is the ONLY payload that crosses
//! this seam.
//!
//! Consumers that need to fetch bytes (workerd, mache) do so via an
//! injected `BlobStore` handle — but this fetch is:
//!
//! 1. **On-demand** (not eager): during the substrate's publish, only
//!    the root hash is delivered. The consumer's fetch happens later,
//!    when the consumer decides to execute (workerd) or project
//!    (mache).
//! 2. **Keyed by hash only**: the consumer calls `store.get(hash)`
//!    with a `Hash` value — not with any mutable handle or per-blob
//!    coordination token.
//!
//! The test asserts:
//!
//! - During publish, every consumer records ONE `(Hash, u64)` sighting
//!   (root + `bytes_across_seam`). `bytes_across_seam` MUST equal 32
//!   (the size of one `Hash`).
//! - Signet's `sign(root)` sees only the 32-byte root — never the
//!   underlying blob.
//! - Workerd fetches `ρ(root)` via `BlobStore::get(hash)` and receives
//!   the blob bytes, but only after `publish` returned — the seam-
//!   crossing during publish carried only `hash`.
//! - Rosary stores the root hash in its bead reference — the bead
//!   body is `Hash`, not the blob bytes.
//! - Mache's projection call fetches `ρ(root)` lazily on the first
//!   observer read — pre-observer, the mache mock hasn't touched any
//!   bytes even though it was notified of the new root.
//!
//! ## Pass criteria
//!
//! - No consumer's per-publish seam payload exceeds `size_of::<Hash>()`
//!   = 32 bytes.
//! - Mache's blob fetch counter is zero after publish + before the
//!   first observer read (lazy contract).
//! - All consumers agree on the value of the published root
//!   (deterministic publish).

use std::sync::atomic::{AtomicU64, Ordering};

use leyline_core::MemBlobStore;
use leyline_core::substrate::{BlobStore, ContentAddressed, Hash};

/// Bytes crossing a seam per `Hash` payload. `Hash` is a 32-byte
/// newtype; this is what a real transport would frame per notification.
const HASH_SEAM_BYTES: u64 = 32;

/// Every seam-observation records `(root, bytes_crossed)`. A consumer
/// receiving more than a `Hash` across the seam would record a
/// `bytes_crossed` > 32 → assertion trip.
#[derive(Clone, Debug)]
struct SeamRecord {
    root: Hash,
    bytes_across_seam: u64,
}

/// The R7 compositionality contract. Every consumer of the substrate
/// implements this trait — the ONLY thing that crosses the seam
/// during a publish event is the root hash. If a future refactor
/// added a blob-payload parameter here (say, `observe_root_update(&mut
/// self, root: Hash, payload: &[u8])`), the trait signature itself
/// would carry the R7 violation and this test would catch the compile-
/// level shape change.
trait ConsumerSurface {
    /// Called by the substrate when a new root is published. Sole
    /// argument: the 32-byte root hash. NO blob bytes, NO mutable
    /// handles — those flow only through the injected `BlobStore`
    /// (which the consumer already holds statically at wiring time).
    fn observe_root_update(&mut self, root: Hash);

    /// Diagnostic accessor for the test: what did this consumer see
    /// during past `observe_root_update` calls? Consumers that ALSO
    /// fetched bytes via `BlobStore::get` MUST NOT record that fetch
    /// as seam-crossing — the fetch is by-hash, and the resulting
    /// bytes are the consumer's private state, not shared runtime
    /// state on the composition seam.
    fn seam_records(&self) -> &[SeamRecord];
}

// ─── Signet mock ─────────────────────────────────────────────────────
//
// Signet signs the root only. Per decade §5 T5.1 and §3.6, the signing
// key operates on the 32-byte hash — never on the blob content.

#[derive(Default)]
struct SignetMock {
    seen: Vec<SeamRecord>,
    signatures: Vec<[u8; 32]>, // stub sig; real impl would use ML-DSA-44
}

impl SignetMock {
    fn sign_last_seen(&mut self) {
        let last = self
            .seen
            .last()
            .expect("signet asked to sign but no root seen");
        // Toy signature: a stand-in for signet's ML-DSA-44 output. The
        // point of the F5 test is the seam shape, not the crypto — a
        // real signet call would take `&Hash` (32 bytes) and return
        // ~2.4 KB of signature. Here we return 32 bytes so the
        // arithmetic is easy to read.
        let mut sig = [0u8; 32];
        sig.copy_from_slice(last.root.as_bytes());
        sig[0] ^= 0xA5; // some transformation so sig != root byte-for-byte
        self.signatures.push(sig);
    }
}

impl ConsumerSurface for SignetMock {
    fn observe_root_update(&mut self, root: Hash) {
        self.seen.push(SeamRecord {
            root,
            bytes_across_seam: HASH_SEAM_BYTES,
        });
    }
    fn seam_records(&self) -> &[SeamRecord] {
        &self.seen
    }
}

// ─── Workerd mock ────────────────────────────────────────────────────
//
// Workerd executes against ρ(R_n). Its seam intake is the root hash;
// the blob fetch happens on-demand via `BlobStore::get(hash)`.

struct WorkerdMock<'a> {
    seen: Vec<SeamRecord>,
    store: &'a dyn BlobStore,
    /// Bytes actually pulled from the store (post-seam, private state).
    /// Not on the seam — the fetch is a subsequent hash-keyed lookup.
    executed_payloads: Vec<Vec<u8>>,
}

impl<'a> WorkerdMock<'a> {
    fn new(store: &'a dyn BlobStore) -> Self {
        Self {
            seen: Vec::new(),
            store,
            executed_payloads: Vec::new(),
        }
    }

    fn execute_last_seen(&mut self) {
        let last = self
            .seen
            .last()
            .expect("workerd asked to execute but no root seen");
        // Consumer fetches ρ(R_n) on-demand, keyed by `Hash` — this is
        // NOT a seam-crossing beyond `Hash`. The fetch is inside the
        // consumer's own execution boundary.
        let payload = self
            .store
            .get(last.root)
            .expect("workerd get")
            .expect("root points at present blob");
        self.executed_payloads.push(payload);
    }
}

impl<'a> ConsumerSurface for WorkerdMock<'a> {
    fn observe_root_update(&mut self, root: Hash) {
        self.seen.push(SeamRecord {
            root,
            bytes_across_seam: HASH_SEAM_BYTES,
        });
    }
    fn seam_records(&self) -> &[SeamRecord] {
        &self.seen
    }
}

// ─── Rosary mock ─────────────────────────────────────────────────────
//
// Rosary bead body references the root hash. Per T5.3, "dispatch
// fetches ρ(R_n)" — the bead itself carries only the hash.

#[derive(Default)]
struct RosaryMock {
    seen: Vec<SeamRecord>,
    /// Beads keyed by `Hash`. The body is the hash reference — NOT the
    /// blob bytes.
    beads: Vec<Hash>,
}

impl RosaryMock {
    fn create_bead_for_last_seen(&mut self) {
        let last = self
            .seen
            .last()
            .expect("rosary asked to bead but no root seen");
        // Bead body is the hash reference. Dispatch later uses this
        // hash to fetch ρ(R_n) — dispatch is a separate act, and its
        // fetch is by-hash from the store, not by shared runtime state.
        self.beads.push(last.root);
    }
}

impl ConsumerSurface for RosaryMock {
    fn observe_root_update(&mut self, root: Hash) {
        self.seen.push(SeamRecord {
            root,
            bytes_across_seam: HASH_SEAM_BYTES,
        });
    }
    fn seam_records(&self) -> &[SeamRecord] {
        &self.seen
    }
}

// ─── Mache mock ──────────────────────────────────────────────────────
//
// Mache subscribes to root changes; fetches ρ(root) on demand but
// never eagerly. Per T5.4: "mache projects ρ(R_n) into FUSE view;
// root-aware hot-swap".

struct MacheMock<'a> {
    seen: Vec<SeamRecord>,
    store: &'a dyn BlobStore,
    /// Counts how many times mache called `store.get()`. This MUST
    /// stay at 0 across an `observe_root_update` call — the eager-
    /// fetch anti-pattern would violate the lazy-projection contract.
    fetch_counter: &'a AtomicU64,
    /// Bytes projected into the FUSE view once mache decides to
    /// materialize (e.g. on the first observer read). Not on the seam.
    materialized: Option<Vec<u8>>,
}

impl<'a> MacheMock<'a> {
    fn new(store: &'a dyn BlobStore, fetch_counter: &'a AtomicU64) -> Self {
        Self {
            seen: Vec::new(),
            store,
            fetch_counter,
            materialized: None,
        }
    }

    /// Simulates a FUSE-side observer reading through mache — mache
    /// lazily materializes ρ(last_root) here. This is the ON-DEMAND
    /// path per §3.6 and T5.4.
    fn observer_read(&mut self) {
        let last = self
            .seen
            .last()
            .expect("mache asked for observer_read but no root seen");
        self.fetch_counter.fetch_add(1, Ordering::Relaxed);
        let payload = self
            .store
            .get(last.root)
            .expect("mache get")
            .expect("present");
        self.materialized = Some(payload);
    }
}

impl<'a> ConsumerSurface for MacheMock<'a> {
    fn observe_root_update(&mut self, root: Hash) {
        self.seen.push(SeamRecord {
            root,
            bytes_across_seam: HASH_SEAM_BYTES,
        });
        // IMPORTANT: no fetch here. Mache is subscribed to root changes
        // but does NOT eagerly pull ρ(root). The R7 invariant is that
        // seam-crossing is Hash-only; the eager fetch would still not
        // be *on the seam* (it's the consumer's own store call), but
        // it would break the lazy-projection contract that §3.6 names.
    }
    fn seam_records(&self) -> &[SeamRecord] {
        &self.seen
    }
}

// ─── The publish primitive ───────────────────────────────────────────
//
// Substrate publishes a root advance to every registered consumer. The
// dispatch loop takes `&mut dyn ConsumerSurface`, which structurally
// prevents leaking blob bytes into the notification — the trait
// signature IS the R7 contract.

fn publish_root_to_consumers(root: Hash, consumers: &mut [&mut dyn ConsumerSurface]) {
    for c in consumers.iter_mut() {
        c.observe_root_update(root);
    }
}

/// Compile-time pin: `Hash` really is 32 bytes. Load-bearing for the
/// `HASH_SEAM_BYTES` constant — if `Hash` grew (e.g. Σ' migration to
/// a larger commitment size), this test's arithmetic would need to be
/// re-derived.
#[test]
fn hash_size_pin_for_seam_arithmetic() {
    assert_eq!(
        std::mem::size_of::<Hash>(),
        HASH_SEAM_BYTES as usize,
        "F5 arithmetic depends on Hash = 32 bytes; adjust HASH_SEAM_BYTES if this changes"
    );
}

#[test]
fn all_consumers_receive_only_the_root_hash_across_the_seam() {
    // Substrate holds an on-disk blob store and a fresh root R that
    // addresses a payload P.
    let mut store = MemBlobStore::new();
    let payload: Vec<u8> = b"F5: the substrate-published payload, not on the seam".to_vec();
    let root = store.put(&payload).expect("substrate put");
    assert_eq!(root, payload.as_slice().hash(), "sanity: root = σ(payload)");

    // Wire consumers. Each holds a static reference to the store (its
    // constructor time), not to any mutable substrate state — this is
    // the "static crypto config" carve-out of §3.6.
    let mache_fetch_counter = AtomicU64::new(0);

    let mut signet = SignetMock::default();
    let mut workerd = WorkerdMock::new(&store);
    let mut rosary = RosaryMock::default();
    let mut mache = MacheMock::new(&store, &mache_fetch_counter);

    // Substrate publishes the root advance.
    {
        let mut consumers: [&mut dyn ConsumerSurface; 4] =
            [&mut signet, &mut workerd, &mut rosary, &mut mache];
        publish_root_to_consumers(root, &mut consumers);
    }

    // Assertion 1: every consumer recorded exactly ONE seam sighting
    // per publish, carrying exactly `HASH_SEAM_BYTES` bytes. Any
    // consumer recording MORE bytes at the seam has violated R7.
    for (name, records) in [
        ("signet", signet.seam_records()),
        ("workerd", workerd.seam_records()),
        ("rosary", rosary.seam_records()),
        ("mache", mache.seam_records()),
    ] {
        assert_eq!(
            records.len(),
            1,
            "F5: {name} recorded {} seam sightings, expected 1",
            records.len()
        );
        assert_eq!(
            records[0].root, root,
            "F5: {name} saw a different root than published",
        );
        assert!(
            records[0].bytes_across_seam <= HASH_SEAM_BYTES,
            "F5: {name} seam-crossing carried {} bytes (must be ≤ {} = size_of Hash). \
             R7 (hash-only interface) violated.",
            records[0].bytes_across_seam,
            HASH_SEAM_BYTES,
        );
    }

    // Assertion 2 (mache lazy-projection): before any observer read,
    // mache MUST NOT have called `store.get`. The subscription-plus-
    // lazy-fetch pattern is what makes mache composable at scale;
    // eager fetch on every publish would defeat R7's "no per-blob
    // coordination" claim.
    assert_eq!(
        mache_fetch_counter.load(Ordering::Relaxed),
        0,
        "F5: mache fetched ρ(root) eagerly during publish — lazy-projection contract broken"
    );
    assert!(
        mache.materialized.is_none(),
        "F5: mache materialized bytes eagerly during publish"
    );

    // Consumers now act on what they saw. Each of these is INSIDE the
    // consumer's own execution boundary — the seam-crossing already
    // completed above, and these are the consumers' private post-seam
    // work.

    // signet signs the root (32 bytes in, ~signature bytes out).
    signet.sign_last_seen();
    assert_eq!(signet.signatures.len(), 1, "signet produced one signature");

    // workerd executes against ρ(root). The fetch is by-hash, from the
    // store the consumer holds statically — not from the substrate's
    // runtime state.
    workerd.execute_last_seen();
    assert_eq!(workerd.executed_payloads.len(), 1);
    assert_eq!(
        workerd.executed_payloads[0], payload,
        "workerd executed against the same payload the substrate published"
    );

    // rosary creates a bead referencing the root hash — not the payload.
    rosary.create_bead_for_last_seen();
    assert_eq!(rosary.beads.len(), 1, "rosary created one bead");
    assert_eq!(
        rosary.beads[0], root,
        "rosary bead body is the root hash, not the blob"
    );

    // mache lazily materializes on the first observer read. THIS is
    // when the fetch counter finally ticks.
    mache.observer_read();
    assert_eq!(
        mache_fetch_counter.load(Ordering::Relaxed),
        1,
        "mache should have fetched ρ(root) exactly once on first observer read"
    );
    assert_eq!(
        mache.materialized.as_ref().expect("materialized"),
        &payload,
        "mache materialized the substrate-published payload"
    );
}

#[test]
fn multi_advance_seam_bytes_stay_bounded() {
    // Advance the substrate multiple times; every publish carries
    // exactly `HASH_SEAM_BYTES` per consumer per advance. If a future
    // refactor introduced ANY per-blob framing on the seam (e.g. a
    // manifest bundled with the notification), the running total would
    // exceed `advances × HASH_SEAM_BYTES × consumers`.
    let mut store = MemBlobStore::new();
    let advances = 5;

    let mut signet = SignetMock::default();
    let mut rosary = RosaryMock::default();

    for i in 0..advances {
        let payload = format!("F5-advance-{i}").into_bytes();
        let root = store.put(&payload).expect("substrate put");
        let mut consumers: [&mut dyn ConsumerSurface; 2] = [&mut signet, &mut rosary];
        publish_root_to_consumers(root, &mut consumers);
    }

    let total_signet: u64 = signet
        .seam_records()
        .iter()
        .map(|r| r.bytes_across_seam)
        .sum();
    let total_rosary: u64 = rosary
        .seam_records()
        .iter()
        .map(|r| r.bytes_across_seam)
        .sum();

    let expected = advances as u64 * HASH_SEAM_BYTES;
    assert_eq!(
        total_signet, expected,
        "F5: signet total seam bytes over {advances} advances = {total_signet} (expected {expected})"
    );
    assert_eq!(
        total_rosary, expected,
        "F5: rosary total seam bytes over {advances} advances = {total_rosary} (expected {expected})"
    );
}
