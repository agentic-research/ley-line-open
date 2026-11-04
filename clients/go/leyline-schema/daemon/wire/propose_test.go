package wire

// read_source / propose / status JSON types (beads ley-line-open-247489,
// ley-line-open-246526). The payloads are the shared daemon protocol
// fixtures, which the Rust gate
// rs/ll-open/cli-lib/tests/integration.rs::daemon_protocol_gate_proposals_produce_the_fixture_shapes
// proves the daemon emits, so these tests pin the shipped Go types to the
// daemon's actual wire rather than to a hand-written mirror.

import (
	"bytes"
	"encoding/json"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
	"unicode"

	"github.com/agentic-research/ley-line-open/clients/go/leyline-schema/daemon"
)

type fixture struct {
	Request  json.RawMessage `json:"request"`
	Response json.RawMessage `json:"response"`
}

func loadFixture(t *testing.T, name string) fixture {
	t.Helper()
	data, err := os.ReadFile(filepath.Join(
		"..", "..", "..", "..", "..",
		"rs", "ll-open", "cli-lib", "tests", "fixtures", "daemon-protocol.json",
	))
	if err != nil {
		t.Fatalf("read daemon-protocol.json: %v", err)
	}
	var all map[string]json.RawMessage
	if err := json.Unmarshal(data, &all); err != nil {
		t.Fatalf("parse daemon-protocol.json: %v", err)
	}
	raw, ok := all[name]
	if !ok {
		t.Fatalf("daemon-protocol.json has no %q fixture", name)
	}
	var f fixture
	if err := json.Unmarshal(raw, &f); err != nil {
		t.Fatalf("parse fixture %q: %v", name, err)
	}
	return f
}

// strictDecode refuses unknown fields and trailing content, so a field the
// daemon emits that the type lacks fails the test instead of vanishing.
func strictDecode(data []byte, v any) error {
	dec := json.NewDecoder(bytes.NewReader(data))
	dec.DisallowUnknownFields()
	if err := dec.Decode(v); err != nil {
		return err
	}
	if _, err := dec.Token(); err != io.EOF {
		return fmt.Errorf("trailing content after the JSON value: %v", err)
	}
	return nil
}

// roundTrip strict-decodes data into v, encodes v again, and requires the
// encoding to be the same JSON value as data.
func roundTrip(t *testing.T, data []byte, v any) {
	t.Helper()
	if err := strictDecode(data, v); err != nil {
		t.Fatalf("strict decode into %T: %v\n%s", v, err, data)
	}
	again, err := json.Marshal(v)
	if err != nil {
		t.Fatalf("encode %T: %v", v, err)
	}
	var want, got any
	if err := json.Unmarshal(data, &want); err != nil {
		t.Fatal(err)
	}
	if err := json.Unmarshal(again, &got); err != nil {
		t.Fatal(err)
	}
	if !reflect.DeepEqual(want, got) {
		t.Fatalf("%T does not round-trip:\n want %s\n  got %s", v, data, again)
	}
}

func TestProposeResponseRoundTripsStaleRefusalWithConflicts(t *testing.T) {
	var got ProposeResponse
	roundTrip(t, loadFixture(t, "propose").Response, &got)

	if got.OK == nil || *got.OK {
		t.Fatalf("a refused proposal is ok:false; got %v", got.OK)
	}
	// A refusal carries `error` beside `refusal`: the error text is not
	// the failure signal, the refusal is.
	if got.Error == nil || *got.Error == "" {
		t.Fatalf("a refusal carries a message in error; got %v", got.Error)
	}
	r := got.Refusal
	if r == nil || r.Reason == nil || *r.Reason != RefusalStale {
		t.Fatalf("refusal.reason: want %q, got %+v", RefusalStale, r)
	}
	if r.Path != nil || r.Other != nil {
		t.Fatalf("a stale refusal names no path or other; got path=%v other=%v", r.Path, r.Other)
	}
	if len(r.Moved) != 1 {
		t.Fatalf("refusal.moved: want 1 entry, got %d", len(r.Moved))
	}
	m := r.Moved[0]
	if m.Path == nil || *m.Path != "src/main.rs" ||
		m.Expected == nil || !strings.HasPrefix(*m.Expected, "3c2f6a5b") ||
		m.Current == nil || !strings.HasPrefix(*m.Current, "0a1b2c3d") {
		t.Fatalf("moved[0]: %+v", m)
	}
	if len(m.Conflicts) != 1 ||
		m.Conflicts[0].Start == nil || *m.Conflicts[0].Start != 0 ||
		m.Conflicts[0].End == nil || *m.Conflicts[0].End != 1 {
		t.Fatalf("moved[0].conflicts: want [{0 1}], got %+v", m.Conflicts)
	}
}

func TestProposeResponseRoundTripsAppliedWithMerged(t *testing.T) {
	var got ProposeResponse
	roundTrip(t, loadFixture(t, "propose_merged").Response, &got)

	if got.OK == nil || !*got.OK || got.Refusal != nil {
		t.Fatalf("an applied proposal is ok:true with no refusal; got ok=%v refusal=%+v", got.OK, got.Refusal)
	}
	if got.TreeRoot == nil || len(*got.TreeRoot) != 64 {
		t.Fatalf("tree_root is 64 hex chars; got %v", got.TreeRoot)
	}
	if len(got.Blobs) != 1 || got.Blobs[0].Path == nil || *got.Blobs[0].Path != "src/main.rs" ||
		got.Blobs[0].BlobHash == nil || len(*got.Blobs[0].BlobHash) != 64 {
		t.Fatalf("blobs: %+v", got.Blobs)
	}
	if !reflect.DeepEqual(got.Merged, []string{"src/main.rs"}) {
		t.Fatalf("merged: want [src/main.rs], got %v", got.Merged)
	}
}

// Every refused and applied propose fixture decodes strictly and
// round-trips, including the refusal `other` field and the unmerged shapes.
func TestProposeResponseRoundTripsEveryProposeFixture(t *testing.T) {
	for _, name := range []string{
		"propose_tree", "propose_refused", "propose_applied",
		"propose_structural", "propose_create_stale",
	} {
		t.Run(name, func(t *testing.T) {
			var got ProposeResponse
			roundTrip(t, loadFixture(t, name).Response, &got)
		})
	}
}

// Bead ley-line-open-23a26c's refusal, shipped in v0.22.0: a create the
// checkout's file system folds onto an existing entry names that entry in
// `other`.
func TestProposalRefusalRoundTripsAliasesExistingWithOther(t *testing.T) {
	data := []byte(`{"ok":false,"error":"proposal refused: 'README.md' aliases 'readme.md' in the checkout","refusal":{"reason":"aliases_existing","path":"README.md","other":"readme.md"}}`)
	var got ProposeResponse
	roundTrip(t, data, &got)
	r := got.Refusal
	if r == nil || r.Reason == nil || *r.Reason != RefusalAliasesExisting ||
		r.Path == nil || *r.Path != "README.md" ||
		r.Other == nil || *r.Other != "readme.md" {
		t.Fatalf("refusal: %+v", r)
	}
}

// A reason this package does not name still decodes, as a refusal.
func TestProposalRefusalDecodesAnUnknownReason(t *testing.T) {
	var got ProposeResponse
	if err := strictDecode([]byte(`{"ok":false,"refusal":{"reason":"from_the_future"}}`), &got); err != nil {
		t.Fatal(err)
	}
	if got.OK == nil || *got.OK || got.Refusal == nil || got.Refusal.Reason == nil ||
		*got.Refusal.Reason != RefusalReason("from_the_future") {
		t.Fatalf("an unknown reason decodes as a refusal; got %+v", got)
	}
}

// Every propose fixture's request writes decode into ProposedWrite and
// encode back to the same JSON: kinds, bases, modes, and an omitted kind.
func TestProposedWriteRoundTripsEveryProposeRequest(t *testing.T) {
	for _, name := range []string{
		"propose", "propose_merged", "propose_tree", "propose_refused",
		"propose_applied", "propose_structural", "propose_create_stale",
	} {
		t.Run(name, func(t *testing.T) {
			var req struct {
				Op     string          `json:"op"`
				Writes json.RawMessage `json:"writes"`
			}
			if err := strictDecode(loadFixture(t, name).Request, &req); err != nil {
				t.Fatal(err)
			}
			if req.Op != "propose" {
				t.Fatalf("op: %q", req.Op)
			}
			var writes []ProposedWrite
			roundTrip(t, req.Writes, &writes)
			if len(writes) == 0 {
				t.Fatal("a propose fixture writes something")
			}
		})
	}
}

func TestProposedWriteEncodesWhatTheDaemonReads(t *testing.T) {
	empty := ""
	mode := ModeExecutable
	got, err := json.Marshal([]ProposedWrite{
		{Path: "a.txt", Base: BaseAny, Content: &empty},
		{Path: "run.sh", Base: BaseAbsent, Content: &empty, Kind: WriteKindCreate, Mode: &mode},
		{Path: "old.txt", Base: BaseAny, Kind: WriteKindDelete},
	})
	if err != nil {
		t.Fatal(err)
	}
	want := `[{"path":"a.txt","base":"any","content":""},` +
		`{"path":"run.sh","base":"absent","content":"","kind":"create","mode":"100755"},` +
		`{"path":"old.txt","base":"any","kind":"delete"}]`
	if string(got) != want {
		t.Fatalf("encoding:\n want %s\n  got %s", want, got)
	}
}

func TestReadSourceResponseRoundTrips(t *testing.T) {
	var got ReadSourceResponse
	roundTrip(t, loadFixture(t, "read_source").Response, &got)
	if got.OK == nil || !*got.OK || got.Content == nil || *got.Content != "fn main() {}\n" ||
		got.BlobHash == nil || len(*got.BlobHash) != 64 ||
		got.TreeRoot == nil || len(*got.TreeRoot) != 64 ||
		got.Mode == nil || *got.Mode != ModeRegular {
		t.Fatalf("read_source: %+v", got)
	}
}

// Bead ley-line-open-bc7df6: the daemon answers an empty file with
// `content: ""`; it decodes as a present empty string, not nil, and
// re-encodes with the field kept.
func TestReadSourceResponseKeepsAnEmptyFile(t *testing.T) {
	const emptyBlob = "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262"
	data := []byte(`{"ok":true,"content":"","blob_hash":"` + emptyBlob +
		`","tree_root":"` + emptyBlob + `","mode":"100644"}`)
	var got ReadSourceResponse
	roundTrip(t, data, &got)
	if got.Content == nil || *got.Content != "" || got.BlobHash == nil || *got.BlobHash != emptyBlob {
		t.Fatalf("an empty file: %+v", got)
	}
}

// Bead ley-line-open-246526: status names the daemon's --source root, and
// a daemon without one sends no source_root.
func TestStatusResponseDecodesSourceRoot(t *testing.T) {
	var got StatusResponse
	if err := strictDecode(loadFixture(t, "status").Response, &got); err != nil {
		t.Fatal(err)
	}
	if got.SourceRoot == nil || *got.SourceRoot != "/tmp/test/source" {
		t.Fatalf("source_root: want /tmp/test/source, got %v", got.SourceRoot)
	}
	var bare StatusResponse
	if err := strictDecode([]byte(`{"ok":true,"phase":"ready"}`), &bare); err != nil {
		t.Fatal(err)
	}
	if bare.SourceRoot != nil {
		t.Fatalf("no source_root on the wire decodes as nil; got %q", *bare.SourceRoot)
	}
}

// snake spells a capnp enumerant name (`notADirectory`) the way the
// schema's `$Json.name` annotations do (`not_a_directory`).
func snake(name string) string {
	var b strings.Builder
	for _, r := range name {
		if unicode.IsUpper(r) {
			b.WriteByte('_')
			r = unicode.ToLower(r)
		}
		b.WriteRune(r)
	}
	return b.String()
}

// The Go constants list every enumerant of the generated capnp enums, in
// ordinal order, with the schema's wire spelling: a reason or kind added
// to daemon.capnp without a constant here fails this test.
func TestEnumSpellingsMatchTheSchema(t *testing.T) {
	reasons := []RefusalReason{
		RefusalStale, RefusalOutsideGrant, RefusalNoSuchFile, RefusalDuplicatePath,
		RefusalDifferentRoots, RefusalNoTreeRoot, RefusalRetiredSyntax, RefusalMalformedBase,
		RefusalRetiredNoStoredBlob, RefusalEmpty, RefusalRetiredNoLanguage, RefusalMalformedPath,
		RefusalPathConflict, RefusalExists, RefusalNotEmpty, RefusalNotADirectory,
		RefusalIsADirectory, RefusalAliasesExisting,
	}
	for i, want := range reasons {
		name := daemon.RefusalReason(i).String()
		if name == "" || RefusalReason(snake(name)) != want {
			t.Fatalf("RefusalReason ordinal %d is %q (wire %q), constant is %q", i, name, snake(name), want)
		}
	}
	if extra := daemon.RefusalReason(len(reasons)).String(); extra != "" {
		t.Fatalf("daemon.capnp has RefusalReason %q (ordinal %d) with no constant", extra, len(reasons))
	}

	kinds := []WriteKind{WriteKindReplace, WriteKindCreate, WriteKindDelete, WriteKindMkdir, WriteKindRmdir}
	for i, want := range kinds {
		name := daemon.WriteKind(i).String()
		if name == "" || WriteKind(snake(name)) != want {
			t.Fatalf("WriteKind ordinal %d is %q, constant is %q", i, name, want)
		}
	}
	if extra := daemon.WriteKind(len(kinds)).String(); extra != "" {
		t.Fatalf("daemon.capnp has WriteKind %q (ordinal %d) with no constant", extra, len(kinds))
	}
}
