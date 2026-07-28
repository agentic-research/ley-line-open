//! Declared decompositions — the tagged fold over a partition (ADR-0032 D2).
//!
//! Σ names whole values (`σ : 𝓥 → 𝓒`) but says nothing about how a value is
//! cut into parts, so every layer that needed parts invented its own cut and
//! the resulting roots are *incommensurable* — hashes over different,
//! undeclared partitions of the same bytes. This module supplies the missing
//! operator: an address is a fold over a **declared** decomposition.
//!
//! ```text
//! A(spec, children) = BLAKE3_derive_key(PARTITION_CONTEXT,
//!                       domain ‖ canonVersion ‖ |scheme| ‖ scheme
//!                       ‖ |params| ‖ params ‖ n ‖ (addrᵢ ‖ aᵢ ‖ bᵢ)…)
//! ```
//!
//! Two properties are load-bearing, both pinned by tests below:
//!
//! * **Domain separation.** The fold is keyed, not a plain BLAKE3.
//!   [`crate::substrate::RootSigner::sign`] accepts a bare 32-byte
//!   [`Hash`] with no notion of what it addresses, so an untagged fold would
//!   be indistinguishable from a content address under a shared key — the
//!   same argument [`crate::head_digest`] makes for the head triple.
//! * **Injectivity.** Every variable-length field is length-prefixed, so the
//!   address commits to the decomposition rather than to the concatenation of
//!   its parts. `Head.rootHash` was the motivating defect (bead `b64505`): it
//!   hashed `source ‖ ast ‖ bindings`, so two segment splits with equal
//!   concatenated bytes shared a root. It now folds through this operator.
//! * **Set canonicality.** For `ChunkSet` / `RowSet` the address is a function
//!   of the multiset of `(addr, a, b)` triples: entries are sorted wholesale
//!   and enumeration order — and only enumeration order — is erased. A set has
//!   no order, so folding it in would let a producer mint unlimited distinct
//!   addresses for one set; framing, by contrast, is scheme-defined content of
//!   the declared decomposition and IS committed. A prior revision dropped it,
//!   which collided materially different decompositions into one address —
//!   cloister's `glob-closure/v1` RowSet framing was the downstream casualty
//!   (bead `b67a73` regression).
//!
//! **Deliberately absent: a hash-algorithm field.** Multihash-style agility is
//! the one field every self-describing format adds and the one Σ must not —
//! `substrate.rs` locks BLAKE3 and warns that mixing functions breaks (DET)
//! and (CR) *at the composition boundary*, which is precisely where this
//! operator lives. A scheme may version itself (`"cdc/gearhash/v2"`); it may
//! not choose a different `σ`.
//!
//! **No capnp schema yet, on purpose.** ADR-0014 fixes ordinals permanently —
//! fields are appended, never renamed or removed. Nothing crosses a runtime
//! boundary with a `PartitionSpec` today, and freezing a wire shape before it
//! has a producer is how you end up maintaining a hole. The first real
//! consumer is a receiver that must RE-DERIVE a partition rather than compare
//! a root — cloister's skip attestation resolving a `spec_digest`, and the
//! disclosure-stream bundling. Tracked as bead `ley-line-open-e1977b`, which
//! lands the capnp family and the Go bindings together so the two runtimes
//! cannot drift.
//!
//! Bead `ley-line-open-b67a73`. ADR: `docs/adr/0032-declared-decompositions.md`.

use crate::substrate::Hash;

/// Which kind of decomposition the entries describe.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Domain {
    /// Contiguous, ordered byte intervals that tile `[0, len)`.
    ByteStream,
    /// An unordered set of parts, canonically ordered by `(addr, a, b)`.
    ChunkSet,
    /// Logical records, canonically ordered by `(addr, a, b)`.
    RowSet,
}

/// One child of a fold: its address plus its framing.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct Entry {
    /// σ of the child.
    pub addr: Hash,
    /// Framing lo — byte offset for `ByteStream`, else scheme-defined.
    pub a: u64,
    /// Framing hi — byte length for `ByteStream`, else scheme-defined.
    pub b: u64,
}

/// A declared decomposition: what was cut, under what scheme.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PartitionSpec {
    pub domain: Domain,
    /// Scheme identifier, e.g. `"cdc/gearhash/v1"`.
    pub scheme: String,
    /// Canonical scheme parameters, so a receiver can re-derive the cut.
    pub params: Vec<u8>,
    /// Canonicalization version for structured leaves.
    pub canon_version: u32,
}

/// Domain-separation context for the partition fold.
///
/// Protocol-visible and versioned, like [`crate::head_digest::HEAD_DIGEST_CONTEXT`]:
/// changing this string invalidates every address ever produced, so a change
/// means `v2`, not an edit.
///
/// # Why the `b67a73` framing fix did NOT bump this to `v2`
///
/// Recorded rather than left implicit, because the opposite conclusion is
/// defensible and a reader will reasonably ask.
///
/// This constant is the fold ALGORITHM's version. `PartitionSpec.canon_version`
/// is not — that is a caller-supplied field naming the *scheme's*
/// canonicalization, so it cannot express "which fold implementation produced
/// this address" and bumping it would misattribute an LLO change to every
/// downstream scheme.
///
/// So the framing fix changed the algorithm without changing its version tag,
/// which is normally exactly the hazard this constant exists to prevent: one
/// tag denoting two algorithms, silently disagreeing. It is acceptable here for
/// one checkable reason — **the buggy fold reached no tagged release**. The
/// eraser (`ad272d3`) is an ancestor of no tag (`git tag --contains ad272d3` is
/// empty; it is not an ancestor of `v0.11.3`), so no consumer could pull a
/// build that produced framing-erased set addresses except by pinning `main`
/// inside a ~24h window.
///
/// Bumping to `v2` would additionally invalidate every `ByteStream` address,
/// which the fix provably does not move (`byte_stream_address_pin` holds a
/// known-answer vector computed at the pre-fix revision). Paying a
/// whole-family invalidation to version away a state nobody could obtain is
/// the wrong trade.
///
/// **If a future fold change ships in a tag, this MUST bump.** The argument
/// above is contingent on the never-released fact, not a general licence to
/// change the algorithm in place.
pub const PARTITION_CONTEXT: &str = "leyline partition fold v1";

impl Domain {
    /// Stable wire tag. Values are protocol-visible — append, never renumber.
    const fn tag(self) -> u8 {
        match self {
            Domain::ByteStream => 1,
            Domain::ChunkSet => 2,
            Domain::RowSet => 3,
        }
    }

    /// Whether entry *enumeration order* is part of this domain's identity.
    ///
    /// For an interval domain it is — the sequence of offsets and lengths *is*
    /// the decomposition. For a set domain it is not: a set has no order, so
    /// folding enumeration order in would let a producer mint unlimited
    /// distinct addresses for one set — (DET) failing at exactly the layer
    /// this type exists to make trustworthy. Framing is committed in BOTH
    /// cases: it is scheme-defined content of the declared decomposition, not
    /// an artifact of enumeration (see the decision comment in [`PartitionSpec::address`]).
    const fn is_ordered(self) -> bool {
        matches!(self, Domain::ByteStream)
    }
}

impl PartitionSpec {
    /// `A(spec, children)` — the tagged fold of ADR-0032 D2.
    ///
    /// Every variable-length field is length-prefixed, so the address commits
    /// to the *decomposition* rather than to the concatenation of its parts.
    /// That is what makes two different field splits distinguishable.
    ///
    /// Ordered domains fold entries in the given order; set domains
    /// canonicalize — entries sorted wholesale by `(addr, a, b)` — so their
    /// address is a function of the multiset of entries rather than of how it
    /// was enumerated. Framing is committed in both cases.
    pub fn address(&self, entries: &[Entry]) -> Hash {
        let mut hasher = blake3::Hasher::new_derive_key(PARTITION_CONTEXT);
        hasher.update(&[self.domain.tag()]);
        hasher.update(&self.canon_version.to_le_bytes());
        hasher.update(&(self.scheme.len() as u64).to_le_bytes());
        hasher.update(self.scheme.as_bytes());
        hasher.update(&(self.params.len() as u64).to_le_bytes());
        hasher.update(&self.params);
        hasher.update(&(entries.len() as u64).to_le_bytes());

        if self.domain.is_ordered() {
            for entry in entries {
                hasher.update(entry.addr.as_bytes());
                hasher.update(&entry.a.to_le_bytes());
                hasher.update(&entry.b.to_le_bytes());
            }
        } else {
            // Set domains canonicalize by sorting whole entries — the
            // multiset commitment is over `(addr, a, b)` triples, with
            // enumeration order (and only enumeration order) erased.
            //
            // DECISION (bead b67a73 regression, cloister probe at 39e1e9b4):
            // order-insensitivity is deliberate — a set has no order, so
            // folding enumeration order would be a malleability slot — but
            // framing is part of the DECLARED decomposition and must be
            // committed (ADR-0032's operator folds `(addrᵢ, frameᵢ)` pairs;
            // it is silent on set-domain ordering, which is why sorting is a
            // canonicalization choice made here, not in the ADR). An earlier
            // revision dropped framing on the theory that it "carries no
            // defined meaning" in set domains; `Entry` documents it as
            // scheme-defined, and downstream schemes define it — cloister's
            // `glob-closure/v1` — so dropping it collided materially
            // different decompositions into one address. A scheme that
            // assigns framing no meaning must canonicalize it to zero; the
            // fold cannot know which fields a scheme uses, so it commits all
            // of them, and a garnished sender is caught by re-derivation
            // (bead b67a73 acceptance criterion 2), not by erasure.
            let mut sorted: Vec<&Entry> = entries.iter().collect();
            sorted.sort_unstable_by_key(|e| (e.addr, e.a, e.b));
            for entry in sorted {
                hasher.update(entry.addr.as_bytes());
                hasher.update(&entry.a.to_le_bytes());
                hasher.update(&entry.b.to_le_bytes());
            }
        }

        Hash::from_bytes(*hasher.finalize().as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn h(b: u8) -> Hash {
        Hash::from_bytes([b; 32])
    }

    fn spec(domain: Domain) -> PartitionSpec {
        PartitionSpec {
            domain,
            scheme: "test/scheme/v1".into(),
            params: vec![],
            canon_version: 1,
        }
    }

    /// ADR-0032 D2: the `domain` field must be load-bearing. An interval fold
    /// and a set fold over IDENTICAL child addresses must not alias — otherwise
    /// a CDC manifest and a sheaf-block over the same chunks share an address
    /// and a verifier cannot tell which claim was made.
    #[test]
    fn interval_and_set_folds_over_identical_children_differ() {
        let entries = [
            Entry {
                addr: h(1),
                a: 0,
                b: 10,
            },
            Entry {
                addr: h(2),
                a: 10,
                b: 10,
            },
        ];

        let interval = spec(Domain::ByteStream).address(&entries);
        let set = spec(Domain::ChunkSet).address(&entries);

        assert_ne!(
            interval, set,
            "an interval fold and a set fold over identical children MUST NOT alias"
        );
    }

    /// The scheme tag must be bound INTO the address, not carried beside it.
    /// A label that rides adjacent to a digest can disagree with the bytes
    /// while everything still "verifies" — cloister's `build-cache/v1` labels
    /// BLAKE3 hex with an OCI `sha256:` prefix and is exactly this failure.
    #[test]
    fn scheme_is_committed_to_the_address() {
        let e = [Entry {
            addr: h(1),
            a: 0,
            b: 4,
        }];
        let mut other = spec(Domain::ChunkSet);
        other.scheme = "test/other/v1".into();

        assert_ne!(spec(Domain::ChunkSet).address(&e), other.address(&e));
    }

    /// Params describe how to re-derive the cut. A receiver that cannot
    /// distinguish two parameterizations cannot falsify a lying sender.
    #[test]
    fn params_are_committed_to_the_address() {
        let e = [Entry {
            addr: h(1),
            a: 0,
            b: 4,
        }];
        let mut other = spec(Domain::ChunkSet);
        other.params = vec![0xff];

        assert_ne!(spec(Domain::ChunkSet).address(&e), other.address(&e));
    }

    /// Field boundaries must be unambiguous. This is the `Head.rootHash`
    /// defect class (bead b64505) stated as a test: hashing a concatenation
    /// without framing lets two different field splits share an address.
    #[test]
    fn scheme_and_params_boundary_is_unambiguous() {
        let e: [Entry; 0] = [];
        let mut left = spec(Domain::ChunkSet);
        left.scheme = "ab".into();
        left.params = b"c".to_vec();

        let mut right = spec(Domain::ChunkSet);
        right.scheme = "a".into();
        right.params = b"bc".to_vec();

        assert_ne!(
            left.address(&e),
            right.address(&e),
            "('ab','c') and ('a','bc') concatenate identically; the address MUST distinguish them"
        );
    }

    /// The address commits to the partition, not merely to the multiset of
    /// child addresses. Reordering entries is a different decomposition.
    #[test]
    fn entry_order_is_committed_to_the_address() {
        let s = spec(Domain::ByteStream);
        let fwd = [
            Entry {
                addr: h(1),
                a: 0,
                b: 5,
            },
            Entry {
                addr: h(2),
                a: 5,
                b: 5,
            },
        ];
        let rev = [fwd[1], fwd[0]];

        assert_ne!(s.address(&fwd), s.address(&rev));
    }

    /// Same children, different framing, is a different decomposition — one
    /// cut at 5 bytes, one at 3. Without framing in the fold they alias.
    #[test]
    fn framing_is_committed_to_the_address() {
        let s = spec(Domain::ByteStream);
        let a = [Entry {
            addr: h(1),
            a: 0,
            b: 5,
        }];
        let b = [Entry {
            addr: h(1),
            a: 0,
            b: 3,
        }];

        assert_ne!(s.address(&a), s.address(&b));
    }

    /// Domain separation, mirroring `head_digest`'s pin. `RootSigner::sign`
    /// takes a bare 32-byte Hash whatever its provenance, so an untagged fold
    /// would be indistinguishable from a content address under a shared key.
    #[test]
    fn address_is_domain_separated_from_plain_blake3() {
        let e = [Entry {
            addr: h(7),
            a: 0,
            b: 1,
        }];
        let tagged = spec(Domain::ChunkSet).address(&e);

        let mut plain = blake3::Hasher::new();
        plain.update(e[0].addr.as_bytes());
        plain.update(&e[0].a.to_le_bytes());
        plain.update(&e[0].b.to_le_bytes());

        assert_ne!(
            tagged.as_bytes(),
            plain.finalize().as_bytes(),
            "the fold must be keyed, not a bare hash of its children"
        );
    }

    /// A SET domain's address must be a function of the SET — not of the
    /// order it happened to be enumerated in. Otherwise two producers
    /// describing the identical entry set disagree on its address, which
    /// breaks (DET) at exactly the layer this type exists to make
    /// trustworthy. Entries here carry distinct framing, so this also pins
    /// that order-insensitivity survives the framing commitment: the
    /// canonicalization sorts whole entries, it does not fold enumeration
    /// order back in.
    ///
    /// Found by adversarial review of `Domain::RowSet` on the day it shipped.
    #[test]
    fn set_domains_ignore_entry_order() {
        let s = spec(Domain::RowSet);
        let fwd = [
            Entry {
                addr: h(1),
                a: 0,
                b: 7,
            },
            Entry {
                addr: h(2),
                a: 7,
                b: 9,
            },
        ];
        let rev = [fwd[1], fwd[0]];

        assert_eq!(
            s.address(&fwd),
            s.address(&rev),
            "a set is unordered; enumeration order must not change its address"
        );
    }

    /// Entries that share an address but differ in framing are distinct
    /// members of the declared decomposition — the multiset is of whole
    /// `(addr, a, b)` triples, not of addresses. Before the fix these
    /// collapsed to one sorted address list of length two.
    #[test]
    fn set_domains_distinguish_same_address_different_framing() {
        let s = spec(Domain::ChunkSet);
        let two_frames = [
            Entry {
                addr: h(1),
                a: 0,
                b: 1,
            },
            Entry {
                addr: h(1),
                a: 0,
                b: 2,
            },
        ];
        let dup_frame = [
            Entry {
                addr: h(1),
                a: 0,
                b: 1,
            },
            Entry {
                addr: h(1),
                a: 0,
                b: 1,
            },
        ];

        assert_ne!(
            s.address(&two_frames),
            s.address(&dup_frame),
            "the multiset is of (addr, a, b) triples, not of addresses"
        );
    }

    /// The interval fold's preimage is untouched by the set-domain framing
    /// fix. This is a known-answer pin computed at the PRE-fix revision
    /// (main @ 33c46dd): if it moves, every persisted `ByteStream` address —
    /// including `cmd_parse`'s segment root — has been invalidated, which is
    /// a generation-lineage event, not a refactor.
    #[test]
    fn byte_stream_address_pin() {
        let s = spec(Domain::ByteStream);
        let e = [
            Entry {
                addr: h(1),
                a: 0,
                b: 5,
            },
            Entry {
                addr: h(2),
                a: 5,
                b: 5,
            },
        ];
        let hex: String = s
            .address(&e)
            .as_bytes()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(
            hex, "dedab6dfd1ecdcab242d1a050c2e2d216a31c4f5c6dfa10133775fec496c0ad9",
            "ByteStream preimage moved — persisted interval addresses invalidated"
        );
    }

    /// Set-domain framing is part of the declared decomposition and MUST be
    /// committed (ADR-0032 D2: the operator folds `(addrᵢ, frameᵢ)` pairs).
    /// `Entry.a`/`b` are scheme-defined for set domains, and downstream
    /// schemes do define them — cloister's `glob-closure/v1` (RowSet) pins
    /// exactly this property in cloister-cas's
    /// `address_matches_upstream_fold_for_a_known_spec`. Dropping framing
    /// collided every framing variant of one address multiset into a single
    /// address: a producer could present materially different decompositions
    /// under one digest, the inverse of the malleability it was meant to stop.
    ///
    /// Each assertion below is a row of cloister's probe table (bead
    /// `b67a73` regression, found at rev 39e1e9b4) — every row collided
    /// before the fix.
    #[test]
    fn set_domains_commit_entry_framing() {
        for domain in [Domain::ChunkSet, Domain::RowSet] {
            let s = spec(domain);
            let at = |a, b| [Entry { addr: h(3), a, b }];

            // (0,1) vs (1,0): a/b swapped
            assert_ne!(
                s.address(&at(0, 1)),
                s.address(&at(1, 0)),
                "{domain:?}: swapped framing must change the address"
            );
            // (0,1) vs (0,2): b varied outright
            assert_ne!(
                s.address(&at(0, 1)),
                s.address(&at(0, 2)),
                "{domain:?}: a changed framing value must change the address"
            );
            // (5,7) vs (7,5)
            assert_ne!(
                s.address(&at(5, 7)),
                s.address(&at(7, 5)),
                "{domain:?}: swapped framing must change the address"
            );
        }
    }

    /// The exact known-spec inputs of cloister-cas's
    /// `address_matches_upstream_fold_for_a_known_spec`, upstreamed. That
    /// test passed at cloister's older pin, regressed at 39e1e9b4, and must
    /// never regress again — this is the downstream contract stated locally.
    #[test]
    fn cloister_known_spec_commits_framing() {
        let s = PartitionSpec {
            domain: Domain::RowSet,
            scheme: "glob-closure/v1".into(),
            params: b"\x00".to_vec(),
            canon_version: 1,
        };
        let zero = Hash::from_bytes([0u8; 32]);
        let a = s.address(&[Entry {
            addr: zero,
            a: 0,
            b: 1,
        }]);
        let b = s.address(&[Entry {
            addr: zero,
            a: 1,
            b: 0,
        }]);
        assert_ne!(a, b, "framing must be committed to, not ignored");
    }

    /// The interval domain keeps both properties — order and framing are its
    /// content. This is the guard that the fix above did not overreach.
    #[test]
    fn interval_domain_still_honours_order_and_framing() {
        let s = spec(Domain::ByteStream);
        let a = [
            Entry {
                addr: h(1),
                a: 0,
                b: 5,
            },
            Entry {
                addr: h(2),
                a: 5,
                b: 5,
            },
        ];
        let reordered = [a[1], a[0]];
        let reframed = [
            a[0],
            Entry {
                addr: h(2),
                a: 5,
                b: 6,
            },
        ];

        assert_ne!(s.address(&a), s.address(&reordered));
        assert_ne!(s.address(&a), s.address(&reframed));
    }

    /// (DET): same spec, same entries, same address across calls.
    #[test]
    fn address_is_deterministic() {
        let s = spec(Domain::RowSet);
        let e = [Entry {
            addr: h(9),
            a: 1,
            b: 2,
        }];
        assert_eq!(s.address(&e), s.address(&e));
    }

    /// An empty decomposition is still a declared one, and must not collapse
    /// to the "no root yet" sentinel that `RootPointer` compares against.
    #[test]
    fn empty_partition_is_not_the_zero_sentinel() {
        let e: [Entry; 0] = [];
        assert_ne!(spec(Domain::ChunkSet).address(&e), Hash::ZERO);
    }
}
