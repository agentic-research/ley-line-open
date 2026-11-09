// Package sigma is a Go implementation of the LLO Σ constructions a
// consumer verifies: the PartitionSpec fold (ADR-0032 D2), the store
// identity storeRoot (`leyline/store-tree/v1`, ADR-0047 D-ii), the head
// digest v3 a signed Head commits to, and node_hash v3
// (`leyline/ast-node/v3`, ADR-0047 D-i).
//
// The Rust implementations are normative (`leyline-core` partition.rs and
// head_digest.rs, `leyline-cli-lib` store_root.rs and cmd_parse.rs). This
// package is checked
// against the same known-answer vectors,
// rs/ll-core/schema-spec/sigma/v1/test-vectors/constructions.json, so a Go
// consumer recomputes a storeRoot from a manifest, or a head digest from a
// Head, and compares bytes rather than trusting a stamp.
package sigma

import (
	"bytes"
	"encoding/binary"
	"fmt"
	"sort"
	"strings"

	"github.com/zeebo/blake3"
)

// Hash is a 32-byte BLAKE3 address.
type Hash [32]byte

// Domain is a PartitionSpec domain; its value is the wire tag.
type Domain uint8

const (
	ByteStream Domain = 1
	ChunkSet   Domain = 2
	RowSet     Domain = 3
)

// PartitionContext is the derive-key context of every PartitionSpec fold.
const PartitionContext = "leyline partition fold v1"

// StoreTreeScheme is the scheme of a storeRoot directory node.
const StoreTreeScheme = "leyline/store-tree/v1"

// HeadDigestContext is the derive-key context of head digest v3. A v2
// ("leyline head digest v2") signature does not verify under it.
const HeadDigestContext = "leyline head digest v3"

// Git modes a store holds.
const (
	ModeRegular    uint32 = 0o100644
	ModeExecutable uint32 = 0o100755
	ModeSymlink    uint32 = 0o120000
	ModeDirectory  uint32 = 0o040000
)

// Entry is one PartitionSpec entry.
type Entry struct {
	Addr Hash
	A, B uint64
}

func deriveKey(context string, preimage []byte) Hash {
	var out Hash
	blake3.DeriveKey(context, preimage, out[:])
	return out
}

// PartitionPreimage is the exact byte string the PartitionSpec fold hashes:
// domain tag ‖ canonVersion u32le ‖ |scheme| u64le ‖ scheme ‖ |params| u64le
// ‖ params ‖ n u64le ‖ entries (addr ‖ a u64le ‖ b u64le). A set domain
// (ChunkSet, RowSet) sorts the entries by (addr, a, b) first.
func PartitionPreimage(domain Domain, canonVersion uint32, scheme string, params []byte, entries []Entry) []byte {
	var p bytes.Buffer
	u64 := func(v uint64) {
		var b [8]byte
		binary.LittleEndian.PutUint64(b[:], v)
		p.Write(b[:])
	}
	p.WriteByte(byte(domain))
	var cv [4]byte
	binary.LittleEndian.PutUint32(cv[:], canonVersion)
	p.Write(cv[:])
	u64(uint64(len(scheme)))
	p.WriteString(scheme)
	u64(uint64(len(params)))
	p.Write(params)
	u64(uint64(len(entries)))
	ordered := append([]Entry(nil), entries...)
	if domain != ByteStream {
		sort.Slice(ordered, func(i, j int) bool {
			if c := bytes.Compare(ordered[i].Addr[:], ordered[j].Addr[:]); c != 0 {
				return c < 0
			}
			if ordered[i].A != ordered[j].A {
				return ordered[i].A < ordered[j].A
			}
			return ordered[i].B < ordered[j].B
		})
	}
	for _, e := range ordered {
		p.Write(e.Addr[:])
		u64(e.A)
		u64(e.B)
	}
	return p.Bytes()
}

// PartitionAddress is the PartitionSpec fold: derive-key under
// PartitionContext over PartitionPreimage.
func PartitionAddress(domain Domain, canonVersion uint32, scheme string, params []byte, entries []Entry) Hash {
	return deriveKey(PartitionContext, PartitionPreimage(domain, canonVersion, scheme, params, entries))
}

// StoreLeaf is one leaf of a store manifest: a file (Mode is a file mode,
// Blob its blob hash) or an empty directory (Mode is ModeDirectory, Blob
// ignored).
type StoreLeaf struct {
	Path string
	Mode uint32
	Blob Hash
}

// StoreNode is one directory node of a store tree: its path ("" for the
// root), its address and its entry count (two per child).
type StoreNode struct {
	Path    string
	Address Hash
	Entries int
}

type dir struct {
	children map[string]*node
}

type node struct {
	mode uint32
	blob Hash
	dir  *dir
}

// StoreRoot computes storeRoot over a leaf set and returns every directory
// node, children before parents. It fails on a path that is not a plain
// relative path (empty, or an empty, "." or ".." component), on two leaves
// at one path, and on a leaf under a file, as the Rust implementation does.
func StoreRoot(leaves []StoreLeaf) (Hash, []StoreNode, error) {
	root := &dir{children: map[string]*node{}}
	for _, l := range leaves {
		parts := strings.Split(l.Path, "/")
		for _, c := range parts {
			if c == "" || c == "." || c == ".." {
				return Hash{}, nil, fmt.Errorf("store path %q is not a plain relative path", l.Path)
			}
		}
		d := root
		for _, name := range parts[:len(parts)-1] {
			n, ok := d.children[name]
			if !ok {
				n = &node{mode: ModeDirectory, dir: &dir{children: map[string]*node{}}}
				d.children[name] = n
			}
			if n.dir == nil {
				return Hash{}, nil, fmt.Errorf("store path %q lies under a file", l.Path)
			}
			d = n.dir
		}
		last := parts[len(parts)-1]
		existing, ok := d.children[last]
		switch {
		case l.Mode == ModeDirectory:
			if ok && existing.dir == nil {
				return Hash{}, nil, fmt.Errorf("store path %q is held twice", l.Path)
			}
			if !ok {
				d.children[last] = &node{mode: ModeDirectory, dir: &dir{children: map[string]*node{}}}
			}
		case l.Mode == ModeRegular || l.Mode == ModeExecutable || l.Mode == ModeSymlink:
			if ok {
				return Hash{}, nil, fmt.Errorf("store path %q is held twice", l.Path)
			}
			d.children[last] = &node{mode: l.Mode, blob: l.Blob}
		default:
			return Hash{}, nil, fmt.Errorf("store path %q has mode %o, which git does not store", l.Path, l.Mode)
		}
	}
	var nodes []StoreNode
	var fold func(path string, d *dir) Hash
	fold = func(path string, d *dir) Hash {
		names := make([]string, 0, len(d.children))
		for name := range d.children {
			names = append(names, name)
		}
		sort.Strings(names) // Go string order is byte order.
		entries := make([]Entry, 0, 2*len(names))
		for i, name := range names {
			n := d.children[name]
			child := n.blob
			if n.dir != nil {
				sub := name
				if path != "" {
					sub = path + "/" + name
				}
				child = fold(sub, n.dir)
			}
			entries = append(entries,
				Entry{Addr: blake3.Sum256([]byte(name)), A: uint64(2 * i), B: uint64(len(name))},
				Entry{Addr: child, A: uint64(2*i + 1), B: uint64(n.mode)},
			)
		}
		addr := PartitionAddress(RowSet, 1, StoreTreeScheme, nil, entries)
		nodes = append(nodes, StoreNode{Path: path, Address: addr, Entries: len(entries)})
		return addr
	}
	r := fold("", root)
	return r, nodes, nil
}

// AstNodeContext is the derive-key context of node_hash v3, the
// merkle-ast-v3 lineage (ADR-0047 D-i, step M2). merkle-ast-v2 hashed the
// same fields as plain BLAKE3 behind an in-band "llo/ast/v1\x00" prefix, so a
// file holding those bytes had a blob hash equal to a node_hash; under the
// keyed construction no plain BLAKE3 digest reaches a node_hash.
const AstNodeContext = "leyline/ast-node/v3"

// Node tags of the node_hash encoding.
const (
	AstNodeLeaf     byte = 0x00
	AstNodeInternal byte = 0x01
)

// AstLeafPreimage is the encoding node_hash v3 keys for a leaf:
// 0x00 ‖ uvarint|kind| ‖ kind ‖ uvarint|token| ‖ token, with kind the
// canonical (κ) kind and the uvarints unsigned LEB128.
func AstLeafPreimage(kind, token string) []byte {
	p := []byte{AstNodeLeaf}
	p = binary.AppendUvarint(p, uint64(len(kind)))
	p = append(p, kind...)
	p = binary.AppendUvarint(p, uint64(len(token)))
	return append(p, token...)
}

// AstInternalPreimage is the encoding node_hash v3 keys for an internal
// node: 0x01 ‖ uvarint|kind| ‖ kind ‖ uvarint n ‖ child node_hash × n, the
// non-extra children in source order.
func AstInternalPreimage(kind string, children []Hash) []byte {
	p := []byte{AstNodeInternal}
	p = binary.AppendUvarint(p, uint64(len(kind)))
	p = append(p, kind...)
	p = binary.AppendUvarint(p, uint64(len(children)))
	for _, c := range children {
		p = append(p, c[:]...)
	}
	return p
}

// AstLeafHash is a leaf's node_hash v3.
func AstLeafHash(kind, token string) Hash {
	return deriveKey(AstNodeContext, AstLeafPreimage(kind, token))
}

// AstInternalHash is an internal node's node_hash v3.
func AstInternalHash(kind string, children []Hash) Hash {
	return deriveKey(AstNodeContext, AstInternalPreimage(kind, children))
}

// HeadClaim is every identity a signed Head attests.
type HeadClaim struct {
	Generation   uint64
	RootHash     Hash
	ParentHash   Hash
	ParentDigest *Hash
	StoreRoot    Hash
	ParserID     Hash
}

// HeadPreimage is generation u64le ‖ rootHash ‖ parentHash ‖ storeRoot ‖
// parserId ‖ P, with P = 0x00 without a parent digest and 0x01 ‖
// parentDigest with one.
func HeadPreimage(c HeadClaim) []byte {
	var p bytes.Buffer
	var g [8]byte
	binary.LittleEndian.PutUint64(g[:], c.Generation)
	p.Write(g[:])
	p.Write(c.RootHash[:])
	p.Write(c.ParentHash[:])
	p.Write(c.StoreRoot[:])
	p.Write(c.ParserID[:])
	if c.ParentDigest == nil {
		p.WriteByte(0)
	} else {
		p.WriteByte(1)
		p.Write(c.ParentDigest[:])
	}
	return p.Bytes()
}

// HeadDigest is head digest v3. It refuses a claim with an unstamped (all
// zero) storeRoot or parserId, or a present all-zero parent digest, as
// leyline_core::HeadClaim::new does.
func HeadDigest(c HeadClaim) (Hash, error) {
	var zero Hash
	if c.StoreRoot == zero || c.ParserID == zero || (c.ParentDigest != nil && *c.ParentDigest == zero) {
		return Hash{}, fmt.Errorf("head claim is unstamped: storeRoot and parserId are required, and a parent digest is absent rather than zero")
	}
	return deriveKey(HeadDigestContext, HeadPreimage(c)), nil
}
