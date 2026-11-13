package wire

// `query` row JSON (bead ley-line-open-0a18bc). Each row is an array in
// `columns` order. The payloads are the shared daemon protocol fixtures,
// which the Rust test
// rs/ll-open/cli-lib/tests/query_read_only_test.rs::the_protocol_fixture_rows_are_what_the_daemon_sends
// proves the daemon emits byte for byte.

import "testing"

func decodeQueryFixture(t *testing.T, name string) QueryResponse {
	t.Helper()
	var got QueryResponse
	roundTrip(t, loadFixture(t, name).Response, &got)
	if got.OK == nil || !*got.OK {
		t.Fatalf("%s: want ok:true, got %v", name, got.OK)
	}
	for i, row := range got.Rows {
		if len(row) != len(got.Columns) {
			t.Fatalf("%s: row %d has %d values for %d columns", name, i, len(row), len(got.Columns))
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
	if row[0] == nil || *row[0] != "1" || row[1] == nil || *row[1] != "2" {
		t.Fatalf("row 0: want [1 2], got %v", row)
	}
}

func TestQueryResponseKeepsNullDistinctFromTheEmptyString(t *testing.T) {
	got := decodeQueryFixture(t, "query_null")
	if len(got.Rows) != 1 {
		t.Fatalf("rows: want 1, got %d", len(got.Rows))
	}
	row := got.Rows[0]
	if row[0] != nil {
		t.Fatalf("a NULL value decodes to nil; got %q", *row[0])
	}
	if row[1] == nil || *row[1] != "" {
		t.Fatalf("an empty string decodes to a non-nil empty string; got %v", row[1])
	}
}

func TestQueryResponseDecodesTheSingleColumnFixture(t *testing.T) {
	got := decodeQueryFixture(t, "query")
	if len(got.Rows) != 1 || got.Rows[0][0] == nil || *got.Rows[0][0] != "1" {
		t.Fatalf("rows: want [[1]], got %v", got.Rows)
	}
}
