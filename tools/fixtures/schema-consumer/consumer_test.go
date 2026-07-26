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

	event := true
	topic := "daemon.snapshot"
	_ = wire.Event{
		Event: &event,
		Topic: &topic,
		Data:  json.RawMessage(`{}`),
	}
}
