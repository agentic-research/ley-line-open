# ADR-0014 — Cap'n Proto as the producer/consumer protocol

**Status:** Accepted (2026-05-08)
**Decade:** `ley-line-open-9d30ac` (Σ Merkle-CAS substrate)
**Thread:** `T8/capnp-as-protocol`
**Bead:** `ley-line-open-ce8fd1` (T8.6)

**Sibling artifacts** (read for fuller context):
- `docs/decades/T8/adr-0014-design-analysis.md` — math-friend's theoretical analysis (8 open questions)
- `docs/decades/T8/capnp-rtfm-findings.md` — published-precedent dossier (canonical encoding, workerd/sandstorm conventions, IPLD/ATproto comparison)

---

## Context

`ley-line-open-be6136` (closed at commit `9fb993f`) was a one-byte path mismatch — `_source.path` stored un-canonicalized while LSP URIs were canonicalized — that made every `_lsp_refs` × `_ast` JOIN miss on macOS, leaving every `referrer_node_id` NULL. The bug was undetectable until empirical falsification at the consumer end (mache's `Falsifiability A` / `Falsifiability B` harnesses). The local fix was a one-line `canonicalize()` call. The structural lesson is that the SQL schema was acting as a cross-process protocol — and SQL has no schema-evolution story, no compile-time type check across the producer/consumer boundary, and no canonicalization contract.

Threads T8.1–T8.5 introduced Cap'n Proto schemas as the typed cross-runtime contract: producers (Rust LLO) emit capnp segment files; consumers (Go mache, future TS workerd, Swift control-room) read them with bindings generated from the same `.capnp` source. SQLite tables are local *projections*, not the contract. Σ root advances are computed over the producer's emitted segments — the bytes-on-wire are the substrate.

This ADR codifies the rules that govern that contract.

---

## Decision

ADR-0014 commits, in order of load-bearing-ness:

### 1. Reading A — canonical encoding at the producer; Σ root over canonical bytes

Cap'n Proto has a published canonical encoding (`docs/encoding.md` "Canonicalization"; capnp 1.3.0+) explicitly engineered so that **adding a field at the next ordinal with default value does not change the canonical bytes for instances that don't set it** (encoding spec, bullet 3). Both the Rust runtime (`Builder::set_root_canonical`) and Go runtime (`canonical.Canonicalize`, public doc: *"identical for equivalent structs, even as the schema evolves"*) implement this. Issue #2171 is closed by Kenton Varda's confirmation that canonical encoding is the answer to deterministic-serialization questions.

**Rule.** Every producer call site that emits a capnp record into a Σ segment file MUST canonicalize via `Builder::set_root_canonical` (Rust) / `Canonicalize` (Go) / equivalents in TS/Swift. Hash functions over Σ segments MUST canonicalize on read defensively (re-canonicalize before hashing) so the chain stays deterministic even if a non-conformant producer ever writes raw bytes. The Σ root is `BLAKE3(canonical_bytes(segment_files), in canonical order)`.

**Implementation status.** Migration completed under this ADR's accept commit — 4 producer call sites in `rs/ll-open/cli-lib/src/cmd_parse.rs` (`write_source_file_record`, `write_ast_node_record`, `write_head_after_parse`) and `rs/ll-open/lsp/src/project.rs` (`write_binding_record`) all use `set_root_canonical`. `hash_segment_files` in `cmd_parse.rs` canonicalizes-on-read via `capnp::message::Reader::canonicalize()`. Regression test `segment_hash_is_canonical_byte_stable` pins the byte-stability invariant.

**Consequence.** An additive schema change (append a field at `@N` with default value) does not advance Σ root for instances that don't set the new field. The substrate is byte-stable across schema evolution for unchanged data. This eliminates the math-friend's Q1 dilemma (Reading A vs B) on the side of canonical encoding — backed by IPLD/DAG-CBOR and ATproto/DRISL precedent (the CID is the version), not the math-friend's incorrect premise that capnp lacks a canonical form.

**Rejected alternative.** Reading B — content-addressing the raw on-disk bytes including segment-table prefixes and unset-default-zero data — has no published precedent and forces every additive schema change to advance Σ root unnecessarily. The math-friend recommended Reading B based on a factually wrong premise (§3.5.4 of the design-analysis claim that "capnp explicitly does NOT guarantee canonical encoding"). The RTFM dossier corrected this; ADR-0014 reverses the recommendation.

### 2. Append-only-additive evolution; the schema is its own version manifest

Workerd (Cloudflare's open-source Workers runtime) and Sandstorm (capnp's birthplace) — the two largest public capnp deployments — both evolve schemas via:

- **Append fields at the next `@N` ordinal**; never rename, never reuse, never repurpose. Inline `# DEPRECATED: ...` docstrings for retired fields. (`workerd/src/workerd/io/compatibility-date.capnp`: 114 ordinals appended over 5 years; `sandstorm/src/sandstorm/grain.capnp`: explicit comment *"new versions of the app only add new permissions, never remove existing ones"*.)
- **File ID (`@0x...`) is file identity, not file version** — stable for the life of the file even as content evolves. (Capnp language spec, "Unique IDs": *"In general, you would only specify an explicit ID for a declaration if that declaration has been renamed or moved and you want the ID to stay the same."*)
- **No `schemaVersion` field on the wire.** Neither workerd nor sandstorm bakes a monotone version counter into their wire format. Workerd's `compatEnableDate` / `compatDisableFlag` annotations are per-field, not per-message.

**Rule.** ADR-0014 schemas follow the same discipline:
- Fields appended at next free `@N` with default values
- Never rename; deprecated fields use `# DEPRECATED:` docstrings and stay in place
- Never reuse an ordinal; never repurpose a field's meaning even if the type happens to match
- File IDs (`@0xb0c0debaadc0deb0` etc.) are stable; CI gate on the `(filename, fileId)` allowlist
- **No `schemaVersion :UInt64` field** in any T8 schema. The schema files themselves, addressed by the canonical hash of their source bytes (and locked to the toolchain triplet — see §3), are the version manifest.

**Rejected alternatives.** A `schemaVersion :UInt64` in `Head` (the math-friend's Q2 option a) has no ecosystem precedent. A sibling `manifest.capnp` per Σ generation (option b) has weak workerd-style precedent but adds wire complexity. An opaque counter (option c) provides no consumer verifiability. The IPLD/ATproto precedent — *"the CID is the version"* — combined with workerd/sandstorm's no-version-on-the-wire practice, makes the schema's own canonical bytes the right version surface.

**Future migration path** (deferred to a follow-on ADR): adopt workerd-style annotation-driven versioning (`$introducedInGen(N)` etc.) once the substrate has a multi-runtime annotation library. Until then, the manifest is the schema source.

### 3. Pin the toolchain triplet; ship cross-runtime fixtures

Three artifact tiers must be version-anchored for reproducible cross-runtime byte equality:

1. **Compiler binary.** The `capnp` C++ tool generates schema metadata consumed by every language generator. Required: `capnp >= 1.0`, tested against `1.3.0`. Document via `tools/install-capnp.sh` and the schema-capnp `build.rs` should fail-fast on too-old versions. Workerd's Bazel-pin pattern is the strongest precedent; we adapt to a script-based pin since this repo doesn't use Bazel.
2. **Per-language generators.** Rust: `capnpc = "=0.20.0"` exact (currently `"0.20"` semver-range — TIGHTEN). Go (mache side): `capnp.org/go/capnp/v3/capnpc-go@v3.0.X` exact tag, NOT `@latest` (the Copilot review of mache PR-1 caught `@latest` as non-reproducible). Generated bindings are committed to source control to eliminate dev-machine drift.
3. **Per-language runtimes.** Rust: `capnp = "=0.20.0"` exact. Go: `require capnproto.org/go/capnp/v3 v3.0.X` exact. Same exact-pin discipline as generators.

**Rule.** Tooling versions are part of the contract. A version mismatch is a CI failure, not a runtime mystery.

**Cross-runtime fixture suite** (novel — community doesn't ship one). `rs/ll-core/schema-capnp/tests/fixtures/*.bin` files committed as gold-standard canonical-encoded messages, with sibling `*.expected.json` files describing their decoded content. Both LLO Rust CI and mache Go CI run the suite — encoding in one language and decoding in the other must produce field-equal results. This is the F8.6.4 test from the math-friend's analysis; it becomes the strongest invariant of T8 once it ships. (Tracked as a follow-up; not blocking ADR acceptance.)

---

## Falsifiable claims

Each rule above maps to at least one CI test that fails when the rule is violated. From the math-friend's §5 (F8.6.1–F8.6.6), updated against the RTFM dossier:

- **F8.6.1** — Schema with renamed field fails CI. Test: vendored fixture asserts each schema's `(field_name → ordinal)` map is unchanged from the prior commit.
- **F8.6.2** — Producer call site that uses `serialize::write_message` directly (instead of `set_root_canonical`) fails CI. Test: clippy lint or grep gate on producer modules.
- **F8.6.3** — File ID (`@0x...`) drift fails CI. Test: allowlist of `(schemas/*.capnp, fileId)` pairs verified at build time.
- **F8.6.4** — Encode-in-Rust, decode-in-Go does NOT round-trip canonical bytes equal. Test: cross-runtime fixture suite (deferred; biggest single CI investment).
- **F8.6.5** — A canonical-encoded message with all defaults is non-empty, OR `hash_segment_files` returns non-deterministic output. Test: `segment_hash_is_canonical_byte_stable` (already shipped; cmd_parse.rs::tests).
- **F8.6.6** — Tooling version drift produces wire-incompatible bytes. Test: pin floor + cross-runtime fixture suite.

---

## Consequences

### Positive

- **be6136 class structurally precluded.** A SQL-column-name disagreement between producer and consumer cannot recur because SQL columns are projections, not the contract; the contract is type-checked at compile time in every consuming runtime.
- **Schema evolution is a routine operation.** Add a field at next ordinal; canonical encoding handles the wire stability; consumers built against an older schema simply see the new field as absent (default-valued); no Σ root advance for unchanged data.
- **Cross-runtime byte equality.** The same `.capnp` source produces byte-equal canonical messages in Rust and Go (and TS/Swift when those land). Cross-runtime fixtures are a real test, not aspirational.
- **Σ root is consumer-verifiable.** Any third party can re-hash the segment files and confirm the producer's claimed root — the producer is not a trusted oracle.
- **Mache's Falsifiability B reduces to a tight equality check** once mache's PR-2 lands and reads the canonical-form `BindingRecord.constructNodeId` directly instead of joining `_lsp_refs` × `_ast`.

### Negative

- **Producer-side complexity.** Every capnp emission now requires a two-message pattern (build → canonicalize-via-`set_root_canonical` → write). Mitigation: localized in helper functions; net change ≈4 LOC per call site.
- **Toolchain pin cost.** Exact-pinning capnpc/capnp-go/capnp runtimes means dependabot-style auto-updates require explicit human review. This is the intent — toolchain version is part of the contract — but it does mean more PR friction than a semver-range allows.
- **Cross-runtime fixture suite is novel work.** No existing community pattern; we ship the first version. Maintenance burden as schemas evolve; offset by the fact that schema evolution itself is rare under append-only-additive discipline.
- **Reading A's canonical encoding gives byte-stability for *unchanged* data.** It does not paper over real semantic changes — repurposing a field, even if the type matches, will produce different canonical bytes once consumers actually start setting the new meaning. The append-only rule is what rules out the dangerous case.

### Out of scope (future ADRs)

- **Live RPC / `interface` schemas.** ADR-0014 covers data schemas (`struct`-only). Daemon RPC, distributed segment fetch, and `Persistent`/SturdyRef capabilities live in a separate file ID set under a future ADR. Scope boundary is explicit.

  *Interim status (2026-05-11, A-3 / `ley-line-open-b69606`):* the daemon's UDS + MCP wire is JSON-encoded, but `daemon.capnp` defines the typed contract for every message and `rs/ll-open/cli-lib/src/daemon/wire.rs` is a hand-written serde mirror that handlers serialize through. wire.rs is *not* codegen'd from `daemon.capnp`, so schema↔wire parity is enforced by tests/CI rather than the compiler — specifically the cross-runtime fixture gate (`rs/ll-open/cli-lib/tests/fixtures/daemon-protocol.json` consumed by both a Rust handler-output test and a Go strict-unmarshal test). The typed `BaseRequest` enum + handler signatures catch the common drift class at compile time; the fixture gate catches the rest. This is the JSON-as-carrier doctrine (cloister-side `interlace-spec/0.1.0/README.md` § "Wire carriers vs. typed contracts"; external to this repo): the schema is the contract; the carrier encoding (JSON today, capnp framing later) is a per-side tag. The future RPC framing ADR — likely triggered by cloister adopting `udsForward` against LLO directly — flips the carrier without touching the contract.
- **Annotation-driven versioning** (`$introducedInGen(N)` à la workerd). Deferred until the substrate has multi-runtime tooling support for capnp annotations. The schema-as-version-manifest pattern is sufficient for now.
- **Schema-source-content hashing as a wire-level commitment.** A future ADR may add a per-Σ-generation manifest that binds *(filename, BLAKE3-of-canonical-schema-bytes)* tuples into the rootHash chain, providing third-party verification of "my consumer parsed this segment with the same schema the producer used." Today's discipline (commit `.capnp` files; pin toolchain) covers this in CI; not on the wire.

---

## Implementation status (snapshot 2026-05-08)

| Commitment | Status | Reference |
|---|---|---|
| Producer canonicalizes (4 call sites) | ✅ Shipped | this commit, cmd_parse.rs / project.rs |
| `hash_segment_files` canonicalizes on read | ✅ Shipped | cmd_parse.rs::hash_segment_files |
| Regression test `segment_hash_is_canonical_byte_stable` | ✅ Shipped | cmd_parse.rs::tests |
| Append-only-additive evolution rule | ✅ Documented; CI gate is followup | this ADR |
| `(filename, fileId)` allowlist CI gate | ⏳ Followup | tracked in T8.6 close comment |
| Toolchain triplet pin | 🟡 Partial — capnpc semver-range, generators unpinned | upgrade in followup |
| Cross-runtime fixture suite | ⏳ Followup (F8.6.4) | major investment; not blocking |
| Annotation-driven versioning (workerd-style) | ⏳ Future ADR | deferred |

---

## References

- `docs/decades/T8/adr-0014-design-analysis.md` — math-friend theoretical analysis
- `docs/decades/T8/capnp-rtfm-findings.md` — RTFM research dossier
- `docs/decades/2026-merkle-cas-substrate.md` — Σ decade (BLAKE3 lock at §3.4)
- Cap'n Proto encoding spec: <https://capnproto.org/encoding.html#canonicalization>
- Cap'n Proto schema language: <https://capnproto.org/language.html>
- Workerd compatibility-date system: <https://github.com/cloudflare/workerd/blob/main/src/workerd/io/compatibility-date.capnp>
- IPLD DAG-CBOR canonical form: <https://ipld.io/specs/codecs/dag-cbor/spec/>
- ATproto Lexicon evolution rules: <https://atproto.com/specs/lexicon>
- ProtoBuf "not canonical" position: <https://protobuf.dev/programming-guides/serialization-not-canonical/>
