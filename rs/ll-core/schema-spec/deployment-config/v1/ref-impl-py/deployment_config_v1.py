#!/usr/bin/env python3
"""deployment-config/v1: independent canonical writer and configDigest.

Standard library only, and it imports neither `json` nor any hashing module:
the reader, the canonical writer and BLAKE3 are written here from the spec
(`../README.md`, "Canonical form") and the BLAKE3 paper, so agreeing with the
Rust implementation (`leyline_schema_spec::deployment_config`) is evidence
about the spec rather than about a shared library.

It takes a *resolved* document (every default filled, `~/` expanded):
resolution reads the host and is LLO's job, while canonicalization and the
digest are what a second runtime must reproduce. Keys may be in any order
and set lists unsorted; the writer sorts both by UTF-8 bytes.

    python3 -I deployment_config_v1.py canonical FILE      # the canonical bytes of FILE
    python3 -I deployment_config_v1.py digest FILE         # blake3-256:<hex> of them
    python3 -I deployment_config_v1.py conformance SPECDIR # check every pinned vector

`configDigest` is BLAKE3-256 over the canonical bytes. It names one resolved
configuration; it is not an authority and never resolved against a CAS.
"""

import sys

MAX_DEPTH = 16

# Keys whose list value is a set (README "Canonical form", rule 4). Every
# list under `signAllow.callers` is a set too.
SET_KEYS = frozenset((
    "devices", "groups", "resolveAllow", "runtimeFiles", "trustKeys",
    "trustedKeys", "trustedQueryHashes",
))


# --------------------------------------------------------------------------
# BLAKE3 (hash mode, 32-byte output).

_M = 0xFFFFFFFF
_IV = (0x6A09E667, 0xBB67AE85, 0x3C6EF372, 0xA54FF53A,
       0x510E527F, 0x9B05688C, 0x1F83D9AB, 0x5BE0CD19)
_PERM = (2, 6, 3, 10, 7, 0, 4, 13, 1, 11, 12, 5, 9, 14, 15, 8)
_CHUNK_START, _CHUNK_END, _PARENT, _ROOT = 1, 2, 4, 8
_CHUNK_LEN, _BLOCK_LEN = 1024, 64


def _rotr(x, n):
    return ((x >> n) | (x << (32 - n))) & _M


def _g(s, a, b, c, d, x, y):
    s[a] = (s[a] + s[b] + x) & _M
    s[d] = _rotr(s[d] ^ s[a], 16)
    s[c] = (s[c] + s[d]) & _M
    s[b] = _rotr(s[b] ^ s[c], 12)
    s[a] = (s[a] + s[b] + y) & _M
    s[d] = _rotr(s[d] ^ s[a], 8)
    s[c] = (s[c] + s[d]) & _M
    s[b] = _rotr(s[b] ^ s[c], 7)


def _compress(cv, words, counter, block_len, flags):
    s = list(cv) + list(_IV[:4]) + [counter & _M, (counter >> 32) & _M, block_len, flags]
    m = list(words)
    for r in range(7):
        _g(s, 0, 4, 8, 12, m[0], m[1])
        _g(s, 1, 5, 9, 13, m[2], m[3])
        _g(s, 2, 6, 10, 14, m[4], m[5])
        _g(s, 3, 7, 11, 15, m[6], m[7])
        _g(s, 0, 5, 10, 15, m[8], m[9])
        _g(s, 1, 6, 11, 12, m[10], m[11])
        _g(s, 2, 7, 8, 13, m[12], m[13])
        _g(s, 3, 4, 9, 14, m[14], m[15])
        if r < 6:
            m = [m[i] for i in _PERM]
    for i in range(8):
        s[i] ^= s[i + 8]
    return s[:8]


def _words(block):
    block = block + bytes(_BLOCK_LEN - len(block))
    return [int.from_bytes(block[i:i + 4], "little") for i in range(0, _BLOCK_LEN, 4)]


def _chunk_cv(chunk, index, root):
    cv = list(_IV)
    blocks = [chunk[i:i + _BLOCK_LEN] for i in range(0, len(chunk), _BLOCK_LEN)] or [b""]
    for i, block in enumerate(blocks):
        flags = (_CHUNK_START if i == 0 else 0) | (_CHUNK_END if i == len(blocks) - 1 else 0)
        if root and i == len(blocks) - 1:
            flags |= _ROOT
        cv = _compress(cv, _words(block), index, len(block), flags)
    return cv


def _parent(left, right, root):
    return _compress(_IV, list(left) + list(right), 0, _BLOCK_LEN, _PARENT | (_ROOT if root else 0))


def blake3_hex(data):
    chunks = [data[i:i + _CHUNK_LEN] for i in range(0, len(data), _CHUNK_LEN)] or [b""]
    if len(chunks) == 1:
        cv = _chunk_cv(chunks[0], 0, True)
    else:
        stack = []
        for index, chunk in enumerate(chunks[:-1]):
            cv = _chunk_cv(chunk, index, False)
            total = index + 1
            while total & 1 == 0:
                cv = _parent(stack.pop(), cv, False)
                total >>= 1
            stack.append(cv)
        cv = _chunk_cv(chunks[-1], len(chunks) - 1, False)
        while stack:
            left = stack.pop()
            cv = _parent(left, cv, not stack)
    return b"".join(w.to_bytes(4, "little") for w in cv).hex()


# --------------------------------------------------------------------------
# The reader: JSON (RFC 8259). A string that would need an escape is refused,
# so strings are kept as their raw UTF-8 bytes.

class Refused(Exception):
    pass


def _allowed(cp):
    return not (cp <= 0x1F or 0x7F <= cp <= 0x9F or cp in (0x2028, 0x2029, 0x22, 0x5C))


class Str:
    def __init__(self, raw):
        try:
            text = raw.decode("utf-8")
        except UnicodeDecodeError:
            raise Refused("bad-string: not UTF-8")
        if not all(_allowed(ord(c)) for c in text):
            raise Refused("bad-string: %r" % text)
        self.raw = raw


class Int:
    def __init__(self, text):
        if not text.isdigit() or (len(text) > 1 and text[0] == "0"):
            raise Refused("not a non-negative integer: %s" % text)
        if int(text) > 9007199254740991:
            raise Refused("out-of-range: %s" % text)
        self.value = int(text)


class Lit:
    def __init__(self, word):
        self.word = word


class Reader:
    def __init__(self, text):
        self.t = text
        self.p = 0

    def peek(self):
        return self.t[self.p] if self.p < len(self.t) else None

    def ws(self):
        while self.peek() in (0x20, 0x09, 0x0A, 0x0D):
            self.p += 1

    def expect(self, b):
        if self.peek() != b:
            raise Refused("malformed at byte %d" % self.p)
        self.p += 1

    def document(self):
        self.ws()
        v = self.value(0)
        self.ws()
        if self.p != len(self.t):
            raise Refused("malformed: trailing bytes")
        return v

    def value(self, depth):
        c = self.peek()
        if c in (ord("{"), ord("[")):
            if depth >= MAX_DEPTH:
                raise Refused("malformed: nesting")
            return self.obj(depth + 1) if c == ord("{") else self.arr(depth + 1)
        if c == ord('"'):
            return Str(self.string())
        if c is not None and (c == ord("-") or ord("0") <= c <= ord("9")):
            start = self.p
            while self.peek() is not None and chr(self.peek()) in "-+.eE0123456789":
                self.p += 1
            return Int(self.t[start:self.p].decode("ascii"))
        for word in (b"true", b"false", b"null"):
            if self.t.startswith(word, self.p):
                self.p += len(word)
                return Lit(word)
        raise Refused("malformed at byte %d" % self.p)

    def obj(self, depth):
        self.expect(ord("{"))
        members = {}
        self.ws()
        if self.peek() == ord("}"):
            self.p += 1
            return members
        while True:
            self.ws()
            key = Str(self.string()).raw
            if key in members:
                raise Refused("duplicate-key: %r" % key)
            self.ws()
            self.expect(ord(":"))
            self.ws()
            members[key] = self.value(depth)
            self.ws()
            c = self.peek()
            self.p += 1
            if c == ord("}"):
                return members
            if c != ord(","):
                raise Refused("malformed at byte %d" % (self.p - 1))

    def arr(self, depth):
        self.expect(ord("["))
        items = []
        self.ws()
        if self.peek() == ord("]"):
            self.p += 1
            return items
        while True:
            self.ws()
            items.append(self.value(depth))
            self.ws()
            c = self.peek()
            self.p += 1
            if c == ord("]"):
                return items
            if c != ord(","):
                raise Refused("malformed at byte %d" % (self.p - 1))

    def string(self):
        self.expect(ord('"'))
        start = self.p
        while True:
            c = self.peek()
            if c is None:
                raise Refused("malformed: unterminated string")
            if c == ord("\\"):
                raise Refused("bad-string: an escape")
            if c == ord('"'):
                raw = self.t[start:self.p]
                self.p += 1
                return raw
            self.p += 1


# --------------------------------------------------------------------------
# The canonical writer (README "Canonical form").

def _write(v, indent, out, key=None, parent=None):
    if isinstance(v, Lit):
        out.append(v.word)
    elif isinstance(v, Int):
        out.append(str(v.value).encode("ascii"))
    elif isinstance(v, Str):
        out.append(b'"' + v.raw + b'"')
    elif isinstance(v, list):
        items = v
        if key in SET_KEYS or parent == b"callers":
            if not all(isinstance(i, Str) for i in items):
                raise Refused("a set holds a non-string")
            items = sorted(items, key=lambda s: s.raw)
            if any(items[i].raw == items[i + 1].raw for i in range(len(items) - 1)):
                raise Refused("duplicate-entry in %s" % key)
        if not items:
            out.append(b"[]")
            return
        out.append(b"[")
        for i, item in enumerate(items):
            out.append(b"," if i else b"")
            out.append(b"\n" + b"  " * (indent + 1))
            _write(item, indent + 1, out)
        out.append(b"\n" + b"  " * indent + b"]")
    else:
        if not v:
            out.append(b"{}")
            return
        out.append(b"{")
        for i, k in enumerate(sorted(v)):
            out.append(b"," if i else b"")
            out.append(b"\n" + b"  " * (indent + 1) + b'"' + k + b'": ')
            _write(v[k], indent + 1, out, k.decode("utf-8"), key.encode("utf-8") if key else None)
        out.append(b"\n" + b"  " * indent + b"}")


def canonical(text):
    tree = Reader(text).document()
    if not isinstance(tree, dict):
        raise Refused("a document is an object")
    out = []
    _write(tree, 0, out)
    return b"".join(out)


def digest(text):
    return "blake3-256:" + blake3_hex(canonical(text))


# --------------------------------------------------------------------------
# Conformance: the vector files are JSON; they are read with this reader too,
# so the vector strings' JSON escapes are decoded here.

_ESC = {"n": "\n", "t": "\t", "r": "\r", "b": "\b", "f": "\f", "/": "/", '"': '"', "\\": "\\"}


def _decode(raw):
    s = raw.decode("utf-8")
    out = []
    i = 0
    while i < len(s):
        c = s[i]
        if c != "\\":
            out.append(c)
            i += 1
            continue
        e = s[i + 1]
        if e == "u":
            cp = int(s[i + 2:i + 6], 16)
            i += 6
            if 0xD800 <= cp < 0xDC00 and s[i:i + 2] == "\\u":
                low = int(s[i + 2:i + 6], 16)
                cp = 0x10000 + ((cp - 0xD800) << 10) + (low - 0xDC00)
                i += 6
            out.append(chr(cp))
        else:
            out.append(_ESC[e])
            i += 2
    return "".join(out)


class _VectorReader(Reader):
    """Reads a vector file: strings may hold escapes, decoded to text."""

    def string(self):
        self.expect(ord('"'))
        start = self.p
        while True:
            c = self.peek()
            if c is None:
                raise Refused("malformed vector file")
            if c == ord("\\"):
                self.p += 2
                continue
            if c == ord('"'):
                raw = self.t[start:self.p]
                self.p += 1
                return raw
            self.p += 1

    def value(self, depth):
        if self.peek() == ord('"'):
            return _decode(self.string())
        return Reader.value(self, depth)

    def obj(self, depth):
        self.expect(ord("{"))
        members = {}
        self.ws()
        if self.peek() == ord("}"):
            self.p += 1
            return members
        while True:
            self.ws()
            key = _decode(self.string())
            self.ws()
            self.expect(ord(":"))
            self.ws()
            members[key] = self.value(depth)
            self.ws()
            c = self.peek()
            self.p += 1
            if c == ord("}"):
                return members
            if c != ord(","):
                raise Refused("malformed vector file")


def _vectors(path):
    with open(path, "rb") as f:
        return _VectorReader(f.read()).document()


def conformance(spec_dir):
    """Every accept case's `canonical` is a fixpoint reaching its pinned
    configDigest, and every `canonical-strings.json` case's `resolved`
    document canonicalizes to its pinned bytes."""
    failures = 0
    checked = 0
    for name in ("daemon-config-cases.json", "execution-daemon-config-cases.json",
                 "sign-helper-config-cases.json", "toml-equivalence.json",
                 "canonical-strings.json"):
        doc = _vectors(spec_dir + "/test-vectors/" + name)
        for case in doc.get("cases", []):
            want = case["canonical"].encode("utf-8")
            sources = [want]
            if "resolved" in case:
                sources.append(case["resolved"].encode("utf-8"))
            for source in sources:
                checked += 1
                try:
                    got = canonical(source)
                except Refused as e:
                    print("FAIL %s/%s: refused: %s" % (name, case["name"], e))
                    failures += 1
                    continue
                if got != want:
                    print("FAIL %s/%s: canonical bytes differ" % (name, case["name"]))
                    failures += 1
                elif "blake3-256:" + blake3_hex(got) != case["configDigest"]:
                    print("FAIL %s/%s: configDigest differs" % (name, case["name"]))
                    failures += 1
        for case in doc.get("resolvedRejects", []):
            checked += 1
            try:
                canonical(case["resolved"].encode("utf-8"))
            except Refused:
                continue
            print("FAIL %s/%s: accepted a refused resolved document" % (name, case["name"]))
            failures += 1
    print("%d checked, %d failed" % (checked, failures))
    return 1 if failures or not checked else 0


def main(argv):
    if len(argv) == 3 and argv[1] in ("digest", "canonical"):
        with open(argv[2], "rb") as f:
            data = f.read()
        try:
            out = canonical(data)
        except Refused as e:
            print("refused: %s" % e, file=sys.stderr)
            return 2
        if argv[1] == "digest":
            print("blake3-256:" + blake3_hex(out))
        else:
            sys.stdout.buffer.write(out)
        return 0
    if len(argv) == 3 and argv[1] == "conformance":
        return conformance(argv[2])
    print(__doc__, file=sys.stderr)
    return 64


if __name__ == "__main__":
    sys.exit(main(sys.argv))
