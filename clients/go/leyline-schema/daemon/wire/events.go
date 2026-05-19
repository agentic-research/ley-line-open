// Event envelope + per-topic payload types for the daemon's pushed-event
// JSON wire surface (line-delimited JSON over UDS).
//
// The capnp `Event` struct in `daemon.capnp.go` defines the envelope at
// the capnp layer; the JSON wire that consumers actually parse has the
// same shape, but the `data` field is a JSON object (not a string-encoded
// nested JSON). This file types both layers:
//
//   - [Event] is the JSON envelope as it appears on the wire.
//   - The `data` field is opaque [json.RawMessage]; per-topic types
//     ([SheafInvalidatePayload], [SheafTopologyPayload], etc.) describe
//     what each topic puts inside it.
//   - [DecodeEvent] is the type-dispatch helper that returns the
//     envelope plus the typed payload for known topics.

package wire

import (
	"encoding/json"
	"fmt"
)

// Event is the JSON envelope wrapping every pushed event over the
// daemon's UDS subscribe stream. The `event: true` discriminator
// distinguishes pushed events from op responses on the same socket.
//
// The capnp counterpart is `daemon.capnp::Event` (see Event_TypeID
// 0xb8a347db1f686319 in `daemon.capnp.go`); the JSON-wire shape and
// capnp struct are kept in lockstep at release time. If you need to
// dispatch on topic, prefer [DecodeEvent] over hand-rolled topic
// switching — it gives you the typed payload in one call.
type Event struct {
	// Event is always true on pushed events; absent or false marks an
	// op response. Consumers MUST gate on this before parsing as an
	// Event because the same socket carries both shapes.
	Event *bool `json:"event"`

	// Seq is the monotonically increasing sequence number (Lamport
	// timestamp) the daemon assigns at dispatch. Quoted-string per
	// the capnp_json u64 convention.
	Seq *uint64 `json:"seq,string"`

	// Topic is the dot-separated hierarchical topic name. Examples:
	// "sheaf.invalidate", "sheaf.topology", "daemon.snapshot",
	// "daemon.head.changed", "daemon.reparse.complete".
	Topic *string `json:"topic"`

	// Source identifies the emitter. All daemon-internal events
	// currently use "leyline"; external clients calling `emit` set
	// their own source.
	Source *string `json:"source"`

	// Data is the topic-specific payload. Decoded as a raw message so
	// callers can dispatch on Topic, then unmarshal into the typed
	// payload struct ([DecodeEvent] does this in one step).
	Data json.RawMessage `json:"data"`
}

// SheafInvalidatePayload is the `data` payload for the `sheaf.invalidate`
// topic — emitted by `op_sheaf_invalidate` after the daemon-side cascade
// runs. Carries the changed roots plus their structurally-affected
// neighbors (whatever the δ⁰ / co-change machinery returned).
//
// As of v0.4.3, both `generation` and `prior_generation` are emitted as
// quoted-string JSON values (capnp_json u64 convention). Consumers
// reading the response of `sheaf_invalidate` ([SheafInvalidateResponse])
// AND this event payload see the same encoding.
type SheafInvalidatePayload struct {
	Invalidated     []uint32 `json:"invalidated"`
	Count           *uint32  `json:"count"`
	Generation      *uint64  `json:"generation,string"`
	PriorGeneration *uint64  `json:"prior_generation,string"`
}

// SheafTopologyPayload is the `data` payload for the `sheaf.topology`
// topic. Two daemon ops emit on this topic with slightly different
// payload shapes; the `kind` discriminator distinguishes them:
//
//   - Emitted by `op_sheaf_set_topology` with no `kind` field — carries
//     Regions, Restrictions, DeltaZeroMode (one-shot replacement of the
//     whole topology).
//   - Emitted by `op_sheaf_update_topology` with `kind: "update"` —
//     carries Affected, Generation, PriorGeneration, DefectAfter
//     (incremental delta + the structurally-affected radius-1 region
//     set the consumer must evict).
//
// All fields are optional; consumers MUST gate on Kind (or on which
// fields are present) before assuming a particular shape. Future
// emit-site additions are non-breaking inside this struct.
type SheafTopologyPayload struct {
	// Kind is "update" for `sheaf_update_topology` emits, absent /
	// empty for `sheaf_set_topology` emits.
	Kind *string `json:"kind,omitempty"`

	// Set-topology fields.
	Regions       *uint32 `json:"regions,omitempty"`
	Restrictions  *uint32 `json:"restrictions,omitempty"`
	DeltaZeroMode *bool   `json:"delta_zero_mode,omitempty"`

	// Update-topology fields.
	Affected        []uint32 `json:"affected,omitempty"`
	Generation      *uint64  `json:"generation,string,omitempty"`
	PriorGeneration *uint64  `json:"prior_generation,string,omitempty"`
	DefectAfter     *float64 `json:"defect_after,omitempty"`
}

// DaemonSnapshotPayload is the `data` payload for the `daemon.snapshot`
// topic — emitted by the periodic snapshot loop in `cmd_daemon.rs`.
// Carries no fields today; reserved for future timing / size info.
type DaemonSnapshotPayload struct{}

// DaemonHeadChangedPayload is the `data` payload for the
// `daemon.head.changed` topic — emitted by the git watcher loop when
// `git rev-parse HEAD` returns a different SHA than the previous tick.
type DaemonHeadChangedPayload struct {
	OldSHA *string `json:"old_sha"`
	NewSHA *string `json:"new_sha"`
}

// DaemonFilesChangedPayload is the `data` payload for the
// `daemon.files.changed` topic — emitted by the git watcher loop when
// `git status --porcelain` returns a different dirty-set than the
// previous tick.
type DaemonFilesChangedPayload struct {
	Paths []string `json:"paths"`
}

// DaemonReparseCompletePayload is the `data` payload for the
// `daemon.reparse.complete` topic — emitted by the watcher loop after
// an incremental reparse finishes. Parsed / Deleted carry quoted-string
// u64 values per the capnp_json convention (v0.4.3 fix).
type DaemonReparseCompletePayload struct {
	Parsed       *uint64  `json:"parsed,string"`
	Deleted      *uint64  `json:"deleted,string"`
	ChangedFiles []string `json:"changed_files"`
}

// DaemonOpPayload is the `data` payload for the generic `daemon.<op>`
// topic — emitted by the UDS dispatcher after every state-changing op
// (see `is_state_changing` in `daemon/ops.rs`). Carries just the op
// name; the actual mutation has its own per-op response.
type DaemonOpPayload struct {
	Op *string `json:"op"`
}

// DecodeEvent parses a line-delimited JSON event line into the
// [Event] envelope plus its typed payload. The returned `payload` is
// one of the concrete `*Payload` types in this file, depending on
// `Event.Topic`; for unknown topics, payload is `json.RawMessage`
// (the raw `data` field) so callers can still introspect or forward.
//
// Returns an error if `b` is not valid JSON, if the envelope's
// `event` field is not true, or if the payload doesn't match the
// schema for its declared topic.
func DecodeEvent(b []byte) (Event, any, error) {
	var ev Event
	if err := json.Unmarshal(b, &ev); err != nil {
		return Event{}, nil, fmt.Errorf("decode event envelope: %w", err)
	}
	if ev.Event == nil || !*ev.Event {
		return ev, nil, fmt.Errorf(
			"decode event: line is not an event (missing `event:true` discriminator)",
		)
	}
	if ev.Topic == nil {
		return ev, nil, fmt.Errorf("decode event: envelope missing `topic` field")
	}
	payload, err := decodePayload(*ev.Topic, ev.Data)
	if err != nil {
		return ev, nil, fmt.Errorf("decode event payload (topic %q): %w", *ev.Topic, err)
	}
	return ev, payload, nil
}

// decodePayload routes a raw `data` blob to the typed payload struct
// matching its topic. Unknown topics fall through to returning the raw
// `json.RawMessage` — callers can dispatch on the empty topic-arm or
// forward the blob as-is.
func decodePayload(topic string, data json.RawMessage) (any, error) {
	switch topic {
	case "sheaf.invalidate":
		var p SheafInvalidatePayload
		if err := json.Unmarshal(data, &p); err != nil {
			return nil, err
		}
		return p, nil
	case "sheaf.topology":
		var p SheafTopologyPayload
		if err := json.Unmarshal(data, &p); err != nil {
			return nil, err
		}
		return p, nil
	case "daemon.snapshot":
		var p DaemonSnapshotPayload
		if err := json.Unmarshal(data, &p); err != nil {
			return nil, err
		}
		return p, nil
	case "daemon.head.changed":
		var p DaemonHeadChangedPayload
		if err := json.Unmarshal(data, &p); err != nil {
			return nil, err
		}
		return p, nil
	case "daemon.files.changed":
		var p DaemonFilesChangedPayload
		if err := json.Unmarshal(data, &p); err != nil {
			return nil, err
		}
		return p, nil
	case "daemon.reparse.complete":
		var p DaemonReparseCompletePayload
		if err := json.Unmarshal(data, &p); err != nil {
			return nil, err
		}
		return p, nil
	default:
		// Generic daemon.<op> topics (load, reparse, flush, snapshot,
		// enrich — see ops.rs::STATE_CHANGING_OPS): they all carry
		// {op: NAME}. Treat any `daemon.*` not matched above as that
		// shape; truly unknown topics fall through to the raw message.
		if len(topic) > len("daemon.") && topic[:len("daemon.")] == "daemon." {
			var p DaemonOpPayload
			if err := json.Unmarshal(data, &p); err != nil {
				return nil, err
			}
			return p, nil
		}
		return data, nil
	}
}
