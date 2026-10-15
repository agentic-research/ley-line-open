package daemon

// Bead ley-line-open-47fb03: a proposal's writes carry a kind. The field is
// additive: a ProposedWrite that never sets it reads as `replace`, the only
// kind before it existed, and each kind round-trips through a message with
// its JSON spelling.

import (
	"testing"

	capnp "capnproto.org/go/capnp/v3"
)

func TestProposedWriteKindDefaultsToReplaceAndRoundTrips(t *testing.T) {
	for _, kind := range []WriteKind{WriteKind_replace, WriteKind_create, WriteKind_delete} {
		msg, seg, err := capnp.NewMessage(capnp.SingleSegment(nil))
		if err != nil {
			t.Fatal(err)
		}
		w, err := NewRootProposedWrite(seg)
		if err != nil {
			t.Fatal(err)
		}
		if w.Kind() != WriteKind_replace {
			t.Fatalf("an unset kind reads %v, want replace", w.Kind())
		}
		w.SetKind(kind)
		data, err := msg.Marshal()
		if err != nil {
			t.Fatal(err)
		}
		decoded, err := capnp.Unmarshal(data)
		if err != nil {
			t.Fatal(err)
		}
		back, err := ReadRootProposedWrite(decoded)
		if err != nil {
			t.Fatal(err)
		}
		if back.Kind() != kind {
			t.Fatalf("kind %v decoded as %v", kind, back.Kind())
		}
	}
	for kind, want := range map[WriteKind]string{
		WriteKind_replace: "replace",
		WriteKind_create:  "create",
		WriteKind_delete:  "delete",
	} {
		if got := kind.String(); got != want {
			t.Fatalf("kind %d spells %q, want %q", kind, got, want)
		}
	}
	for reason, want := range map[RefusalReason]string{
		RefusalReason_noLanguage:    "noLanguage",
		RefusalReason_malformedPath: "malformedPath",
		RefusalReason_pathConflict:  "pathConflict",
	} {
		if got := reason.String(); got != want {
			t.Fatalf("reason %d spells %q, want %q", reason, got, want)
		}
	}
}
