#!/usr/bin/env python3
"""Summarize run.sh output into the markdown tables of the V-F3 report.

usage: python3 -I summarize.py <results dir>   (prints markdown to stdout)

Percentiles are nearest-rank. Over N=5 runs p99 is the slowest run.
Per-operation percentiles (edit, create) pool every operation of every run.
"""
import collections
import math
import pathlib
import sys

ENVS = [
    ("apfs", "APFS (floor)"),
    ("vf-t0", "virtio-fs ttl=0"),
    ("vf-t3600", "virtio-fs ttl=3600"),
    ("vf-plus", "ttl=3600 + READDIRPLUS"),
    ("vf-cache", "ttl=3600 + READDIRPLUS + cached reads"),
    ("vf-dax", "ttl=3600 + READDIRPLUS + cached reads + DAX"),
]
WORKLOADS = [
    ("status", "A `git status`"),
    ("rg", "A `rg` whole tree"),
    ("find", "A `find . -type f | wc -l`"),
    ("edit", "B read/edit loop (1,000 ops)"),
    ("create", "C create 1,000 files"),
    ("untar", "C untar 1,000 files"),
]


def pct(values, p):
    s = sorted(values)
    if not s:
        return None
    rank = max(1, math.ceil(p / 100 * len(s)))
    return s[min(rank, len(s)) - 1]


def load(root):
    walls = collections.defaultdict(list)  # (env, temp, workload) -> [us]
    ops = collections.defaultdict(list)  # (env, temp, workload) -> [us]
    reqs = collections.defaultdict(list)  # (env, temp, workload) -> [dict]
    dlines = []
    for path in sorted(root.glob("*.out")):
        for line in path.read_text().splitlines():
            parts = line.split(" ")
            if parts[0] in ("RESULT", "OPS") and len(parts) >= 5:
                label, workload = parts[1], parts[2]
                env, temp = label.rsplit("-", 1)
                if parts[0] == "RESULT":
                    walls[(env, temp, workload)].append(int(parts[4].split("=")[1]))
                else:
                    ops[(env, temp, workload)].extend(int(v) for v in parts[4].split(","))
            elif parts[0] == "D":
                dlines.append(line)
    for path in sorted(root.glob("*.err")):
        for line in path.read_text().splitlines():
            if line.startswith("VF3OPS "):
                parts = line.split(" ")
                env, temp = parts[1].rsplit("-", 1)
                counts = dict(kv.split("=") for kv in parts[4:] if "=" in kv)
                reqs[(env, temp, parts[2])].append({k: int(v) for k, v in counts.items()})
    return walls, ops, reqs, dlines


def ms(us):
    return "—" if us is None else (f"{us / 1000:.1f}" if us >= 10000 else f"{us / 1000:.2f}")


def main():
    root = pathlib.Path(sys.argv[1])
    walls, ops, reqs, dlines = load(root)
    out = []
    for workload, title in WORKLOADS:
        floor = pct(walls.get(("apfs", "warm", workload), []), 50)
        floor_cold = pct(walls.get(("apfs", "cold", workload), []), 50)
        out.append(f"#### {title}\n")
        header = "| path | cold p50 ms | cold p99 ms | warm p50 ms | warm p99 ms | warm p50 / APFS | cold p50 / APFS |"
        has_ops = any(ops.get((e, t, workload)) for e, _ in ENVS for t in ("cold", "warm"))
        if has_ops:
            header += " op p50 µs (cold / warm) | op p99 µs (cold / warm) |"
        out.append(header)
        out.append("|" + "---|" * (header.count("|") - 1))
        for env, name in ENVS:
            cold = walls.get((env, "cold", workload), [])
            warm = walls.get((env, "warm", workload), [])
            if not cold and not warm:
                continue
            wp50, cp50 = pct(warm, 50), pct(cold, 50)
            ratio = f"{wp50 / floor:.1f}×" if wp50 and floor else "—"
            cratio = f"{cp50 / floor_cold:.1f}×" if cp50 and floor_cold else "—"
            row = f"| {name} | {ms(cp50)} | {ms(pct(cold, 99))} | {ms(wp50)} | {ms(pct(warm, 99))} | {ratio} | {cratio} |"
            if has_ops:
                oc, ow = ops.get((env, "cold", workload), []), ops.get((env, "warm", workload), [])
                row += f" {pct(oc, 50)} / {pct(ow, 50)} | {pct(oc, 99)} / {pct(ow, 99)} |"
            out.append(row)
        out.append("")

    out.append("#### FUSE requests the backend answered (one recorded run, median over runs)\n")
    kinds = ["lookup", "getattr", "readdir", "readdirplus", "opendir", "open", "read", "write",
             "flush", "release", "create", "setattr", "rename", "unlink", "mkdir", "dax_map"]
    out.append("| path | workload | temp | " + " | ".join(kinds) + " |")
    out.append("|" + "---|" * (len(kinds) + 3))
    for workload, _ in WORKLOADS:
        for env, name in ENVS[1:]:
            for temp in ("cold", "warm"):
                runs = reqs.get((env, temp, workload))
                if not runs:
                    continue
                cells = []
                for k in kinds:
                    vals = sorted(r.get(k, 0) for r in runs)
                    cells.append(str(vals[len(vals) // 2]))
                out.append(f"| {name} | {workload} | {temp} | " + " | ".join(cells) + " |")
    out.append("")

    out.append("#### Raw wall times (ms, run order)\n")
    out.append("```text")
    for workload, _ in WORKLOADS:
        for env, _ in ENVS:
            for temp in ("cold", "warm"):
                vals = walls.get((env, temp, workload))
                if vals:
                    out.append(f"{workload:7} {env:9} {temp:5} " + " ".join(f"{v / 1000:.1f}" for v in vals))
    out.append("```\n")

    out.append("#### D (correctness) lines as the guest printed them\n")
    out.append("```text")
    out.extend(dlines)
    out.append("```")
    print("\n".join(out))


if __name__ == "__main__":
    main()
