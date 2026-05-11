// Daemon protocol drift gate, Go half (bead ley-line-open-b5a77b / A-1).
//
// THIS gate (Go side, no daemon round-trip): strict-unmarshals each
// fixture's `response` payload from `rs/ll-open/cli-lib/tests/fixtures/
// daemon-protocol.json` into the matching typed Go binding declared by
// `daemon.capnp`. Pins FIXTURE ↔ SCHEMA agreement.
//
// The companion Rust gate at
// rs/ll-open/cli-lib/tests/integration.rs::daemon_protocol_gate_* DOES
// spawn the daemon and validates HANDLER ↔ FIXTURE agreement at runtime.
//
// Composing the two:
//   handler ↔ fixture (Rust gate) + fixture ↔ schema (this gate)
//   ⇒ handler ↔ schema (transitively)
//
// Either half failing means the chain broke. Together they extend T8.10's
// cross-runtime fixture pattern (bead 6b7d43) from the substrate (capnp
// segment files; see binding/binding_test.go) to the daemon protocol
// (JSON wire).
//
// Fixtures with non-null `go_drift_skip` are skipped here with the drift
// reason as the skip message — the Rust runtime gate still runs for them.
// The skip count is the diagnostic for A-2 (bead b631c8) to track schema
// reconciliation progress: every skip removed is one op whose fixture
// shape matches the typed Go binding.

package daemon_test

import (
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"testing"
)

// fixturePath joins the repo-root-relative path to the daemon protocol
// fixture file. Same un-vendored layout as binding/binding_test.go — one
// source of truth, both runtimes assert against it.
func fixturePath() string {
	return filepath.Join(
		"..", "..", "..", "..",
		"rs", "ll-open", "cli-lib", "tests", "fixtures",
		"daemon-protocol.json",
	)
}

// fixtureEntry is the shape of each per-op entry in daemon-protocol.json.
// Fields not used by this test (like response_required_keys, used only by
// the Rust side) are decoded as raw json.RawMessage to keep the gate
// resilient to future fixture-schema additions.
type fixtureEntry struct {
	Request              json.RawMessage `json:"request"`
	Response             json.RawMessage `json:"response"`
	ResponseRequiredKeys json.RawMessage `json:"response_required_keys"`
	ResponseOptionalKeys json.RawMessage `json:"response_optional_keys"`
	GoBinding            *string         `json:"go_binding"`
	GoDriftSkip          *string         `json:"go_drift_skip"`
}

func loadFixtures(t *testing.T) map[string]fixtureEntry {
	t.Helper()
	// `data`, not `bytes` — the `bytes` package is imported for
	// bytes.NewReader and shadowing it makes the file harder to scan.
	data, err := os.ReadFile(fixturePath())
	if err != nil {
		t.Fatalf("read daemon-protocol.json: %v", err)
	}
	// Decode permissively so the top-level `_doc` string is ignored.
	var raw map[string]json.RawMessage
	if err := json.Unmarshal(data, &raw); err != nil {
		t.Fatalf("parse daemon-protocol.json: %v", err)
	}
	out := make(map[string]fixtureEntry, len(raw))
	for op, payload := range raw {
		if op == "_doc" {
			continue
		}
		var entry fixtureEntry
		if err := json.Unmarshal(payload, &entry); err != nil {
			t.Fatalf("parse fixture %q: %v", op, err)
		}
		out[op] = entry
	}
	return out
}

// Inline JSON-tagged mirrors of `daemon.capnp` message types. Hand-written
// for A-1 because the capnpc-go generated bindings emit capnp-binary shapes,
// not JSON-tagged structs. A-2 (bead b631c8) extends regen.sh to emit these
// from the schema; at that point this file's inline definitions are
// replaced with imports from the regen target.
//
// IMPORTANT: keep field names and JSON tags in sync with daemon.capnp until
// A-2 lands. A drift between this file and the schema means the gate is
// testing the wrong thing.

type statusResponse struct {
	OK         *bool   `json:"ok"`
	Generation *uint64 `json:"generation"`
	ArenaPath  *string `json:"arenaPath"`
	ArenaSize  *uint64 `json:"arenaSize"`
}

type snapshotResponse struct {
	OK         *bool   `json:"ok"`
	Generation *uint64 `json:"generation"`
}

type node struct {
	ID       *string `json:"id"`
	ParentID *string `json:"parentId"`
	Name     *string `json:"name"`
	Kind     *int32  `json:"kind"`
	Size     *int64  `json:"size"`
	Record   *string `json:"record"`
}

type ref struct {
	NodeID   *string `json:"nodeId"`
	SourceID *string `json:"sourceId"`
}

type queryRow struct {
	Values []string `json:"values"`
}

type readContentResponse struct {
	OK      *bool   `json:"ok"`
	Content *string `json:"content"`
	Error   *string `json:"error"`
}

type listChildrenResponse struct {
	OK       *bool  `json:"ok"`
	Children []node `json:"children"`
}

type getNodeResponse struct {
	OK    *bool   `json:"ok"`
	Node  *node   `json:"node"`
	Error *string `json:"error"`
}

type findCallersResponse struct {
	OK      *bool `json:"ok"`
	Callers []ref `json:"callers"`
}

type findDefsResponse struct {
	OK   *bool `json:"ok"`
	Defs []ref `json:"defs"`
}

type findCalleesResponse struct {
	OK      *bool `json:"ok"`
	Callees []ref `json:"callees"`
}

type tokenMapEntry struct {
	Token   *string  `json:"token"`
	NodeIDs []string `json:"nodeIds"`
}

type getRefsMapResponse struct {
	OK      *bool           `json:"ok"`
	Entries []tokenMapEntry `json:"entries"`
}

type getDefsMapResponse struct {
	OK      *bool           `json:"ok"`
	Entries []tokenMapEntry `json:"entries"`
}

type schemaTier struct {
	Name   *string  `json:"name"`
	Crates []string `json:"crates"`
}

type getSchemaResponse struct {
	OK    *bool        `json:"ok"`
	Tiers []schemaTier `json:"tiers"`
}

type getDbPathResponse struct {
	OK           *bool   `json:"ok"`
	DBPath       *string `json:"dbPath"`
	CtrlPath     *string `json:"ctrlPath"`
	BindingsPath *string `json:"bindingsPath"`
	ASTPath      *string `json:"astPath"`
	SourcePath   *string `json:"sourcePath"`
	HeadPath     *string `json:"headPath"`
}

type queryResponse struct {
	OK      *bool      `json:"ok"`
	Columns []string   `json:"columns"`
	Rows    []queryRow `json:"rows"`
}

// decoderFor returns a function that attempts to unmarshal a response into
// the typed binding named by go_binding. Returns nil if the name is
// unknown — the gate treats unknown names as a fixture authoring error.
func decoderFor(name string) func([]byte) error {
	switch name {
	case "StatusResponse":
		return func(b []byte) error { var v statusResponse; return strictUnmarshal(b, &v) }
	case "SnapshotResponse":
		return func(b []byte) error { var v snapshotResponse; return strictUnmarshal(b, &v) }
	case "ReadContentResponse":
		return func(b []byte) error { var v readContentResponse; return strictUnmarshal(b, &v) }
	case "ListChildrenResponse":
		return func(b []byte) error { var v listChildrenResponse; return strictUnmarshal(b, &v) }
	case "GetNodeResponse":
		return func(b []byte) error { var v getNodeResponse; return strictUnmarshal(b, &v) }
	case "FindCallersResponse":
		return func(b []byte) error { var v findCallersResponse; return strictUnmarshal(b, &v) }
	case "FindDefsResponse":
		return func(b []byte) error { var v findDefsResponse; return strictUnmarshal(b, &v) }
	case "FindCalleesResponse":
		return func(b []byte) error { var v findCalleesResponse; return strictUnmarshal(b, &v) }
	case "GetRefsMapResponse":
		return func(b []byte) error { var v getRefsMapResponse; return strictUnmarshal(b, &v) }
	case "GetDefsMapResponse":
		return func(b []byte) error { var v getDefsMapResponse; return strictUnmarshal(b, &v) }
	case "GetSchemaResponse":
		return func(b []byte) error { var v getSchemaResponse; return strictUnmarshal(b, &v) }
	case "GetDbPathResponse":
		return func(b []byte) error { var v getDbPathResponse; return strictUnmarshal(b, &v) }
	case "QueryResponse":
		return func(b []byte) error { var v queryResponse; return strictUnmarshal(b, &v) }
	default:
		return nil
	}
}

// strictUnmarshal fails on unknown fields AND on trailing non-whitespace.
// The drift gate's job is to catch any divergence: schema declaring a field
// the handler doesn't emit, handler emitting a field the schema doesn't
// declare, OR a fixture accidentally containing multiple JSON values.
// Without DisallowUnknownFields + the explicit EOF check, json.Decode would
// silently drop unknown fields and ignore trailing junk.
func strictUnmarshal(data []byte, v any) error {
	dec := json.NewDecoder(bytes.NewReader(data))
	dec.DisallowUnknownFields()
	if err := dec.Decode(v); err != nil {
		return err
	}
	// Refuse to silently accept trailing content. A second Token() call
	// must return io.EOF; anything else means the fixture contained extra
	// data the first Decode didn't consume.
	if _, err := dec.Token(); err != io.EOF {
		if err == nil {
			return fmt.Errorf("trailing content after first JSON value")
		}
		return fmt.Errorf("trailing content: %v", err)
	}
	return nil
}

// TestDaemonProtocolGate_FixturesDecodeIntoTypedBindings pins the
// FIXTURE ↔ SCHEMA half of the drift chain. For every op fixture whose
// `go_drift_skip` is null, attempt a strict-decode of the fixture's
// `response` JSON payload (NOT a live daemon response — this test never
// talks to the daemon; the Rust gate handles runtime handler validation)
// into the matching hand-written Go binding that mirrors `daemon.capnp`.
//
// A decode failure here means the FIXTURE shape disagrees with the
// schema-mirroring Go binding. Fix the schema (A-2, bead b631c8) — and
// the fixture entry's response payload — to reconcile. The Rust gate
// then catches any HANDLER ↔ FIXTURE drift at runtime, completing the
// handler ↔ schema chain transitively.
//
// Fixtures with non-null `go_drift_skip` are skipped with the drift
// reason as the diagnostic message. As A-2 reconciles schema fields,
// each `go_drift_skip` flips to null and the matching op starts running
// here. The count of skipped ops is the visible progress metric for A-2.
func TestDaemonProtocolGate_FixturesDecodeIntoTypedBindings(t *testing.T) {
	fixtures := loadFixtures(t)
	if len(fixtures) == 0 {
		t.Fatal("expected at least one op fixture")
	}

	for op, entry := range fixtures {
		t.Run(op, func(t *testing.T) {
			if entry.GoDriftSkip != nil {
				t.Skipf("known drift (reconciled by bead b631c8 / A-2): %s", *entry.GoDriftSkip)
			}
			if entry.GoBinding == nil {
				t.Fatalf("fixture %q has go_drift_skip=null but no go_binding — fixture authoring error", op)
			}

			decode := decoderFor(*entry.GoBinding)
			if decode == nil {
				t.Fatalf("unknown go_binding %q (add a case in decoderFor or fix the fixture)", *entry.GoBinding)
			}

			if err := decode(entry.Response); err != nil {
				t.Errorf("decode response into %s failed: %v\nresponse=%s",
					*entry.GoBinding, err, string(entry.Response))
			}
		})
	}
}
