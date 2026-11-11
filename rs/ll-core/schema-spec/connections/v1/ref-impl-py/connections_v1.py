#!/usr/bin/env python3
"""cloister/connections/v1: independent reference parser, canonicalizer and digest.

Standard library only, and it imports neither `json` nor any hashing module:
the reader, the canonical writer and BLAKE3 are written here from the spec
(`../README.md`) and the BLAKE3 paper, so agreeing with the Rust
implementation (`leyline_schema_spec::connections`) is evidence about the spec
rather than about a shared library.

    python3 -I connections_v1.py digest FILE         # print blake3-256:<hex> of FILE's canonical form
    python3 -I connections_v1.py canonical FILE      # print the canonical bytes
    python3 -I connections_v1.py conformance SPECDIR # check every pin, case and refusal

`connectionsDigest` is BLAKE3-256 over the canonical bytes. It names those
bytes only; it is not confinementDigest, a blob hash or any arena or store root.
"""

import sys

VERSION = "cloister/connections/v1"
APP_PROTOCOL_TCP = "art.tcp"
APP_PROTOCOLS_NOT_DELIVERED = ("art.grpc", "art.http", "art.mysql", "art.postgres", "art.redis")
MAX_ENDPOINT_PATH_BYTES = 103
MAX_DEPTH = 16


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
# The reader: JSON with strings kept raw (escapes scanned, never decoded).

class Refused(Exception):
    def __init__(self, reason, connection=None, field=""):
        super().__init__(reason)
        self.reason = reason
        self.connection = connection
        self.field = field

    def triple(self):
        return "%s|%s|%s" % (self.connection or "-", self.field or "-", self.reason)


class Raw:
    """A string token: the bytes between the quotes, undecoded."""

    def __init__(self, raw):
        self.raw = raw

    def clean(self):
        for b in self.raw:
            if b < 0x20 or b > 0x7E or b in (0x22, 0x5C):
                return None
        return self.raw.decode("ascii")

    def shown(self):
        return "".join(chr(b) if 0x20 <= b <= 0x7E else "\\x%02x" % b for b in self.raw)


class Num:
    def __init__(self, text):
        self.text = text


class Literal:
    def __init__(self, word):
        self.word = word


def _syntax():
    return Refused("malformed-json")


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
            raise _syntax()
        self.p += 1

    def document(self):
        self.ws()
        v = self.value(0)
        self.ws()
        if self.p != len(self.t):
            raise _syntax()
        return v

    def enter(self, depth):
        if depth >= MAX_DEPTH:
            raise _syntax()
        return depth + 1

    def value(self, depth):
        c = self.peek()
        if c == ord("{"):
            return self.obj(depth)
        if c == ord("["):
            return self.arr(depth)
        if c == ord('"'):
            return self.string()
        if c is not None and (c == ord("-") or ord("0") <= c <= ord("9")):
            return self.number()
        for word in (b"true", b"false", b"null"):
            if self.t.startswith(word, self.p):
                self.p += len(word)
                return Literal(word)
        raise _syntax()

    def obj(self, depth):
        depth = self.enter(depth)
        self.expect(ord("{"))
        members = []
        self.ws()
        if self.peek() == ord("}"):
            self.p += 1
            return members_dict(members)
        while True:
            self.ws()
            if self.peek() != ord('"'):
                raise _syntax()
            k = self.string()
            self.ws()
            self.expect(ord(":"))
            self.ws()
            members.append((k, self.value(depth)))
            self.ws()
            c = self.peek()
            if c == ord(","):
                self.p += 1
            elif c == ord("}"):
                self.p += 1
                return members_dict(members)
            else:
                raise _syntax()

    def arr(self, depth):
        depth = self.enter(depth)
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
            if c == ord(","):
                self.p += 1
            elif c == ord("]"):
                self.p += 1
                return items
            else:
                raise _syntax()

    def string(self):
        self.expect(ord('"'))
        start = self.p
        while True:
            c = self.peek()
            if c is None:
                raise _syntax()
            if c == ord('"'):
                raw = self.t[start:self.p]
                self.p += 1
                return Raw(raw)
            if c == ord("\\"):
                self.p += 1
                e = self.peek()
                if e is not None and chr(e) in '"\\/bfnrt':
                    self.p += 1
                elif e == ord("u"):
                    self.p += 1
                    for _ in range(4):
                        h = self.peek()
                        if h is None or chr(h) not in "0123456789abcdefABCDEF":
                            raise _syntax()
                        self.p += 1
                else:
                    raise _syntax()
            else:
                self.p += 1

    def number(self):
        start = self.p
        if self.peek() == ord("-"):
            self.p += 1
        c = self.peek()
        if c == ord("0"):
            self.p += 1
        elif c is not None and ord("1") <= c <= ord("9"):
            while self.peek() is not None and ord("0") <= self.peek() <= ord("9"):
                self.p += 1
        else:
            raise _syntax()
        if self.peek() == ord("."):
            self.p += 1
            self.digits()
        if self.peek() in (ord("e"), ord("E")):
            self.p += 1
            if self.peek() in (ord("+"), ord("-")):
                self.p += 1
            self.digits()
        return Num(self.t[start:self.p].decode("ascii"))

    def digits(self):
        c = self.peek()
        if c is None or not ord("0") <= c <= ord("9"):
            raise _syntax()
        while self.peek() is not None and ord("0") <= self.peek() <= ord("9"):
            self.p += 1


class Obj(list):
    """An object: its (Raw key, value) members in input order."""


def members_dict(members):
    return Obj(members)


# --------------------------------------------------------------------------
# Validation, in the order the spec fixes.

def _field(prefix, key):
    if not prefix:
        return key
    if not key:
        return prefix
    return prefix + "." + key


def _members(conn, prefix, obj, allowed, refused, required):
    seen = []
    out = {}
    for key, value in obj:
        shown = key.shown()
        if key.raw in seen:
            raise Refused("duplicate-key", conn, _field(prefix, shown))
        seen.append(key.raw)
        name = key.clean()
        if name is not None and name in refused:
            raise Refused(refused[name], conn, _field(prefix, name))
        if name is not None and name in allowed:
            out[name] = value
        else:
            raise Refused("unknown-key", conn, _field(prefix, shown))
    for key in required:
        if key not in out:
            raise Refused("missing-key", conn, _field(prefix, key))
    return out


def _obj(conn, field, v):
    if not isinstance(v, Obj):
        raise Refused("wrong-type", conn, field)
    return v


def _str(conn, field, v):
    if not isinstance(v, Raw):
        raise Refused("wrong-type", conn, field)
    s = v.clean()
    if s is None:
        raise Refused("string-not-printable-ascii", conn, field)
    return s


def is_dns_label(s):
    inner = "abcdefghijklmnopqrstuvwxyz0123456789"
    return (0 < len(s) <= 63 and s[0] in inner and s[-1] in inner
            and all(c in inner or c == "-" for c in s))


def endpoint_path_refusal(path):
    if not path.startswith("/"):
        return "endpoint-not-absolute"
    if len(path.encode("ascii")) > MAX_ENDPOINT_PATH_BYTES:
        return "endpoint-too-long"
    if "//" in path:
        return "endpoint-empty-component"
    if path.endswith("/"):
        return "endpoint-trailing-slash"
    if any(c in (".", "..") for c in path[1:].split("/")):
        return "endpoint-dot-component"
    return None


def _quad(text, lenient):
    parts = text.split(".")
    if len(parts) != 4:
        return None
    out = []
    for p in parts:
        if not p or len(p) > 3 or not all(c in "0123456789" for c in p):
            return None
        if not lenient and len(p) > 1 and p[0] == "0":
            return None
        n = int(p)
        if n > 255:
            return None
        out.append(n)
    return out


def _ipv6_words(text):
    """Parse IPv6 text (RFC 4291 2.2, with an optional trailing dotted quad)
    into eight 16-bit words, or None. Written here so the classification does
    not lean on a library's notion of what it accepts."""
    if text.count("::") > 1:
        return None
    tail = []
    head_text, sep, tail_text = text.partition("::")

    def groups(part, allow_quad):
        if part == "":
            return []
        out = []
        pieces = part.split(":")
        for i, piece in enumerate(pieces):
            if allow_quad and i == len(pieces) - 1 and "." in piece:
                q = _quad(piece, False)
                if q is None:
                    return None
                out += [(q[0] << 8) | q[1], (q[2] << 8) | q[3]]
                continue
            if not 1 <= len(piece) <= 4 or not all(c in "0123456789abcdefABCDEF" for c in piece):
                return None
            out.append(int(piece, 16))
        return out

    if sep:
        head = groups(head_text, False)
        tail = groups(tail_text, True)
        if head is None or tail is None or len(head) + len(tail) > 7:
            return None
        return head + [0] * (8 - len(head) - len(tail)) + tail
    words = groups(text, True)
    if words is None or len(words) != 8:
        return None
    return words


def loopback_host(text):
    """Return the canonical host, or raise the refusal reason (a string)."""
    if text == "::1":
        return text
    q = _quad(text, False)
    if q is not None:
        if q[0] == 127:
            return text
        raise ValueError("non-loopback-target")
    q = _quad(text, True)
    if q is not None:
        raise ValueError("non-canonical-loopback" if q[0] == 127 else "non-loopback-target")
    if text and all(c in "0123456789abcdefABCDEF:." for c in text):
        w = _ipv6_words(text)
        if w is not None:
            loopback = w == [0] * 7 + [1]
            mapped = w[:5] == [0] * 5 and w[5] == 0xFFFF and (w[6] >> 8) == 127
            raise ValueError("non-canonical-loopback" if loopback or mapped else "non-loopback-target")
    raise ValueError("host-is-name")


def port_value(v):
    if not isinstance(v, Num):
        raise ValueError("port-not-integer")
    t = v.text
    if any(c in t for c in ".eE"):
        raise ValueError("port-not-integer")
    if t.startswith("-") or len(t) > 5:
        raise ValueError("port-out-of-range")
    n = int(t)
    if not 1 <= n <= 65535:
        raise ValueError("port-out-of-range")
    return n


def _label(index, item):
    if isinstance(item, Obj):
        for k, v in item:
            if k.raw == b"name":
                if isinstance(v, Raw):
                    s = v.clean()
                    if s is not None and is_dns_label(s):
                        return s
                break
    return "#%d" % index


def _connection(index, item):
    conn = _label(index, item)
    raw = _obj(conn, "", item)
    m = _members(conn, "", raw, ("appProtocol", "endpoint", "name", "target"),
                 {"credentialRef": "credential-ref-present"},
                 ("appProtocol", "endpoint", "name", "target"))

    proto = _str(conn, "appProtocol", m["appProtocol"])
    if proto != APP_PROTOCOL_TCP:
        reason = "app-protocol-not-delivered" if proto in APP_PROTOCOLS_NOT_DELIVERED else "unknown-app-protocol"
        raise Refused(reason, conn, "appProtocol")

    em = _members(conn, "endpoint", _obj(conn, "endpoint", m["endpoint"]), ("unix",),
                  {"tcp": "endpoint-tcp-not-delivered"}, ("unix",))
    path = _str(conn, "endpoint.unix", em["unix"])
    reason = endpoint_path_refusal(path)
    if reason:
        raise Refused(reason, conn, "endpoint.unix")

    name = _str(conn, "name", m["name"])
    if not is_dns_label(name):
        raise Refused("bad-name", conn, "name")

    tm = _members(conn, "target", _obj(conn, "target", m["target"]), ("tcp",),
                  {"cloudSql": "target-not-delivered", "https": "target-not-delivered",
                   "unix": "target-not-delivered"}, ("tcp",))
    pm = _members(conn, "target.tcp", _obj(conn, "target.tcp", tm["tcp"]), ("host", "port"),
                  {"tls": "tls-not-delivered"}, ("host", "port"))
    host_text = _str(conn, "target.tcp.host", pm["host"])
    try:
        host = loopback_host(host_text)
    except ValueError as e:
        raise Refused(str(e), conn, "target.tcp.host")
    try:
        port = port_value(pm["port"])
    except ValueError as e:
        raise Refused(str(e), conn, "target.tcp.port")
    return {"name": name, "unix": path, "host": host, "port": port}


def parse(text):
    """Parse and validate; return the connections sorted by name."""
    doc = Reader(text).document()
    if not isinstance(doc, Obj):
        raise Refused("wrong-type")
    version = None
    for k, v in doc:
        if k.raw == b"version":
            version = v
            break
    if version is None:
        raise Refused("missing-key", None, "version")
    if _str(None, "version", version) != VERSION:
        raise Refused("bad-version", None, "version")
    m = _members(None, "", doc, ("connections", "version"), {}, ("connections", "version"))
    conns = m["connections"]
    if not isinstance(conns, list) or isinstance(conns, Obj):
        raise Refused("wrong-type", None, "connections")
    if not conns:
        raise Refused("empty-connections", None, "connections")
    out = []
    for index, item in enumerate(conns):
        c = _connection(index, item)
        if any(o["name"] == c["name"] for o in out):
            raise Refused("duplicate-name", c["name"], "name")
        if any(o["unix"] == c["unix"] for o in out):
            raise Refused("duplicate-endpoint", c["name"], "endpoint.unix")
        out.append(c)
    out.sort(key=lambda c: c["name"].encode("ascii"))
    return out


def canonical(conns):
    """The canonical bytes, written by hand from README "Canonical form"."""
    lines = ["{", '  "connections": [']
    for i, c in enumerate(conns):
        lines += [
            "    {",
            '      "appProtocol": "%s",' % APP_PROTOCOL_TCP,
            '      "endpoint": {',
            '        "unix": "%s"' % c["unix"],
            "      },",
            '      "name": "%s",' % c["name"],
            '      "target": {',
            '        "tcp": {',
            '          "host": "%s",' % c["host"],
            '          "port": %d' % c["port"],
            "        }",
            "      }",
            "    }" + ("," if i + 1 < len(conns) else ""),
        ]
    lines += ["  ],", '  "version": "%s"' % VERSION, "}"]
    return "\n".join(lines).encode("ascii")


# --------------------------------------------------------------------------
# Conformance against the shipped vectors. The vector file is itself JSON
# with escapes, so it is read with this module's reader plus a small decoder
# for the vector file's own strings; the documents under test are the
# decoded `input` strings, encoded back to UTF-8 bytes.

def _decode(raw):
    out = []
    i = 0
    b = raw.raw
    while i < len(b):
        c = b[i]
        if c != 0x5C:
            out.append(c)
            i += 1
            continue
        e = chr(b[i + 1])
        if e == "u":
            cp = int(b[i + 2:i + 6].decode("ascii"), 16)
            i += 6
            if 0xD800 <= cp < 0xDC00 and b[i:i + 2] == b"\\u":
                lo = int(b[i + 2:i + 6].decode("ascii"), 16)
                cp = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00)
                i += 6
            out += list(chr(cp).encode("utf-8"))
        else:
            out.append(ord({"b": "\b", "f": "\f", "n": "\n", "r": "\r", "t": "\t"}.get(e, e)))
            i += 2
    return bytes(out)


def _vector_value(v):
    if isinstance(v, Obj):
        return {_decode(k).decode("utf-8"): _vector_value(x) for k, x in v}
    if isinstance(v, list):
        return [_vector_value(x) for x in v]
    if isinstance(v, Raw):
        return _decode(v)
    if isinstance(v, Literal):
        return {b"true": True, b"false": False, b"null": None}[v.word]
    return v.text


def _read_vectors(path):
    with open(path, "rb") as f:
        return _vector_value(Reader(f.read()).document())


def conformance(spec_dir):
    import os
    failures = 0
    if blake3_hex(b"") != "af1349b9f5f9a1a6a0404dea36dcc9499bcb25c9adc112b7cc9a93cae41f3262":
        print("FAIL blake3 self-test")
        return 1
    with open(os.path.join(spec_dir, "CONNECTIONS_DIGESTS.blake3"), "rb") as f:
        pin_lines = f.read().decode("ascii").splitlines()
    for line in pin_lines:
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        hexd, rel = line.split(None, 1)
        with open(os.path.join(spec_dir, rel), "rb") as f:
            data = f.read()
        got = blake3_hex(canonical(parse(data)))
        if got != hexd.lower() or canonical(parse(data)) != data:
            print("FAIL pin %s" % rel)
            failures += 1
        else:
            print("ok pin %s blake3-256:%s" % (rel, got))
    vectors = _read_vectors(os.path.join(spec_dir, "test-vectors", "connections-cases.json"))
    for case in vectors["cases"]:
        name = case["name"].decode("ascii")
        try:
            data = canonical(parse(case["input"]))
        except Refused as e:
            print("FAIL case %s refused %s" % (name, e.triple()))
            failures += 1
            continue
        digest = "blake3-256:" + blake3_hex(data)
        if data != case["canonical"] or digest.encode("ascii") != case["connectionsDigest"]:
            print("FAIL case %s %s" % (name, digest))
            failures += 1
        else:
            print("ok case %s %s" % (name, digest))
    for case in vectors["rejects"]:
        name = case["name"].decode("ascii")
        want = case["refusal"]
        want_triple = "%s|%s|%s" % (
            (want["connection"] or b"-").decode("ascii"),
            (want["field"] or b"-").decode("ascii"),
            want["reason"].decode("ascii"),
        )
        try:
            parse(case["input"])
        except Refused as e:
            if e.triple() == want_triple:
                print("ok reject %s %s" % (name, e.triple()))
            else:
                print("FAIL reject %s got %s want %s" % (name, e.triple(), want_triple))
                failures += 1
            continue
        print("FAIL reject %s accepted" % name)
        failures += 1
    return 1 if failures else 0


def main(argv):
    if len(argv) == 3 and argv[1] in ("digest", "canonical"):
        with open(argv[2], "rb") as f:
            data = f.read()
        try:
            out = canonical(parse(data))
        except Refused as e:
            print("refused " + e.triple(), file=sys.stderr)
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
