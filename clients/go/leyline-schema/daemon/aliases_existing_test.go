package daemon

// Bead ley-line-open-23a26c: `aliasesExisting` is a new RefusalReason, and
// ProposalRefusal gains `other`. Both are additive. A refusal carries
// `ok: false` whatever its reason, so a consumer built before the enumerant
// existed reads the reply as a refusal: on the Cap'n Proto wire the reason
// is an ordinal past its last known one, and on the JSON wire it is a string
// it does not recognize. In neither case does the reply read as success.

import (
	"encoding/json"
	"testing"

	capnp "capnproto.org/go/capnp/v3"
)

// aliasesReply is a propose reply refusing `Readme.md` as an alias of
// `README.md`, with the reason given as a raw ordinal.
func aliasesReply(t *testing.T, reason RefusalReason) ProposeResponse {
	t.Helper()
	msg, seg, err := capnp.NewMessage(capnp.SingleSegment(nil))
	if err != nil {
		t.Fatal(err)
	}
	r, err := NewRootProposeResponse(seg)
	if err != nil {
		t.Fatal(err)
	}
	r.SetOk(false)
	refusal, err := r.NewRefusal()
	if err != nil {
		t.Fatal(err)
	}
	refusal.SetReason(reason)
	if err := refusal.SetPath("Readme.md"); err != nil {
		t.Fatal(err)
	}
	if err := refusal.SetOther("README.md"); err != nil {
		t.Fatal(err)
	}
	data, err := msg.Marshal()
	if err != nil {
		t.Fatal(err)
	}
	decoded, err := capnp.Unmarshal(data)
	if err != nil {
		t.Fatal(err)
	}
	back, err := ReadRootProposeResponse(decoded)
	if err != nil {
		t.Fatal(err)
	}
	return back
}

func TestAliasesExistingRoundTripsWithBothPaths(t *testing.T) {
	r := aliasesReply(t, RefusalReason_aliasesExisting)
	if r.Ok() {
		t.Fatal("a refusal reads as success")
	}
	refusal, err := r.Refusal()
	if err != nil {
		t.Fatal(err)
	}
	if got := refusal.Reason(); got != RefusalReason_aliasesExisting || got.String() != "aliasesExisting" {
		t.Fatalf("reason decoded as %v", got)
	}
	if path, _ := refusal.Path(); path != "Readme.md" {
		t.Fatalf("path decoded as %q", path)
	}
	if other, _ := refusal.Other(); other != "README.md" {
		t.Fatalf("other decoded as %q", other)
	}
}

// An older binding sees an ordinal it has no name for: it still decodes,
// and the reply is still not a success.
func TestAnUnknownRefusalReasonStillReadsAsARefusal(t *testing.T) {
	unknown := RefusalReason(RefusalReason_aliasesExisting + 100)
	r := aliasesReply(t, unknown)
	if r.Ok() {
		t.Fatal("a refusal with an unknown reason reads as success")
	}
	refusal, err := r.Refusal()
	if err != nil {
		t.Fatal(err)
	}
	if refusal.Reason() != unknown {
		t.Fatalf("the unknown ordinal decoded as %v", refusal.Reason())
	}
}

// A JSON consumer from before the reason (no `other` field, reasons it
// knows by name) decodes the reply as a refusal it does not recognize.
func TestAnOlderJSONConsumerReadsAliasesExistingAsARefusal(t *testing.T) {
	reply := []byte(`{"ok":false,"refusal":{"reason":"aliases_existing","path":"Readme.md","other":"README.md"},` +
		`"error":"proposal refused: 'Readme.md' and 'README.md' name one entry in the checkout"}`)
	var older struct {
		OK      *bool `json:"ok"`
		Refusal *struct {
			Reason string `json:"reason"`
			Path   string `json:"path"`
		} `json:"refusal"`
	}
	if err := json.Unmarshal(reply, &older); err != nil {
		t.Fatal(err)
	}
	if older.OK == nil || *older.OK {
		t.Fatalf("ok decoded as %v", older.OK)
	}
	if older.Refusal == nil || older.Refusal.Reason != "aliases_existing" || older.Refusal.Path != "Readme.md" {
		t.Fatalf("refusal decoded as %+v", older.Refusal)
	}
}
