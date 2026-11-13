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

and requires the result to equal the fixture's response. The codec drops a
field the schema does not declare, so equality also catches a fixture key the
schema lacks. The struct is the entry's `capnp_struct`, else its `go_binding`;
a name that is not a struct of daemon.capnp fails the gate. There is no skip.

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


def check(fixtures: dict) -> tuple[int, list[str]]:
    structs = set(re.findall(r"^struct (\w+)", (SCHEMA_DIR / SCHEMA).read_text(), re.M))
    failures = []
    checked = 0
    for name, entry in fixtures.items():
        if name == "_doc":
            continue
        struct = entry.get("capnp_struct") or entry.get("go_binding")
        if struct not in structs:
            failures.append(f"{name}: {struct!r} is not a struct of {SCHEMA}")
            continue
        want = entry["response"]
        try:
            binary = convert("json:binary", struct, json.dumps(want).encode())
            got = json.loads(convert("binary:json", struct, binary))
        except ValueError as err:
            failures.append(f"{name}: {struct} does not decode its response: {err}")
            continue
        if got != want:
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
        "a binding that is no struct": ({"go_binding": "ExecutionErrorResponse",
                                         "response": {"ok": False}},
                                        "is not a struct"),
    }
    bad = 0
    for label, (entry, expect) in cases.items():
        _, failures = check({"case": entry})
        if len(failures) != 1 or expect not in failures[0]:
            print(f"self-test: {label} was not refused with {expect!r}: {failures}", file=sys.stderr)
            bad += 1
    checked, failures = check({"case": {"go_binding": "QueryResponse", "response": good}})
    if checked != 1 or failures:
        print(f"self-test: a positional NULL row was refused: {failures}", file=sys.stderr)
        bad += 1
    if bad:
        return 1
    print(f"daemon fixture codec self-test: {len(cases)} drifts refused, a good entry passes")
    return 0


def main() -> int:
    if shutil.which("capnp") is None:
        print("daemon fixture codec: capnp not on PATH (task deps:capnp)", file=sys.stderr)
        return 1
    if sys.argv[1:] == ["--self-test"]:
        return self_test()
    path = Path(sys.argv[1]) if len(sys.argv) > 1 else DEFAULT_FIXTURES
    checked, failures = check(json.loads(path.read_text()))
    if failures:
        print(f"daemon fixture codec: {len(failures)} failure(s):", file=sys.stderr)
        for failure in failures:
            print(f"  {failure}", file=sys.stderr)
        return 1
    print(f"daemon fixture codec: {checked} fixture responses round-trip through {SCHEMA}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
