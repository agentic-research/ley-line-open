// Cross-runtime drift gate for the daemon JSON wire protocol.
//
// Companion of the Rust gate at
// rs/ll-open/cli-lib/tests/integration.rs::daemon_protocol_gate_*. The Rust
// side asserts the daemon's UDS handlers emit responses containing every
// `response_required_keys` from `rs/ll-open/cli-lib/tests/fixtures/
// daemon-protocol.json`. This Go side asserts those same responses decode
// into the typed Go bindings declared by `daemon.capnp` (additively
// reconciled by bead ley-line-open-b631c8 / A-2).
//
// Together the two halves catch handler ↔ schema drift before it ships.
// Extends T8.10's cross-runtime fixture pattern (bead 6b7d43) from the
// substrate (capnp segment files; see binding/binding_test.go) to the
// daemon protocol (JSON wire).
//
// Fixtures with non-null `go_drift_skip` are skipped here with the drift
// reason as the skip message — the Rust gate still runs for them. The
// skip count is the diagnostic for A-2 to track schema reconciliation
// progress: every skip removed is one op brought into alignment.
//
// Bead: ley-line-open-b5a77b (A-1, this gate).

package daemon_test

import (
	"bytes"
	"encoding/json"
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
	bytes, err := os.ReadFile(fixturePath())
	if err != nil {
		t.Fatalf("read daemon-protocol.json: %v", err)
	}
	// Decode permissively so the top-level `_doc` string is ignored.
	var raw map[string]json.RawMessage
	if err := json.Unmarshal(bytes, &raw); err != nil {
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
	case "QueryResponse":
		return func(b []byte) error { var v queryResponse; return strictUnmarshal(b, &v) }
	default:
		return nil
	}
}

// strictUnmarshal fails on unknown fields. The point of the drift gate is
// to catch handler emitting fields the schema doesn't declare; without
// DisallowUnknownFields, json.Unmarshal silently drops them.
func strictUnmarshal(data []byte, v any) error {
	dec := json.NewDecoder(bytes.NewReader(data))
	dec.DisallowUnknownFields()
	return dec.Decode(v)
}

// TestDaemonProtocolGate_FixturesDecodeIntoTypedBindings is the structural
// drift-prevention test. For every op fixture whose `go_drift_skip` is
// null, attempt to decode the fixture's response JSON into the matching
// hand-written Go binding. Failure means the schema (daemon.capnp) and
// the handler-emitted JSON disagree on that op's shape — fix the schema
// (A-2) or the handler (A-3), don't fix the test.
//
// Fixtures with non-null `go_drift_skip` are skipped with the reason as
// the diagnostic message. As bead b631c8 (A-2) reconciles schema fields,
// each `go_drift_skip` flips to null and the matching op starts running
// here. The count of skipped ops is the visible progress metric.
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
