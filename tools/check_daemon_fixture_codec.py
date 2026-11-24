#!/usr/bin/env python3
"""Decode every daemon protocol fixture response through the normative schema.

Bead ley-line-open-0a18bc. The Go drift gate decodes each fixture into a
hand-written mirror, and the Rust gate checks the live handler against the
fixture; neither proves the fixture is a value the schema's own JSON codec
reads. A fixture can agree with both mirrors and still be unreadable by the
schema: `query` rows once carried a bare JSON `null` for SQL NULL, which
`List(Text)` cannot hold.

For each fixture entry in rs/ll-open/cli-lib/tests/fixtures/daemon-protocol.json
this runs the reference Cap'n Proto JSON codec (`capnp convert`, the C++
JsonCodec) against rs/ll-core/public-schema/capnp/daemon.capnp:

  response JSON --json:binary--> message --binary:json--> JSON

and requires the result to equal the fixture's response, JSON type for JSON
type (`0` is not `0.0`, `true` is not `1`). The codec drops a field the
schema does not declare, so equality also catches a fixture key the schema
lacks. A response carrying a `retired_*` key fails: a retired field is never
written (ADR-0014 section 2), though the codec still reads it.

The struct is looked up in BINDINGS by the entry's `go_binding`: the Go
mirror the Go gate decodes into, paired with the daemon.capnp struct it
mirrors. An entry whose binding is not in the table, whose `capnp_struct`
names a different struct than the table, or whose struct is not in
daemon.capnp fails. The table's bindings must also be exactly the cases of
the Go gate's decoderFor (clients/go/leyline-schema/daemon/
daemon_protocol_test.go), so a Go mirror and its capnp struct are always
declared together. There is no skip.

Usage: check_daemon_fixture_codec.py [fixture.json | --self-test]
"""

import json
import re
import shutil
import subprocess
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
SCHEMA_DIR = REPO / "rs/ll-core/public-schema/capnp"
SCHEMA = "daemon.capnp"
DEFAULT_FIXTURES = REPO / "rs/ll-open/cli-lib/tests/fixtures/daemon-protocol.json"
GO_DECODERS = REPO / "clients/go/leyline-schema/daemon/daemon_protocol_test.go"

# go_binding -> the daemon.capnp struct that Go mirror stands for. A new
# binding is a deliberate addition here, beside its case in
# clients/go/leyline-schema/daemon/daemon_protocol_test.go's decoderFor.
BINDINGS = {
    "StatusResponse": "StatusResponse",
    "SnapshotResponse": "SnapshotResponse",
    "ReadContentResponse": "ReadContentResponse",
    "ReadSourceResponse": "ReadSourceResponse",
    "ProposeResponse": "ProposeResponse",
    "ListChildrenResponse": "ListChildrenResponse",
    "GetNodeResponse": "GetNodeResponse",
    "FindCallersResponse": "FindCallersResponse",
    "FindDefsResponse": "FindDefsResponse",
    "FindCalleesResponse": "FindCalleesResponse",
    "GetDbPathResponse": "GetDbPathResponse",
    "QueryResponse": "QueryResponse",
    # wire.ExecutionErrorResponse is the Go mirror of the error envelope
    # that carries `executionError` (bead ley-line-open-e347ae).
    "ExecutionErrorResponse": "ErrorResponse",
    "SheafSetTopologyResponse": "SheafSetTopologyResponse",
    "SheafInvalidateResponse": "SheafInvalidateResponse",
    "SheafDefectResponse": "SheafDefectResponse",
    "SheafStatusResponse": "SheafStatusResponse",
    "SheafLearnedWeightsResponse": "SheafLearnedWeightsResponse",
}


def convert(direction: str, struct: str, data: bytes) -> bytes:
    proc = subprocess.run(
        ["capnp", "convert", direction, "-I", str(SCHEMA_DIR), SCHEMA, struct],
        cwd=SCHEMA_DIR,
        input=data,
        capture_output=True,
        check=False,
    )
    if proc.returncode != 0:
        lines = proc.stderr.decode(errors="replace").strip().splitlines()
        reason = next((line for line in lines if "failed:" in line), None)
        raise ValueError(reason or (lines[0] if lines else f"exit {proc.returncode}"))
    return proc.stdout


def go_binding_drift(go_source: str) -> list[str]:
    """The table's bindings must be exactly the Go gate's decoderFor cases,
    so no Go mirror decodes a fixture without a declared capnp struct and no
    table row names a mirror the Go gate lacks."""
    body = re.search(r"func decoderFor\(.*?\n}\n", go_source, re.S)
    if body is None:
        return ["the Go gate has no decoderFor function"]
    cases = set(re.findall(r'^\tcase "(\w+)":', body.group(0), re.M))
    drift = [f"go_binding {b!r} decodes in Go but is not in the binding table"
             for b in sorted(cases - BINDINGS.keys())]
    drift += [f"go_binding {b!r} is in the binding table but not a Go decoderFor case"
              for b in sorted(BINDINGS.keys() - cases)]
    return drift


def same(want, got) -> bool:
    """JSON equality that also requires the same JSON type at every node."""
    if type(want) is not type(got):
        return False
    if isinstance(want, dict):
        return want.keys() == got.keys() and all(same(want[k], got[k]) for k in want)
    if isinstance(want, list):
        return len(want) == len(got) and all(same(w, g) for w, g in zip(want, got))
    return want == got


def retired_keys(value, at="response"):
    if isinstance(value, dict):
        for key, inner in value.items():
            if key.startswith("retired_"):
                yield f"{at}.{key}"
            yield from retired_keys(inner, f"{at}.{key}")
    elif isinstance(value, list):
        for i, inner in enumerate(value):
            yield from retired_keys(inner, f"{at}[{i}]")


def check(fixtures: dict) -> tuple[int, list[str]]:
    structs = set(re.findall(r"^struct (\w+)", (SCHEMA_DIR / SCHEMA).read_text(), re.M))
    failures = []
    checked = 0
    for name, entry in fixtures.items():
        if name == "_doc":
            continue
        binding = entry.get("go_binding")
        if binding not in BINDINGS:
            failures.append(f"{name}: go_binding {binding!r} is not in the binding table")
            continue
        struct = BINDINGS[binding]
        declared = entry.get("capnp_struct")
        if declared is not None and declared != struct:
            failures.append(
                f"{name}: capnp_struct {declared!r} differs from {struct!r}, "
                f"the struct the table pairs with {binding!r}"
            )
            continue
        if struct not in structs:
            failures.append(f"{name}: {struct!r} is not a struct of {SCHEMA}")
            continue
        want = entry["response"]
        retired = list(retired_keys(want))
        if retired:
            failures.append(f"{name}: a retired field is never written: {', '.join(retired)}")
            continue
        try:
            binary = convert("json:binary", struct, json.dumps(want).encode())
            got = json.loads(convert("binary:json", struct, binary))
        except ValueError as err:
            failures.append(f"{name}: {struct} does not decode its response: {err}")
            continue
        if not same(want, got):
            failures.append(
                f"{name}: {struct} re-encodes its response differently\n"
                f"    fixture: {json.dumps(want, sort_keys=True)}\n"
                f"    codec:   {json.dumps(got, sort_keys=True)}"
            )
            continue
        checked += 1
    return checked, failures


def self_test() -> int:
    """Each drift the gate exists to catch fails it, and a good entry passes."""
    good = {"ok": True, "columns": ["a", "b"], "rows": [[{"null": None}, {"text": ""}]]}
    cases = {
        "a bare null cell": ({"go_binding": "QueryResponse",
                              "response": {"ok": True, "columns": ["a"], "rows": [[None]]}},
                             "does not decode"),
        "a key the schema lacks": ({"go_binding": "QueryResponse",
                                    "response": dict(good, extra=True)},
                                   "re-encodes"),
        "a default the fixture omits": ({"go_binding": "SheafInvalidateResponse",
                                         "response": {"invalidated": [], "count": 0,
                                                      "generation": "1"}},
                                        "re-encodes"),
        "a retired field": ({"go_binding": "QueryResponse",
                             "response": dict(good, retired_rows=[])},
                            "retired field"),
        "a float where the codec writes an integer": (
            {"go_binding": "SheafInvalidateResponse",
             "response": {"invalidated": [], "count": 0.0, "generation": "1",
                          "prior_generation": "0"}},
            "re-encodes"),
        "a binding outside the table": ({"go_binding": "NoSuchResponse",
                                         "response": {"ok": False}},
                                        "not in the binding table"),
        "a capnp_struct the table does not pair": ({"go_binding": "QueryResponse",
                                                    "capnp_struct": "ErrorResponse",
                                                    "response": {"ok": False}},
                                                   "differs from"),
    }
    bad = 0
    for label, (entry, expect) in cases.items():
        _, failures = check({"case": entry})
        if len(failures) != 1 or expect not in failures[0]:
            print(f"self-test: {label} was not refused with {expect!r}: {failures}", file=sys.stderr)
            bad += 1
    passes = {
        "a positional NULL row": good,
        # The reference codec keeps NUL, control characters, a quote, a
        # backslash and U+2028 in a text cell (bead ley-line-open-0a18bc).
        "a text cell with NUL and control characters": {
            "ok": True, "columns": ["t"],
            "rows": [[{"text": "a\x00b\x01\t\n\x1f\"\\\u2028"}]],
        },
        "a truncated reply": dict(good, truncation={"limit": 1}),
    }
    for label, response in passes.items():
        checked, failures = check({"case": {"go_binding": "QueryResponse", "response": response}})
        if checked != 1 or failures:
            print(f"self-test: {label} was refused: {failures}", file=sys.stderr)
            bad += 1
    decoder = '\tcase "QueryResponse":\n\t\treturn nil\n'
    go_cases = {
        "a Go mirror the table lacks": (
            go_source_with(decoder + '\tcase "NoSuchResponse":\n\t\treturn nil\n'),
            "not in the binding table"),
        "a table row the Go gate lacks": (go_source_with(decoder), "not a Go decoderFor case"),
    }
    for label, (source, expect) in go_cases.items():
        drift = go_binding_drift(source)
        if not any(expect in d for d in drift):
            print(f"self-test: {label} was not refused with {expect!r}: {drift}", file=sys.stderr)
            bad += 1
    full = "".join(f'\tcase "{b}":\n\t\treturn nil\n' for b in BINDINGS)
    if go_binding_drift(go_source_with(full)):
        print("self-test: a Go gate matching the table was refused", file=sys.stderr)
        bad += 1
    if bad:
        return 1
    print(f"daemon fixture codec self-test: {len(cases) + len(go_cases)} drifts refused, "
          f"{len(passes) + 1} good inputs pass")
    return 0


def go_source_with(cases: str) -> str:
    return f"func decoderFor(name string) func([]byte) error {{\n\tswitch name {{\n{cases}\t}}\n}}\n"


def main() -> int:
    if shutil.which("capnp") is None:
        print("daemon fixture codec: capnp not on PATH (task deps:capnp)", file=sys.stderr)
        return 1
    if sys.argv[1:] == ["--self-test"]:
        return self_test()
    path = Path(sys.argv[1]) if len(sys.argv) > 1 else DEFAULT_FIXTURES
    checked, failures = check(json.loads(path.read_text()))
    failures += go_binding_drift(GO_DECODERS.read_text())
    if failures:
        print(f"daemon fixture codec: {len(failures)} failure(s):", file=sys.stderr)
        for failure in failures:
            print(f"  {failure}", file=sys.stderr)
        return 1
    print(f"daemon fixture codec: {checked} fixture responses round-trip through {SCHEMA}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
