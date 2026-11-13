package wire

// `query` row JSON (bead ley-line-open-0a18bc). Each row is a list of
// QueryCell unions in `columns` order. The payloads are the shared daemon
// protocol fixtures, which the Rust test
// rs/ll-open/cli-lib/tests/query_read_only_test.rs::the_protocol_fixture_rows_are_what_the_daemon_sends
// proves the daemon emits, and which tools/check_daemon_fixture_codec.py
// proves the schema's reference JSON codec reads.

import (
	"encoding/json"
	"testing"
)

func decodeQueryFixture(t *testing.T, name string) QueryResponse {
	t.Helper()
	var got QueryResponse
	roundTrip(t, loadFixture(t, name).Response, &got)
	if got.OK == nil || !*got.OK {
		t.Fatalf("%s: want ok:true, got %v", name, got.OK)
	}
	for i, row := range got.Rows {
		if len(row) != len(got.Columns) {
			t.Fatalf("%s: row %d has %d cells for %d columns", name, i, len(row), len(got.Columns))
		}
	}
	return got
}

func TestQueryResponseKeepsEveryValueOfARepeatedColumnName(t *testing.T) {
	got := decodeQueryFixture(t, "query_duplicate_columns")
	if len(got.Columns) != 2 || got.Columns[0] != "id" || got.Columns[1] != "id" {
		t.Fatalf("columns: want [id id], got %v", got.Columns)
	}
	if len(got.Rows) != 1 {
		t.Fatalf("rows: want 1, got %d", len(got.Rows))
	}
	row := got.Rows[0]
	if row[0].IsNull() || *row[0].Text != "1" || row[1].IsNull() || *row[1].Text != "2" {
		t.Fatalf("row 0: want [1 2], got %+v", row)
	}
}

func TestQueryResponseKeepsNullDistinctFromTheEmptyString(t *testing.T) {
	got := decodeQueryFixture(t, "query_null")
	if len(got.Rows) != 1 {
		t.Fatalf("rows: want 1, got %d", len(got.Rows))
	}
	row := got.Rows[0]
	if !row[0].IsNull() {
		t.Fatalf("a NULL cell decodes as null; got text %q", *row[0].Text)
	}
	if row[1].IsNull() || *row[1].Text != "" {
		t.Fatalf("an empty-string cell decodes as non-null empty text; got %+v", row[1])
	}
}

func TestQueryResponseDecodesTheSingleColumnFixture(t *testing.T) {
	got := decodeQueryFixture(t, "query")
	if len(got.Rows) != 1 || got.Rows[0][0].IsNull() || *got.Rows[0][0].Text != "1" {
		t.Fatalf("rows: want [[{text 1}]], got %+v", got.Rows)
	}
}

// A cell that is not exactly one known member is refused, never read as a
// value: a bare null or string (the pre-cell shape), an empty or two-member
// object, a member this type does not know, or a member carrying the wrong
// JSON type.
func TestQueryCellRefusesWhatIsNotOneKnownMember(t *testing.T) {
	for _, bad := range []string{
		`null`, `"1"`, `{}`, `{"null": null, "text": "1"}`, `{"integer": "1"}`,
		`{"null": 0}`, `{"text": null}`, `{"text": 1}`,
	} {
		var c QueryCell
		if err := json.Unmarshal([]byte(bad), &c); err == nil {
			t.Errorf("%s decoded as %+v, want a refusal", bad, c)
		}
	}
}
