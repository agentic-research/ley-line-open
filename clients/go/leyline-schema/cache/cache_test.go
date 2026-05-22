// Cache schema smoke tests (Go side, bead ley-line-open-ae89aa).
//
// Pins the same shape the Rust round-trip tests in
// `rs/ll-core/schema-capnp/src/lib.rs` exercise — if either side
// drifts, this gate fails. Together they extend ADR-0014's
// "every schema has a producer/consumer round-trip" discipline
// to cache.capnp.
//
// The deeper round-trip suite (capnp ↔ TOML, capnp ↔ OCI-JSON) lives
// in the consumer crates: mache for the TOML rendering, eventually
// cloister for the OCI-JSON wire shape. This file pins only that the
// generated Go binding accepts and round-trips capnp-segment-shaped
// messages — the contract every consumer reads against.

package cache_test

import (
	"bytes"
	"testing"

	capnp "capnproto.org/go/capnp/v3"
	cache "github.com/agentic-research/ley-line-open/clients/go/leyline-schema/cache"
	common "github.com/agentic-research/ley-line-open/clients/go/leyline-schema/common"
)

// writeHash fills a common.Hash with the canonical 32-byte BLAKE3 shape.
// Helper to keep the test bodies tight; mirrors the `write_hash` helper
// in the Rust tests for symmetry.
func writeHash(t *testing.T, h common.Hash, bytes32 [32]byte) {
	t.Helper()
	if err := h.SetBytes(bytes32[:]); err != nil {
		t.Fatalf("set hash bytes: %v", err)
	}
}

// readHashBytes extracts the 32-byte BLAKE3 payload from a common.Hash.
// Returns the bytes (asserting length 32) — any deviation is a hard fail
// because the substrate locks Hash to BLAKE3 per Σ §3.4.
func readHashBytes(t *testing.T, h common.Hash) [32]byte {
	t.Helper()
	b, err := h.Bytes()
	if err != nil {
		t.Fatalf("read hash bytes: %v", err)
	}
	if len(b) != 32 {
		t.Fatalf("expected 32-byte BLAKE3, got %d bytes", len(b))
	}
	var out [32]byte
	copy(out[:], b)
	return out
}

// TestSourceEntry_RoundTrip pins (path, inputHash, chunkHash, kind).
// If any ordinal drifts, the Rust + Go bindings end up with different
// field positions and round-trip fails at exactly this assertion.
func TestSourceEntry_RoundTrip(t *testing.T) {
	msg, seg, err := capnp.NewMessage(capnp.SingleSegment(nil))
	if err != nil {
		t.Fatalf("new message: %v", err)
	}

	se, err := cache.NewRootSourceEntry(seg)
	if err != nil {
		t.Fatalf("new source entry: %v", err)
	}
	if err := se.SetPath("src/auth.go"); err != nil {
		t.Fatalf("set path: %v", err)
	}
	ih, err := se.NewInputHash()
	if err != nil {
		t.Fatalf("new input hash: %v", err)
	}
	writeHash(t, ih, [32]byte{17: 0xAA, 31: 0xBB})
	ch, err := se.NewChunkHash()
	if err != nil {
		t.Fatalf("new chunk hash: %v", err)
	}
	writeHash(t, ch, [32]byte{2: 0xCC, 0: 0xDD})
	if err := se.SetKind("go-source"); err != nil {
		t.Fatalf("set kind: %v", err)
	}

	var buf bytes.Buffer
	if err := capnp.NewEncoder(&buf).Encode(msg); err != nil {
		t.Fatalf("encode: %v", err)
	}

	dec, err := capnp.NewDecoder(&buf).Decode()
	if err != nil {
		t.Fatalf("decode: %v", err)
	}
	roundtrip, err := cache.ReadRootSourceEntry(dec)
	if err != nil {
		t.Fatalf("read root: %v", err)
	}

	path, err := roundtrip.Path()
	if err != nil {
		t.Fatalf("read path: %v", err)
	}
	if path != "src/auth.go" {
		t.Errorf("path drift: want src/auth.go, got %q", path)
	}
	kind, err := roundtrip.Kind()
	if err != nil {
		t.Fatalf("read kind: %v", err)
	}
	if kind != "go-source" {
		t.Errorf("kind drift: want go-source, got %q", kind)
	}

	gotInput, err := roundtrip.InputHash()
	if err != nil {
		t.Fatalf("read input hash: %v", err)
	}
	inBytes := readHashBytes(t, gotInput)
	if inBytes[17] != 0xAA || inBytes[31] != 0xBB {
		t.Errorf("input hash drift: want [17]=0xAA, [31]=0xBB; got [17]=0x%02x, [31]=0x%02x", inBytes[17], inBytes[31])
	}

	gotChunk, err := roundtrip.ChunkHash()
	if err != nil {
		t.Fatalf("read chunk hash: %v", err)
	}
	chBytes := readHashBytes(t, gotChunk)
	if chBytes[0] != 0xDD || chBytes[2] != 0xCC {
		t.Errorf("chunk hash drift: want [0]=0xDD, [2]=0xCC; got [0]=0x%02x, [2]=0x%02x", chBytes[0], chBytes[2])
	}
}

// TestMeta_WithProcessors_RoundTrip pins (producer, producerVersion,
// schemaVersion, generatedAtMs) AND the nested inputProcessors list.
// Catches the class of bug where outer fields look fine but the
// nested list-of-struct has drifted independently.
func TestMeta_WithProcessors_RoundTrip(t *testing.T) {
	msg, seg, err := capnp.NewMessage(capnp.SingleSegment(nil))
	if err != nil {
		t.Fatalf("new message: %v", err)
	}

	m, err := cache.NewRootMeta(seg)
	if err != nil {
		t.Fatalf("new meta: %v", err)
	}
	if err := m.SetProducer("mache"); err != nil {
		t.Fatalf("set producer: %v", err)
	}
	if err := m.SetProducerVersion("0.7.1"); err != nil {
		t.Fatalf("set producer version: %v", err)
	}
	if err := m.SetSchemaVersion("0.1.0"); err != nil {
		t.Fatalf("set schema version: %v", err)
	}
	m.SetGeneratedAtMs(1_748_345_600_000)

	procs, err := m.NewInputProcessors(2)
	if err != nil {
		t.Fatalf("new input processors: %v", err)
	}
	p0 := procs.At(0)
	if err := p0.SetKind("tree-sitter-go"); err != nil {
		t.Fatalf("set p0 kind: %v", err)
	}
	if err := p0.SetVersion("0.21.0"); err != nil {
		t.Fatalf("set p0 version: %v", err)
	}
	p1 := procs.At(1)
	if err := p1.SetKind("blake3"); err != nil {
		t.Fatalf("set p1 kind: %v", err)
	}
	if err := p1.SetVersion("1.5.0"); err != nil {
		t.Fatalf("set p1 version: %v", err)
	}

	var buf bytes.Buffer
	if err := capnp.NewEncoder(&buf).Encode(msg); err != nil {
		t.Fatalf("encode: %v", err)
	}

	dec, err := capnp.NewDecoder(&buf).Decode()
	if err != nil {
		t.Fatalf("decode: %v", err)
	}
	rt, err := cache.ReadRootMeta(dec)
	if err != nil {
		t.Fatalf("read root meta: %v", err)
	}

	prod, _ := rt.Producer()
	if prod != "mache" {
		t.Errorf("producer drift: want mache, got %q", prod)
	}
	pv, _ := rt.ProducerVersion()
	if pv != "0.7.1" {
		t.Errorf("producer version drift: want 0.7.1, got %q", pv)
	}
	sv, _ := rt.SchemaVersion()
	if sv != "0.1.0" {
		t.Errorf("schema version drift: want 0.1.0, got %q", sv)
	}
	if got := rt.GeneratedAtMs(); got != 1_748_345_600_000 {
		t.Errorf("generated ms drift: want 1748345600000, got %d", got)
	}

	rtProcs, err := rt.InputProcessors()
	if err != nil {
		t.Fatalf("read processors: %v", err)
	}
	if rtProcs.Len() != 2 {
		t.Fatalf("processor count drift: want 2, got %d", rtProcs.Len())
	}
	k0, _ := rtProcs.At(0).Kind()
	v0, _ := rtProcs.At(0).Version()
	if k0 != "tree-sitter-go" || v0 != "0.21.0" {
		t.Errorf("processor[0] drift: want (tree-sitter-go, 0.21.0), got (%s, %s)", k0, v0)
	}
	k1, _ := rtProcs.At(1).Kind()
	v1, _ := rtProcs.At(1).Version()
	if k1 != "blake3" || v1 != "1.5.0" {
		t.Errorf("processor[1] drift: want (blake3, 1.5.0), got (%s, %s)", k1, v1)
	}
}

// TestCacheLockfile_FullRoundTrip pins the top-level assembly:
// meta + N sources + N edges + root. Mirrors the Rust
// `cache_lockfile_full_round_trip` test for cross-runtime symmetry.
func TestCacheLockfile_FullRoundTrip(t *testing.T) {
	msg, seg, err := capnp.NewMessage(capnp.SingleSegment(nil))
	if err != nil {
		t.Fatalf("new message: %v", err)
	}

	lf, err := cache.NewRootCacheLockfile(seg)
	if err != nil {
		t.Fatalf("new lockfile: %v", err)
	}

	// Meta
	m, err := lf.NewMeta()
	if err != nil {
		t.Fatalf("new meta: %v", err)
	}
	_ = m.SetProducer("mache")
	_ = m.SetProducerVersion("0.7.1")
	_ = m.SetSchemaVersion("0.1.0")
	m.SetGeneratedAtMs(1_748_345_600_000)
	procs, _ := m.NewInputProcessors(1)
	p := procs.At(0)
	_ = p.SetKind("tree-sitter-go")
	_ = p.SetVersion("0.21.0")

	// Sources
	srcs, err := lf.NewSources(2)
	if err != nil {
		t.Fatalf("new sources: %v", err)
	}
	s0 := srcs.At(0)
	_ = s0.SetPath("src/main.go")
	ih0, _ := s0.NewInputHash()
	writeHash(t, ih0, [32]byte{0: 0x01})
	ch0, _ := s0.NewChunkHash()
	writeHash(t, ch0, [32]byte{0: 0x10})
	_ = s0.SetKind("go-source")

	s1 := srcs.At(1)
	_ = s1.SetPath("src/auth.go")
	ih1, _ := s1.NewInputHash()
	writeHash(t, ih1, [32]byte{0: 0x02})
	ch1, _ := s1.NewChunkHash()
	writeHash(t, ch1, [32]byte{0: 0x20})
	_ = s1.SetKind("go-source")

	// Topology
	edges, err := lf.NewTopology(1)
	if err != nil {
		t.Fatalf("new topology: %v", err)
	}
	e := edges.At(0)
	_ = e.SetFrom("src/main.go")
	_ = e.SetToSource("src/auth.go")

	// Root
	root, _ := lf.NewRoot()
	writeHash(t, root, [32]byte{0: 0xFF, 31: 0xFF})

	var buf bytes.Buffer
	if err := capnp.NewEncoder(&buf).Encode(msg); err != nil {
		t.Fatalf("encode: %v", err)
	}

	dec, err := capnp.NewDecoder(&buf).Decode()
	if err != nil {
		t.Fatalf("decode: %v", err)
	}
	rt, err := cache.ReadRootCacheLockfile(dec)
	if err != nil {
		t.Fatalf("read root lockfile: %v", err)
	}

	rtMeta, _ := rt.Meta()
	prod, _ := rtMeta.Producer()
	if prod != "mache" {
		t.Errorf("producer drift: want mache, got %q", prod)
	}
	rtSrcs, _ := rt.Sources()
	if rtSrcs.Len() != 2 {
		t.Errorf("sources count drift: want 2, got %d", rtSrcs.Len())
	}
	rtEdges, _ := rt.Topology()
	if rtEdges.Len() != 1 {
		t.Errorf("topology count drift: want 1, got %d", rtEdges.Len())
	}
	from, _ := rtEdges.At(0).From()
	to, _ := rtEdges.At(0).ToSource()
	if from != "src/main.go" || to != "src/auth.go" {
		t.Errorf("edge drift: want (src/main.go -> src/auth.go), got (%s -> %s)", from, to)
	}
	rtRoot, _ := rt.Root()
	rootBytes := readHashBytes(t, rtRoot)
	if rootBytes[0] != 0xFF || rootBytes[31] != 0xFF {
		t.Errorf("root drift: want [0]=0xFF, [31]=0xFF; got [0]=0x%02x, [31]=0x%02x", rootBytes[0], rootBytes[31])
	}
}

// TestEmptyCacheLockfile_Valid pins the "first push, no chunks yet"
// edge case — an empty sources list, empty topology, default root.
// Restore implementations must accept this as a valid empty bundle,
// not error on the missing data.
func TestEmptyCacheLockfile_Valid(t *testing.T) {
	msg, seg, err := capnp.NewMessage(capnp.SingleSegment(nil))
	if err != nil {
		t.Fatalf("new message: %v", err)
	}

	lf, err := cache.NewRootCacheLockfile(seg)
	if err != nil {
		t.Fatalf("new lockfile: %v", err)
	}
	m, err := lf.NewMeta()
	if err != nil {
		t.Fatalf("new meta: %v", err)
	}
	_ = m.SetProducer("mache")
	_ = m.SetSchemaVersion("0.1.0")

	var buf bytes.Buffer
	if err := capnp.NewEncoder(&buf).Encode(msg); err != nil {
		t.Fatalf("encode: %v", err)
	}

	dec, err := capnp.NewDecoder(&buf).Decode()
	if err != nil {
		t.Fatalf("decode: %v", err)
	}
	rt, err := cache.ReadRootCacheLockfile(dec)
	if err != nil {
		t.Fatalf("read root: %v", err)
	}

	srcs, err := rt.Sources()
	if err != nil {
		t.Fatalf("read sources: %v", err)
	}
	if srcs.Len() != 0 {
		t.Errorf("empty sources drift: want 0, got %d", srcs.Len())
	}
	edges, err := rt.Topology()
	if err != nil {
		t.Fatalf("read topology: %v", err)
	}
	if edges.Len() != 0 {
		t.Errorf("empty topology drift: want 0, got %d", edges.Len())
	}
}
