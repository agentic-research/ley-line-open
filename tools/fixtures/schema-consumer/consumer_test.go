package consumer

import (
	"encoding/json"
	"testing"

	"github.com/agentic-research/ley-line-open/clients/go/leyline-schema/daemon/wire"
)

func TestCanonicalDaemonWireAPI(t *testing.T) {
	input := []byte(`{"ok":true,"node":{"id":"node-1","size":"42"}}`)
	var got wire.GetNodeResponse
	if err := json.Unmarshal(input, &got); err != nil {
		t.Fatal(err)
	}
	if got.OK == nil || !*got.OK || got.Node == nil ||
		got.Node.ID == nil || *got.Node.ID != "node-1" ||
		got.Node.Size == nil || *got.Node.Size != 42 {
		t.Fatalf("unexpected typed response: %#v", got)
	}

	// read_source / propose / status types (beads ley-line-open-247489,
	// ley-line-open-246526) are part of the external API.
	var status wire.StatusResponse
	if err := json.Unmarshal([]byte(`{"ok":true,"source_root":"/src"}`), &status); err != nil {
		t.Fatal(err)
	}
	if status.SourceRoot == nil || *status.SourceRoot != "/src" {
		t.Fatalf("unexpected status: %#v", status)
	}
	var proposed wire.ProposeResponse
	if err := json.Unmarshal([]byte(`{"ok":false,"refusal":{"reason":"stale","moved":[{"path":"a","expected":"absent","current":"absent","conflicts":[{"start":0,"end":1}]}]}}`), &proposed); err != nil {
		t.Fatal(err)
	}
	if proposed.Refusal == nil || proposed.Refusal.Reason == nil ||
		*proposed.Refusal.Reason != wire.RefusalStale ||
		len(proposed.Refusal.Moved) != 1 || len(proposed.Refusal.Moved[0].Conflicts) != 1 {
		t.Fatalf("unexpected propose response: %#v", proposed)
	}
	_ = wire.ReadSourceResponse{}
	_ = wire.ProposedWrite{Path: "a", Base: wire.BaseAbsent, Kind: wire.WriteKindCreate}

	event := true
	topic := "daemon.snapshot"
	_ = wire.Event{
		Event: &event,
		Topic: &topic,
		Data:  json.RawMessage(`{}`),
	}
}
