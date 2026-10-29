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
	for _, kind := range []WriteKind{WriteKind_replace, WriteKind_create, WriteKind_delete, WriteKind_mkdir, WriteKind_rmdir} {
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
		WriteKind_mkdir:   "mkdir",
		WriteKind_rmdir:   "rmdir",
	} {
		if got := kind.String(); got != want {
			t.Fatalf("kind %d spells %q, want %q", kind, got, want)
		}
	}
	// ADR-0045: `syntax`, `noLanguage` and `noStoredBlob` are retired (their
	// ordinals stay, renamed, and are never sent); the directory reasons
	// are new.
	for reason, want := range map[RefusalReason]string{
		RefusalReason_retiredSyntax:       "retiredSyntax",
		RefusalReason_retiredNoLanguage:   "retiredNoLanguage",
		RefusalReason_retiredNoStoredBlob: "retiredNoStoredBlob",
		RefusalReason_malformedPath:       "malformedPath",
		RefusalReason_pathConflict:        "pathConflict",
		RefusalReason_exists:              "exists",
		RefusalReason_notEmpty:            "notEmpty",
		RefusalReason_notADirectory:       "notADirectory",
		RefusalReason_isADirectory:        "isADirectory",
	} {
		if got := reason.String(); got != want {
			t.Fatalf("reason %d spells %q, want %q", reason, got, want)
		}
	}
}

// ADR-0045 T4, T5: a write names an optional git mode, and a stale entry
// carries the base line ranges its merge conflicted on.
func TestProposedWriteModeAndStaleConflictsRoundTrip(t *testing.T) {
	msg, seg, err := capnp.NewMessage(capnp.SingleSegment(nil))
	if err != nil {
		t.Fatal(err)
	}
	w, err := NewRootProposedWrite(seg)
	if err != nil {
		t.Fatal(err)
	}
	if w.HasMode() {
		t.Fatal("an unset mode reads as set")
	}
	if err := w.SetMode("100755"); err != nil {
		t.Fatal(err)
	}
	s, err := NewStaleRefusal(seg)
	if err != nil {
		t.Fatal(err)
	}
	ranges, err := s.NewConflicts(1)
	if err != nil {
		t.Fatal(err)
	}
	ranges.At(0).SetStart(4)
	ranges.At(0).SetEnd(5)
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
	if mode, err := back.Mode(); err != nil || mode != "100755" {
		t.Fatalf("mode decoded as %q (%v)", mode, err)
	}
	got, err := s.Conflicts()
	if err != nil || got.Len() != 1 || got.At(0).Start() != 4 || got.At(0).End() != 5 {
		t.Fatalf("conflicts decoded as %v (%v)", got, err)
	}
}
