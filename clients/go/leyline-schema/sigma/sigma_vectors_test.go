package sigma

import (
	"encoding/hex"
	"encoding/json"
	"os"
	"path/filepath"
	"testing"

	"github.com/zeebo/blake3"
)

// The canonical vectors; the Rust tests read the same file.
const vectorsPath = "../../../../rs/ll-core/schema-spec/sigma/v1/test-vectors/constructions.json"

type entryJSON struct {
	Addr string `json:"addr"`
	A    uint64 `json:"a"`
	B    uint64 `json:"b"`
}

type vectors struct {
	Partition []struct {
		Name         string      `json:"name"`
		Domain       string      `json:"domain"`
		CanonVersion uint32      `json:"canonVersion"`
		Scheme       string      `json:"scheme"`
		ParamsHex    string      `json:"paramsHex"`
		Entries      []entryJSON `json:"entries"`
		PreimageHex  string      `json:"preimageHex"`
		Address      string      `json:"address"`
	} `json:"partition"`
	StoreRoot []struct {
		Name   string `json:"name"`
		Scheme string `json:"scheme"`
		Leaves []struct {
			Path    string  `json:"path"`
			Mode    uint32  `json:"mode"`
			Content *string `json:"content"`
		} `json:"leaves"`
		Nodes []struct {
			Path    string `json:"path"`
			Entries int    `json:"entries"`
			Address string `json:"address"`
		} `json:"nodes"`
		Address string `json:"address"`
	} `json:"storeRoot"`
	HeadDigest []struct {
		Name         string  `json:"name"`
		Context      string  `json:"context"`
		Generation   uint64  `json:"generation"`
		RootHash     string  `json:"rootHash"`
		ParentHash   string  `json:"parentHash"`
		ParentDigest *string `json:"parentDigest"`
		StoreRoot    string  `json:"storeRoot"`
		ParserID     string  `json:"parserId"`
		PreimageHex  string  `json:"preimageHex"`
		Digest       string  `json:"digest"`
	} `json:"headDigest"`
	AstNode []struct {
		Name        string   `json:"name"`
		Context     string   `json:"context"`
		Tag         string   `json:"tag"`
		Kind        string   `json:"kind"`
		Token       string   `json:"token"`
		Children    []string `json:"children"`
		PreimageHex string   `json:"preimageHex"`
		Address     string   `json:"address"`
	} `json:"astNode"`
}

func load(t *testing.T) vectors {
	t.Helper()
	data, err := os.ReadFile(filepath.FromSlash(vectorsPath))
	if err != nil {
		t.Fatalf("read the Σ vectors: %v", err)
	}
	var v vectors
	if err := json.Unmarshal(data, &v); err != nil {
		t.Fatalf("decode the Σ vectors: %v", err)
	}
	return v
}

func hashOf(t *testing.T, s string) Hash {
	t.Helper()
	b, err := hex.DecodeString(s)
	if err != nil || len(b) != 32 {
		t.Fatalf("%q is not 32 hex bytes", s)
	}
	var h Hash
	copy(h[:], b)
	return h
}

func TestPartitionVectors(t *testing.T) {
	domains := map[string]Domain{"ByteStream": ByteStream, "ChunkSet": ChunkSet, "RowSet": RowSet}
	v := load(t)
	if len(v.Partition) == 0 {
		t.Fatal("no partition cases")
	}
	for _, c := range v.Partition {
		params, err := hex.DecodeString(c.ParamsHex)
		if err != nil {
			t.Fatal(err)
		}
		entries := make([]Entry, len(c.Entries))
		for i, e := range c.Entries {
			entries[i] = Entry{Addr: hashOf(t, e.Addr), A: e.A, B: e.B}
		}
		d, ok := domains[c.Domain]
		if !ok {
			t.Fatalf("%s: unknown domain %q", c.Name, c.Domain)
		}
		pre := PartitionPreimage(d, c.CanonVersion, c.Scheme, params, entries)
		if got := hex.EncodeToString(pre); got != c.PreimageHex {
			t.Fatalf("%s: preimage\n want %s\n  got %s", c.Name, c.PreimageHex, got)
		}
		if got := PartitionAddress(d, c.CanonVersion, c.Scheme, params, entries); got != hashOf(t, c.Address) {
			t.Fatalf("%s: address %x, want %s", c.Name, got, c.Address)
		}
	}
}

// LLO ADR-0047 M1: Go reproduces every storeRoot vector, node by node.
func TestStoreRootVectors(t *testing.T) {
	v := load(t)
	if len(v.StoreRoot) == 0 {
		t.Fatal("no storeRoot cases")
	}
	for _, c := range v.StoreRoot {
		if c.Scheme != StoreTreeScheme {
			t.Fatalf("%s: scheme %q", c.Name, c.Scheme)
		}
		leaves := make([]StoreLeaf, len(c.Leaves))
		for i, l := range c.Leaves {
			leaves[i] = StoreLeaf{Path: l.Path, Mode: l.Mode}
			if l.Content != nil {
				leaves[i].Blob = hashOf(t, *l.Content)
			}
		}
		root, nodes, err := StoreRoot(leaves)
		if err != nil {
			t.Fatalf("%s: %v", c.Name, err)
		}
		if root != hashOf(t, c.Address) {
			t.Fatalf("%s: storeRoot %x, want %s", c.Name, root, c.Address)
		}
		want := map[string]StoreNode{}
		for _, n := range c.Nodes {
			want[n.Path] = StoreNode{Path: n.Path, Address: hashOf(t, n.Address), Entries: n.Entries}
		}
		if len(nodes) != len(want) {
			t.Fatalf("%s: %d nodes, want %d", c.Name, len(nodes), len(want))
		}
		for _, n := range nodes {
			if want[n.Path] != n {
				t.Fatalf("%s: node %q = %x/%d, want %+v", c.Name, n.Path, n.Address, n.Entries, want[n.Path])
			}
		}
	}
}

// storeRoot takes no parser input, and one blob byte moves it.
func TestStoreRootIsAFunctionOfTheStoreAlone(t *testing.T) {
	leaves := []StoreLeaf{{Path: "a/b.go", Mode: ModeRegular, Blob: Hash{1}}}
	a, _, _ := StoreRoot(leaves)
	leaves[0].Blob[31] ^= 1
	b, _, _ := StoreRoot(leaves)
	if a == b {
		t.Fatal("a changed blob did not move storeRoot")
	}
	for _, bad := range [][]StoreLeaf{
		{{Path: "a//b", Mode: ModeRegular}},
		{{Path: "a", Mode: ModeRegular}, {Path: "a/b", Mode: ModeRegular}},
		{{Path: "a", Mode: ModeRegular}, {Path: "a", Mode: ModeRegular}},
		{{Path: "a", Mode: 0o644}},
	} {
		if _, _, err := StoreRoot(bad); err == nil {
			t.Fatalf("accepted %+v", bad)
		}
	}
}

// LLO ADR-0047 M1: Go reproduces head digest v3 on every vector, preimage
// first.
func TestHeadDigestV3Vectors(t *testing.T) {
	v := load(t)
	if len(v.HeadDigest) == 0 {
		t.Fatal("no headDigest cases")
	}
	for _, c := range v.HeadDigest {
		if c.Context != HeadDigestContext {
			t.Fatalf("%s: the case pins %q, this package computes %q", c.Name, c.Context, HeadDigestContext)
		}
		claim := HeadClaim{
			Generation: c.Generation,
			RootHash:   hashOf(t, c.RootHash),
			ParentHash: hashOf(t, c.ParentHash),
			StoreRoot:  hashOf(t, c.StoreRoot),
			ParserID:   hashOf(t, c.ParserID),
		}
		if c.ParentDigest != nil {
			d := hashOf(t, *c.ParentDigest)
			claim.ParentDigest = &d
		}
		if got := hex.EncodeToString(HeadPreimage(claim)); got != c.PreimageHex {
			t.Fatalf("%s: preimage\n want %s\n  got %s", c.Name, c.PreimageHex, got)
		}
		got, err := HeadDigest(claim)
		if err != nil || got != hashOf(t, c.Digest) {
			t.Fatalf("%s: digest %x (%v), want %s", c.Name, got, err, c.Digest)
		}
		// A v2 signature over the same preimage names another digest.
		if deriveKey("leyline head digest v2", HeadPreimage(claim)) == got {
			t.Fatalf("%s: v3 digest equals the v2 construction", c.Name)
		}
		unstamped := claim
		unstamped.StoreRoot = Hash{}
		if _, err := HeadDigest(unstamped); err == nil {
			t.Fatalf("%s: an unstamped storeRoot made a claim", c.Name)
		}
	}
}

// LLO ADR-0047 M2: Go reproduces every node_hash v3 vector, preimage first,
// and no plain BLAKE3 digest of the encoding (with or without merkle-ast-v2's
// in-band prefix) is the node_hash: law (c), construction separation.
func TestAstNodeV3Vectors(t *testing.T) {
	v := load(t)
	if len(v.AstNode) == 0 {
		t.Fatal("no astNode cases")
	}
	for _, c := range v.AstNode {
		if c.Context != AstNodeContext {
			t.Fatalf("%s: the case pins %q, this package computes %q", c.Name, c.Context, AstNodeContext)
		}
		var pre []byte
		var got Hash
		switch c.Tag {
		case "leaf":
			pre = AstLeafPreimage(c.Kind, c.Token)
			got = AstLeafHash(c.Kind, c.Token)
		case "internal":
			children := make([]Hash, len(c.Children))
			for i, h := range c.Children {
				children[i] = hashOf(t, h)
			}
			pre = AstInternalPreimage(c.Kind, children)
			got = AstInternalHash(c.Kind, children)
		default:
			t.Fatalf("%s: unknown tag %q", c.Name, c.Tag)
		}
		if hex.EncodeToString(pre) != c.PreimageHex {
			t.Fatalf("%s: preimage\n want %s\n  got %x", c.Name, c.PreimageHex, pre)
		}
		if got != hashOf(t, c.Address) {
			t.Fatalf("%s: node_hash %x, want %s", c.Name, got, c.Address)
		}
		legacy := append([]byte("llo/ast/v1\x00"), pre...)
		if blake3.Sum256(pre) == got || blake3.Sum256(legacy) == got {
			t.Fatalf("%s: a plain BLAKE3 digest names the node", c.Name)
		}
	}
}
