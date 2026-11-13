package wire

import (
	"bytes"
	"encoding/json"
	"fmt"
)

// QueryCell is one value of a `query` result row, the JSON form of
// daemon.capnp's `QueryCell` union (LLO bead ley-line-open-0a18bc). On the
// wire a cell is exactly one of `{"null": null}` (SQL NULL) or
// `{"text": "..."}`; Text is nil for the first and non-nil for the second,
// so NULL stays distinct from the empty string.
//
// Text carries every non-NULL value as text: an integer or real in decimal,
// text as itself, a blob as lowercase hex. Typed members (integer, real,
// blob) are the schema's intended extension; until this type knows one, a
// cell naming it is refused rather than guessed at.
type QueryCell struct {
	Text *string
}

// IsNull reports whether the cell is SQL NULL.
func (c QueryCell) IsNull() bool { return c.Text == nil }

// UnmarshalJSON accepts exactly one known union member.
func (c *QueryCell) UnmarshalJSON(data []byte) error {
	var members map[string]json.RawMessage
	if err := json.Unmarshal(data, &members); err != nil {
		return fmt.Errorf("query cell: %w", err)
	}
	if members == nil || len(members) != 1 {
		return fmt.Errorf("query cell: want exactly one of null or text, got %s", data)
	}
	for name, raw := range members {
		switch name {
		case "null":
			if !bytes.Equal(bytes.TrimSpace(raw), []byte("null")) {
				return fmt.Errorf("query cell: null member carries %s, want null", raw)
			}
			c.Text = nil
		case "text":
			var text string
			if err := json.Unmarshal(raw, &text); err != nil || bytes.Equal(bytes.TrimSpace(raw), []byte("null")) {
				return fmt.Errorf("query cell: text member carries %s, want a string", raw)
			}
			c.Text = &text
		default:
			return fmt.Errorf("query cell: unknown member %q", name)
		}
	}
	return nil
}

// MarshalJSON writes the cell's one union member.
func (c QueryCell) MarshalJSON() ([]byte, error) {
	if c.Text == nil {
		return []byte(`{"null":null}`), nil
	}
	return json.Marshal(map[string]string{"text": *c.Text})
}
