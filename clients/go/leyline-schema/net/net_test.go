// leyline-net frame decode gate (Go side, bead ley-line-open-083344).
//
// Decodes the pinned leyline-net/v1 conformance vectors from
// rs/ll-core/schema-spec/leyline-net/v1/test-vectors/ — BOTH byte
// forms (reference + strict canonical) — through the generated Go
// binding and asserts field equality. Together with the Rust gate
// (rs/ll-core/schema-capnp/tests/leyline_net_vectors.rs, which pins
// the BLAKE3 digests and the encode direction) this proves the
// cross-runtime contract: Rust encodes, Go decodes, values match.
//
// The vectors are committed; regeneration is a deliberate spec-version
// event (see the test-vectors README). If this test fails and the Rust
// gate passes, the Go binding is stale — re-run
// clients/go/leyline-schema/regen.sh and commit the diff.

package net_test

import (
	"bytes"
	"os"
	"path/filepath"
	"testing"

	capnp "capnproto.org/go/capnp/v3"
	net "github.com/agentic-research/ley-line-open/clients/go/leyline-schema/net"
)

func vectorPath(form, name string) string {
	return filepath.Join(
		"..", "..", "..", "..",
		"rs", "ll-core", "schema-spec", "leyline-net", "v1", "test-vectors",
		form, name+".bin",
	)
}

func readVector(t *testing.T, form, name string) *capnp.Message {
	t.Helper()
	b, err := os.ReadFile(vectorPath(form, name))
	if err != nil {
		t.Fatalf("read vector %s/%s: %v\n(regenerate via: cd rs && cargo run -p leyline-schema-capnp --example gen_leyline_net_vectors -- ll-core/schema-spec/leyline-net/v1/test-vectors)", form, name, err)
	}
	msg, err := capnp.Unmarshal(b)
	if err != nil {
		t.Fatalf("unmarshal vector %s/%s: %v", form, name, err)
	}
	return msg
}

var bothForms = []string{"reference", "canonical"}

func TestManifestCanonical_DecodesFromVectors(t *testing.T) {
	for _, form := range bothForms {
		msg := readVector(t, form, "manifest-canonical")
		m, err := net.ReadRootManifest(msg)
		if err != nil {
			t.Fatalf("[%s] root Manifest: %v", form, err)
		}
		if got := m.Sequence(); got != 42 {
			t.Errorf("[%s] sequence = %d, want 42", form, got)
		}
		pk, err := m.PublicKey()
		if err != nil || len(pk) != 32 || pk[0] != 0x11 || pk[31] != 0x11 {
			t.Errorf("[%s] publicKey wrong: %v len=%d", form, err, len(pk))
		}
		sig, err := m.Signature()
		if err != nil || len(sig) != 64 || sig[0] != 0x22 || sig[63] != 0x22 {
			t.Errorf("[%s] signature wrong: %v len=%d", form, err, len(sig))
		}
		ch, err := m.ContentHash()
		if err != nil || len(ch) != 32 || ch[0] != 0x33 || ch[31] != 0x33 {
			t.Errorf("[%s] contentHash wrong: %v len=%d", form, err, len(ch))
		}
	}
}

func TestToolCallBasic_DecodesFromVectors(t *testing.T) {
	for _, form := range bothForms {
		msg := readVector(t, form, "tool-call-basic")
		tc, err := net.ReadRootToolCall(msg)
		if err != nil {
			t.Fatalf("[%s] root ToolCall: %v", form, err)
		}
		if got, _ := tc.UpstreamId(); got != "rosary" {
			t.Errorf("[%s] upstreamId = %q, want rosary", form, got)
		}
		if got, _ := tc.ToolName(); got != "rsry_status" {
			t.Errorf("[%s] toolName = %q, want rsry_status", form, got)
		}
		if got, _ := tc.ArgumentsJson(); !bytes.Equal(got, []byte("{}")) {
			t.Errorf("[%s] argumentsJson = %q, want {}", form, got)
		}
	}
}

func TestToolCallEmpty_DefaultedDataDecodes(t *testing.T) {
	for _, form := range bothForms {
		msg := readVector(t, form, "tool-call-empty")
		tc, err := net.ReadRootToolCall(msg)
		if err != nil {
			t.Fatalf("[%s] root ToolCall: %v", form, err)
		}
		if got, _ := tc.UpstreamId(); got != "" {
			t.Errorf("[%s] upstreamId = %q, want empty", form, got)
		}
		if got, _ := tc.ArgumentsJson(); len(got) != 0 {
			t.Errorf("[%s] omitted argumentsJson must decode empty, got %q", form, got)
		}
	}
}

func TestToolResultMixed_AllVariantsDecode(t *testing.T) {
	for _, form := range bothForms {
		msg := readVector(t, form, "tool-result-mixed")
		tr, err := net.ReadRootToolResult(msg)
		if err != nil {
			t.Fatalf("[%s] root ToolResult: %v", form, err)
		}
		if tr.IsError() {
			t.Errorf("[%s] isError = true, want false", form)
		}
		content, err := tr.Content()
		if err != nil || content.Len() != 4 {
			t.Fatalf("[%s] content: %v len=%d, want 4", form, err, content.Len())
		}

		c0 := content.At(0).Body()
		if c0.Which() != net.Content_body_Which_text {
			t.Fatalf("[%s] content[0] variant %v, want text", form, c0.Which())
		}
		if got, _ := c0.Text(); got != "first" {
			t.Errorf("[%s] content[0] = %q, want first", form, got)
		}

		c1 := content.At(1).Body()
		if c1.Which() != net.Content_body_Which_binary {
			t.Fatalf("[%s] content[1] variant %v, want binary", form, c1.Which())
		}
		bin, err := c1.Binary()
		if err != nil {
			t.Fatalf("[%s] content[1] binary: %v", form, err)
		}
		if got, _ := bin.Data(); !bytes.Equal(got, []byte{1, 2, 3}) {
			t.Errorf("[%s] binary data = %v, want [1 2 3]", form, got)
		}
		if got, _ := bin.MimeType(); got != "application/octet-stream" {
			t.Errorf("[%s] mimeType = %q", form, got)
		}

		c2 := content.At(2).Body()
		if c2.Which() != net.Content_body_Which_resource {
			t.Fatalf("[%s] content[2] variant %v, want resource", form, c2.Which())
		}
		if got, _ := c2.Resource(); !bytes.Equal(got, []byte("opaque2")) {
			t.Errorf("[%s] resource = %q, want opaque2", form, got)
		}

		c3 := content.At(3).Body()
		if c3.Which() != net.Content_body_Which_text {
			t.Fatalf("[%s] content[3] variant %v, want text", form, c3.Which())
		}
		if got, _ := c3.Text(); got != "last" {
			t.Errorf("[%s] content[3] = %q, want last", form, got)
		}
	}
}

func TestToolResultErrorEmpty_DecodesFromVectors(t *testing.T) {
	for _, form := range bothForms {
		msg := readVector(t, form, "tool-result-error-empty")
		tr, err := net.ReadRootToolResult(msg)
		if err != nil {
			t.Fatalf("[%s] root ToolResult: %v", form, err)
		}
		if !tr.IsError() {
			t.Errorf("[%s] isError = false, want true", form)
		}
		content, err := tr.Content()
		if err != nil || content.Len() != 0 {
			t.Errorf("[%s] content: %v len=%d, want 0", form, err, content.Len())
		}
	}
}
