package sigma

import (
	"bytes"
	"errors"
	"strings"
	"testing"

	"capnproto.org/go/capnp/v3"
	"github.com/agentic-research/ley-line-open/clients/go/leyline-schema/common"
	"github.com/agentic-research/ley-line-open/clients/go/leyline-schema/head"
)

// headFields is a Head to build: each set field is the bytes its Hash holds.
type headFields struct {
	root, parent, store, parser, parentDigest, retired []byte
}

func setHash(t *testing.T, set func(common.Hash) error, seg *capnp.Segment, b []byte) {
	t.Helper()
	if b == nil {
		return
	}
	h, err := common.NewHash(seg)
	if err != nil {
		t.Fatal(err)
	}
	if len(b) > 0 {
		if err := h.SetBytes(b); err != nil {
			t.Fatal(err)
		}
	}
	if err := set(h); err != nil {
		t.Fatal(err)
	}
}

func buildHead(t *testing.T, f headFields) head.Head {
	t.Helper()
	_, seg, err := capnp.NewMessage(capnp.SingleSegment(nil))
	if err != nil {
		t.Fatal(err)
	}
	h, err := head.NewRootHead(seg)
	if err != nil {
		t.Fatal(err)
	}
	h.SetGeneration(1)
	setHash(t, h.SetRootHash, seg, f.root)
	setHash(t, h.SetParentHash, seg, f.parent)
	setHash(t, h.SetStoreRoot, seg, f.store)
	setHash(t, h.SetParserId, seg, f.parser)
	setHash(t, h.SetParentDigest, seg, f.parentDigest)
	setHash(t, h.SetRetiredTreeRoot, seg, f.retired)
	return h
}

func fill(b byte, n int) []byte { return bytes.Repeat([]byte{b}, n) }

// ClaimOf's refusals mirror LLO's head_claim.rs unit tests case by case
// (bead ley-line-open-a46711): half-stamped, a zero parentDigest, a hash
// field of the wrong length and a missing rootHash are errors; zero or
// empty stamps are unstamped (no claim); retiredTreeRoot wins over all.
func TestClaimOfMirrorsTheRustReader(t *testing.T) {
	stamped := headFields{root: fill(1, 32), parent: fill(0, 32), store: fill(2, 32), parser: fill(3, 32)}

	claim, err := ClaimOf(buildHead(t, stamped))
	if err != nil || claim == nil || claim.StoreRoot != Hash(fill(2, 32)) || claim.ParentDigest != nil {
		t.Fatalf("a stamped head: claim %+v, err %v", claim, err)
	}

	withDigest := stamped
	withDigest.parentDigest = fill(6, 32)
	if claim, err := ClaimOf(buildHead(t, withDigest)); err != nil || claim.ParentDigest == nil || *claim.ParentDigest != Hash(fill(6, 32)) {
		t.Fatalf("a parentDigest: claim %+v, err %v", claim, err)
	}

	for name, f := range map[string]headFields{
		"no stamps":          {root: fill(1, 32), parent: fill(0, 32)},
		"zero stamps":        {root: fill(1, 32), parent: fill(0, 32), store: fill(0, 32), parser: fill(0, 32)},
		"empty stamps":       {root: fill(1, 32), parent: fill(0, 32), store: []byte{}, parser: []byte{}},
		"empty parentDigest": {root: fill(1, 32), parent: fill(0, 32), parentDigest: []byte{}},
	} {
		if claim, err := ClaimOf(buildHead(t, f)); err != nil || claim != nil {
			t.Fatalf("%s: want no claim and no error, got %+v, %v", name, claim, err)
		}
	}

	refusals := map[string]struct {
		f    headFields
		want string
	}{
		"store only":        {headFields{root: fill(1, 32), parent: fill(0, 32), store: fill(2, 32)}, "half-stamped"},
		"parser only":       {headFields{root: fill(1, 32), parent: fill(0, 32), parser: fill(3, 32)}, "half-stamped"},
		"zero parentDigest": {func() headFields { f := stamped; f.parentDigest = fill(0, 32); return f }(), "all zero"},
		"short rootHash":    {func() headFields { f := stamped; f.root = fill(1, 31); return f }(), "31 bytes"},
		"long storeRoot":    {func() headFields { f := stamped; f.store = fill(2, 33); return f }(), "33 bytes"},
		"no rootHash":       {func() headFields { f := stamped; f.root = nil; return f }(), "no rootHash"},
		"no parentHash":     {func() headFields { f := stamped; f.parent = nil; return f }(), "no parentHash"},
	}
	for name, c := range refusals {
		claim, err := ClaimOf(buildHead(t, c.f))
		if err == nil || !strings.Contains(err.Error(), c.want) || errors.Is(err, ErrRetiredDigestV2) {
			t.Fatalf("%s: want an error containing %q, got claim %+v, err %v", name, c.want, claim, err)
		}
	}

	// retiredTreeRoot is checked first: even a head malformed otherwise is
	// refused by version.
	malformed := refusals["short rootHash"].f
	malformed.retired = fill(4, 32)
	if _, err := ClaimOf(buildHead(t, malformed)); !errors.Is(err, ErrRetiredDigestV2) {
		t.Fatalf("a malformed v2 head: want ErrRetiredDigestV2, got %v", err)
	}
}
