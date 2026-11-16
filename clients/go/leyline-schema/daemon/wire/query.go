package wire

import (
	"bytes"
	"encoding/json"
	"fmt"
	"io"
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

// UnmarshalJSON accepts exactly one known union member. It reads the object
// token by token, so a member repeated under the same key (which a map
// decode would collapse, last one winning) is refused too.
func (c *QueryCell) UnmarshalJSON(data []byte) error {
	dec := json.NewDecoder(bytes.NewReader(data))
	if tok, err := dec.Token(); err != nil || tok != json.Delim('{') {
		return fmt.Errorf("query cell: want an object with one of null or text, got %s", data)
	}
	var member string
	var text *string
	for dec.More() {
		tok, err := dec.Token()
		if err != nil {
			return fmt.Errorf("query cell: %w", err)
		}
		name, _ := tok.(string)
		if member != "" {
			return fmt.Errorf("query cell: want exactly one of null or text, got %s", data)
		}
		member = name
		var raw json.RawMessage
		if err := dec.Decode(&raw); err != nil {
			return fmt.Errorf("query cell: %w", err)
		}
		isNull := bytes.Equal(bytes.TrimSpace(raw), []byte("null"))
		switch name {
		case "null":
			if !isNull {
				return fmt.Errorf("query cell: null member carries %s, want null", raw)
			}
		case "text":
			var s string
			if isNull || json.Unmarshal(raw, &s) != nil {
				return fmt.Errorf("query cell: text member carries %s, want a string", raw)
			}
			text = &s
		default:
			return fmt.Errorf("query cell: unknown member %q", name)
		}
	}
	if member == "" {
		return fmt.Errorf("query cell: want exactly one of null or text, got %s", data)
	}
	if tok, err := dec.Token(); err != nil || tok != json.Delim('}') {
		return fmt.Errorf("query cell: malformed object %s", data)
	}
	if _, err := dec.Token(); err != io.EOF {
		return fmt.Errorf("query cell: trailing content after %s", data)
	}
	c.Text = text
	return nil
}

// MarshalJSON writes the cell's one union member.
func (c QueryCell) MarshalJSON() ([]byte, error) {
	if c.Text == nil {
		return []byte(`{"null":null}`), nil
	}
	return json.Marshal(map[string]string{"text": *c.Text})
}
