// Cross-runtime fixture decode (Go side of T8.10, ADR-0014 §F8.6.4).
//
// The Rust side at rs/ll-core/schema-capnp/tests/cross_runtime_fixtures.rs
// asserts that the canonical-encoded bytes for two BindingRecord shapes
// (minimal + realistic) are byte-equal to the committed `.bin` fixtures.
// This test is the Go-consumer half: read those *same* fixtures and
// verify the Go bindings decode them with field-equal values.
//
// When this passes alongside the Rust test, F8.6.4 is mechanized:
// cross-runtime byte-equal canonical encoding *and* cross-runtime
// field-equal decode are both gated by CI. Schema drift that breaks
// either side fails loudly.
//
// Bead: ley-line-open-41867b (Go bindings publish), follows T8.10
// (ley-line-open-6b7d43, the Rust fixture suite that produced the .bin).

package binding_test

import (
	"os"
	"path/filepath"
	"testing"

	capnp "capnproto.org/go/capnp/v3"

	"github.com/agentic-research/ley-line-open/clients/go/leyline-schema/binding"
)

// fixturePath joins the repo-root-relative fixture path against this
// package's directory at clients/go/leyline-schema/binding/. The
// canonical fixtures live in the Rust crate, deliberately un-vendored:
// one source of truth, both runtimes assert against it.
func fixturePath(name string) string {
	return filepath.Join(
		"..", "..", "..", "..",
		"rs", "ll-core", "schema-capnp", "tests", "fixtures",
		name,
	)
}

func readFixture(t *testing.T, name string) *capnp.Message {
	t.Helper()
	bytes, err := os.ReadFile(fixturePath(name))
	if err != nil {
		t.Fatalf("read fixture %s: %v", name, err)
	}
	msg, err := capnp.Unmarshal(bytes)
	if err != nil {
		t.Fatalf("unmarshal fixture %s: %v", name, err)
	}
	return msg
}

// Minimal fixture: every field at default. The Rust producer truncates
// trailing-zero data per canonical encoding, so the on-disk size is
// near-zero. Decode must succeed and every accessor must return the
// type-zero — proving the truncated layout still reads back correctly.
func TestBindingRecordMinimal_Decodes(t *testing.T) {
	msg := readFixture(t, "binding-record-minimal.bin")
	rec, err := binding.ReadRootBindingRecord(msg)
	if err != nil {
		t.Fatalf("ReadRootBindingRecord: %v", err)
	}

	// Defaults: Text fields decode as "" (no error), uint64 as 0.
	for _, c := range []struct {
		name string
		got  func() (string, error)
	}{
		{"TargetNodeId", rec.TargetNodeId},
		{"RefToken", rec.RefToken},
		{"ConstructNodeId", rec.ConstructNodeId},
		{"RefSiteNodeId", rec.RefSiteNodeId},
		{"RefUri", rec.RefUri},
		{"Qualifier", rec.Qualifier},
	} {
		v, err := c.got()
		if err != nil {
			t.Errorf("%s: unexpected error on default fixture: %v", c.name, err)
		}
		if v != "" {
			t.Errorf("%s: want empty default, got %q", c.name, v)
		}
	}
	if rec.ParseGen() != 0 {
		t.Errorf("ParseGen: want 0, got %d", rec.ParseGen())
	}
}

// Realistic fixture: every field populated by the Rust producer in
// build_binding_record_realistic(). Field values must match exactly —
// these constants are the Go side of the cross-runtime contract; if
// either side drifts, this test or the Rust counterpart fails.
func TestBindingRecordRealistic_DecodesAndFieldsMatch(t *testing.T) {
	msg := readFixture(t, "binding-record-realistic.bin")
	rec, err := binding.ReadRootBindingRecord(msg)
	if err != nil {
		t.Fatalf("ReadRootBindingRecord: %v", err)
	}

	// Mirror of build_binding_record_realistic() in
	// rs/ll-core/schema-capnp/tests/cross_runtime_fixtures.rs.
	wantText := []struct {
		name, want string
		got        func() (string, error)
	}{
		{"TargetNodeId", "pkg/auth.go/function_declaration/Validate", rec.TargetNodeId},
		{"RefToken", "Validate", rec.RefToken},
		{"ConstructNodeId", "pkg/main.go/function_declaration", rec.ConstructNodeId},
		{
			"RefSiteNodeId",
			"pkg/main.go/function_declaration/block/expression_list/call_expression/selector_expression/field_identifier",
			rec.RefSiteNodeId,
		},
		{"RefUri", "file:///canon/pkg/main.go", rec.RefUri},
		{"Qualifier", "auth", rec.Qualifier},
	}
	for _, c := range wantText {
		got, err := c.got()
		if err != nil {
			t.Errorf("%s: %v", c.name, err)
			continue
		}
		if got != c.want {
			t.Errorf("%s: want %q, got %q", c.name, c.want, got)
		}
	}

	if got, want := rec.ParseGen(), uint64(42); got != want {
		t.Errorf("ParseGen: want %d, got %d", want, got)
	}

	// Nested struct: Range{ start: Position, end: Position }. Cross-package
	// (binding -> common) decode — this is the load-bearing check that
	// $Go.import annotations wired the cross-package types correctly.
	r, err := rec.RefRange()
	if err != nil {
		t.Fatalf("RefRange: %v", err)
	}
	start, err := r.Start()
	if err != nil {
		t.Fatalf("RefRange.Start: %v", err)
	}
	end, err := r.End()
	if err != nil {
		t.Fatalf("RefRange.End: %v", err)
	}
	if start.Line() != 7 || start.Column() != 11 || start.Byte() != 123 {
		t.Errorf("Start: want (line=7 col=11 byte=123), got (line=%d col=%d byte=%d)",
			start.Line(), start.Column(), start.Byte())
	}
	if end.Line() != 7 || end.Column() != 19 || end.Byte() != 131 {
		t.Errorf("End: want (line=7 col=19 byte=131), got (line=%d col=%d byte=%d)",
			end.Line(), end.Column(), end.Byte())
	}
}

// Sanity: minimal fixture is strictly smaller on disk than realistic.
// Mirrors the Rust-side `minimal_strictly_smaller_than_realistic` test;
// confirms canonical-encoding truncation kept the defaults compact.
func TestFixtureSizes_MinimalSmallerThanRealistic(t *testing.T) {
	min, err := os.ReadFile(fixturePath("binding-record-minimal.bin"))
	if err != nil {
		t.Fatalf("read minimal: %v", err)
	}
	real, err := os.ReadFile(fixturePath("binding-record-realistic.bin"))
	if err != nil {
		t.Fatalf("read realistic: %v", err)
	}
	if len(min) >= len(real) {
		t.Errorf("expected minimal (%d bytes) < realistic (%d bytes)", len(min), len(real))
	}
}
