package wire

import (
	"encoding/json"
	"testing"
)

// TestDecodeEvent_SheafInvalidate exercises the canonical happy path:
// a sheaf.invalidate event with the v0.4.3 wire shape (data nested,
// generation + prior_generation as quoted strings) decodes into the
// typed payload with no lossy coercion. Pre-typed mache parsed these
// with parseUint64 that silently returned 0 on string values; this
// test pins that the typed path doesn't suffer the same failure mode.
func TestDecodeEvent_SheafInvalidate(t *testing.T) {
	line := []byte(`{
		"event": true,
		"seq": "6",
		"topic": "sheaf.invalidate",
		"source": "leyline",
		"data": {
			"invalidated": [1, 2],
			"count": 2,
			"generation": "1",
			"prior_generation": "0"
		}
	}`)

	ev, payload, err := DecodeEvent(line)
	if err != nil {
		t.Fatalf("DecodeEvent: %v", err)
	}

	if ev.Event == nil || !*ev.Event {
		t.Errorf("envelope.event must be true; got %v", ev.Event)
	}
	if ev.Seq == nil || *ev.Seq != 6 {
		t.Errorf("envelope.seq: want 6, got %v", ev.Seq)
	}
	if ev.Topic == nil || *ev.Topic != "sheaf.invalidate" {
		t.Errorf("envelope.topic: want sheaf.invalidate, got %v", ev.Topic)
	}

	sip, ok := payload.(SheafInvalidatePayload)
	if !ok {
		t.Fatalf("payload must be SheafInvalidatePayload, got %T", payload)
	}
	if len(sip.Invalidated) != 2 || sip.Invalidated[0] != 1 || sip.Invalidated[1] != 2 {
		t.Errorf("Invalidated: want [1 2], got %v", sip.Invalidated)
	}
	if sip.Count == nil || *sip.Count != 2 {
		t.Errorf("Count: want 2, got %v", sip.Count)
	}
	if sip.Generation == nil || *sip.Generation != 1 {
		t.Errorf("Generation: want 1, got %v", sip.Generation)
	}
	if sip.PriorGeneration == nil || *sip.PriorGeneration != 0 {
		t.Errorf("PriorGeneration: want 0, got %v", sip.PriorGeneration)
	}
}

// TestDecodeEvent_SheafTopology_SetTopologyShape: sheaf.topology emitted
// by op_sheaf_set_topology has no `kind` field. Discriminator: Kind nil.
func TestDecodeEvent_SheafTopology_SetTopologyShape(t *testing.T) {
	line := []byte(`{
		"event": true,
		"seq": "4",
		"topic": "sheaf.topology",
		"source": "leyline",
		"data": {"regions": 2, "restrictions": 1, "delta_zero_mode": true}
	}`)

	_, payload, err := DecodeEvent(line)
	if err != nil {
		t.Fatalf("DecodeEvent: %v", err)
	}
	stp, ok := payload.(SheafTopologyPayload)
	if !ok {
		t.Fatalf("payload must be SheafTopologyPayload, got %T", payload)
	}
	if stp.Kind != nil {
		t.Errorf("set-topology emit must have nil Kind; got %v", *stp.Kind)
	}
	if stp.Regions == nil || *stp.Regions != 2 {
		t.Errorf("Regions: want 2, got %v", stp.Regions)
	}
	if stp.Restrictions == nil || *stp.Restrictions != 1 {
		t.Errorf("Restrictions: want 1, got %v", stp.Restrictions)
	}
	if stp.DeltaZeroMode == nil || !*stp.DeltaZeroMode {
		t.Errorf("DeltaZeroMode: want true, got %v", stp.DeltaZeroMode)
	}
}

// TestDecodeEvent_SheafTopology_UpdateTopologyShape: sheaf.topology emitted
// by op_sheaf_update_topology has kind="update". Discriminator: Kind == "update".
func TestDecodeEvent_SheafTopology_UpdateTopologyShape(t *testing.T) {
	line := []byte(`{
		"event": true,
		"seq": "8",
		"topic": "sheaf.topology",
		"source": "leyline",
		"data": {
			"kind": "update",
			"affected": [7, 11, 13],
			"generation": "3",
			"prior_generation": "2",
			"defect_after": 0.0042
		}
	}`)

	_, payload, err := DecodeEvent(line)
	if err != nil {
		t.Fatalf("DecodeEvent: %v", err)
	}
	stp, ok := payload.(SheafTopologyPayload)
	if !ok {
		t.Fatalf("payload must be SheafTopologyPayload, got %T", payload)
	}
	if stp.Kind == nil || *stp.Kind != "update" {
		t.Errorf("Kind: want 'update', got %v", stp.Kind)
	}
	if len(stp.Affected) != 3 {
		t.Errorf("Affected: want 3 entries, got %v", stp.Affected)
	}
	if stp.Generation == nil || *stp.Generation != 3 {
		t.Errorf("Generation: want 3, got %v", stp.Generation)
	}
	if stp.PriorGeneration == nil || *stp.PriorGeneration != 2 {
		t.Errorf("PriorGeneration: want 2, got %v", stp.PriorGeneration)
	}
}

// TestDecodeEvent_DaemonReparseComplete: pre-v0.4.3 emitted parsed/deleted
// as raw u64 numbers; v0.4.3 stringified them per capnp_json convention.
// This test pins the post-fix encoding.
func TestDecodeEvent_DaemonReparseComplete(t *testing.T) {
	line := []byte(`{
		"event": true,
		"seq": "12",
		"topic": "daemon.reparse.complete",
		"source": "leyline",
		"data": {"parsed": "42", "deleted": "3", "changed_files": ["a.rs", "b.rs"]}
	}`)
	_, payload, err := DecodeEvent(line)
	if err != nil {
		t.Fatalf("DecodeEvent: %v", err)
	}
	p, ok := payload.(DaemonReparseCompletePayload)
	if !ok {
		t.Fatalf("payload must be DaemonReparseCompletePayload, got %T", payload)
	}
	if p.Parsed == nil || *p.Parsed != 42 {
		t.Errorf("Parsed: want 42, got %v", p.Parsed)
	}
	if p.Deleted == nil || *p.Deleted != 3 {
		t.Errorf("Deleted: want 3, got %v", p.Deleted)
	}
	if len(p.ChangedFiles) != 2 {
		t.Errorf("ChangedFiles: want 2 entries, got %v", p.ChangedFiles)
	}
}

// TestDecodeEvent_HeadAndFilesChanged: head.changed + files.changed are
// pure-string payloads — pinning them as a smoke test that the typed
// shapes load correctly.
func TestDecodeEvent_HeadAndFilesChanged(t *testing.T) {
	headLine := []byte(`{"event":true,"seq":"5","topic":"daemon.head.changed","source":"leyline","data":{"old_sha":"abc1234","new_sha":"def5678"}}`)
	_, p1, err := DecodeEvent(headLine)
	if err != nil {
		t.Fatalf("DecodeEvent head: %v", err)
	}
	hcp, ok := p1.(DaemonHeadChangedPayload)
	if !ok {
		t.Fatalf("payload must be DaemonHeadChangedPayload, got %T", p1)
	}
	if hcp.OldSHA == nil || *hcp.OldSHA != "abc1234" {
		t.Errorf("OldSHA: want abc1234, got %v", hcp.OldSHA)
	}
	if hcp.NewSHA == nil || *hcp.NewSHA != "def5678" {
		t.Errorf("NewSHA: want def5678, got %v", hcp.NewSHA)
	}

	filesLine := []byte(`{"event":true,"seq":"6","topic":"daemon.files.changed","source":"leyline","data":{"paths":["x/y.rs"]}}`)
	_, p2, err := DecodeEvent(filesLine)
	if err != nil {
		t.Fatalf("DecodeEvent files: %v", err)
	}
	fcp, ok := p2.(DaemonFilesChangedPayload)
	if !ok {
		t.Fatalf("payload must be DaemonFilesChangedPayload, got %T", p2)
	}
	if len(fcp.Paths) != 1 || fcp.Paths[0] != "x/y.rs" {
		t.Errorf("Paths: want [x/y.rs], got %v", fcp.Paths)
	}
}

// TestDecodeEvent_GenericDaemonOp: a daemon.<op> topic not enumerated
// explicitly (e.g., daemon.load, daemon.flush, daemon.reparse) is
// routed to the generic DaemonOpPayload shape.
func TestDecodeEvent_GenericDaemonOp(t *testing.T) {
	line := []byte(`{"event":true,"seq":"3","topic":"daemon.load","source":"leyline","data":{"op":"load"}}`)
	_, payload, err := DecodeEvent(line)
	if err != nil {
		t.Fatalf("DecodeEvent: %v", err)
	}
	dop, ok := payload.(DaemonOpPayload)
	if !ok {
		t.Fatalf("payload must be DaemonOpPayload, got %T", payload)
	}
	if dop.Op == nil || *dop.Op != "load" {
		t.Errorf("Op: want 'load', got %v", dop.Op)
	}
}

// TestDecodeEvent_UnknownTopic: unknown topics return the raw payload
// as json.RawMessage so callers can introspect / forward without losing
// the bytes. Critical for forward-compat: future LLO versions can add
// topics and a pre-typed consumer keeps working.
func TestDecodeEvent_UnknownTopic(t *testing.T) {
	line := []byte(`{"event":true,"seq":"99","topic":"future.unknown","source":"leyline","data":{"arbitrary":"shape"}}`)
	_, payload, err := DecodeEvent(line)
	if err != nil {
		t.Fatalf("DecodeEvent: %v", err)
	}
	raw, ok := payload.(json.RawMessage)
	if !ok {
		t.Fatalf("unknown topic payload must be json.RawMessage, got %T", payload)
	}
	if len(raw) == 0 {
		t.Errorf("raw payload must not be empty")
	}
}

// TestDecodeEvent_RejectsNonEvent: an op-response line (event field
// absent or false) must error. Same socket carries both shapes; the
// `event:true` discriminator is load-bearing.
func TestDecodeEvent_RejectsNonEvent(t *testing.T) {
	opResponse := []byte(`{"ok":true,"generation":"1","invalidated":[1,2],"count":2,"prior_generation":"0"}`)
	_, _, err := DecodeEvent(opResponse)
	if err == nil {
		t.Errorf("DecodeEvent must reject op-response line; got no error")
	}
}

// TestDecodeEvent_RejectsMalformedJSON: garbage input errors at the
// envelope-parse step, not later. Pin the error path.
func TestDecodeEvent_RejectsMalformedJSON(t *testing.T) {
	_, _, err := DecodeEvent([]byte(`{"event":true,"topic":}`))
	if err == nil {
		t.Errorf("DecodeEvent must reject malformed JSON; got no error")
	}
}
