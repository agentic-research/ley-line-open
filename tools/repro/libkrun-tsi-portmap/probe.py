#!/usr/bin/env python3
"""Host-side probes for the TSI port-map reproduction.

All probes are bounded: each waits at most `--deadline` seconds and then reports
what it observed. No probe uses a sleep as a synchronisation primitive; they
poll a socket with a timeout and stop on first success or at the deadline.

Subcommands:
  tcp HOST PORT            connect once (bounded). Exit 0 if reachable.
  udp-inject PID FIXED INJECT
                           the datagram probe. (1) Repeatedly sends a datagram
                           to the guest's fixed unmapped UDP port
                           127.0.0.1:FIXED (published only if the build creates
                           a host listener for a guest UDP bind — the TSIv4
                           behaviour). (2) Polls `lsof` for the ephemeral host
                           UDP port the proxy binds after the guest's own
                           sendto (the v1.19.x behaviour), learns that port E,
                           and injects datagrams to 127.0.0.1:E from a fresh
                           host source bound to 127.0.0.1:INJECT — a source the
                           guest never sent to. Prints machine-readable lines.
"""
import re
import socket
import subprocess
import sys
import time


def ephemeral_udp_ports(pid):
    """Numeric-port UDP sockets owned by `pid` (the proxy's bound host ports)."""
    try:
        out = subprocess.run(
            ["lsof", "-nP", "-p", str(pid), "-iUDP"],
            capture_output=True, text=True, timeout=5,
        ).stdout
    except Exception:
        return set()
    ports = set()
    for line in out.splitlines():
        m = re.search(r"UDP \*:(\d+)\b", line)
        if m:
            ports.add(int(m.group(1)))
    return ports


def tcp(host, port, deadline):
    end = time.monotonic() + deadline
    while time.monotonic() < end:
        try:
            with socket.create_connection((host, port), timeout=0.5) as s:
                s.recv(16)
            print(f"tcp_reachable host={host} port={port}")
            return 0
        except OSError:
            time.sleep(0.1)
    print(f"tcp_refused host={host} port={port}")
    return 1


def udp_inject(pid, fixed_port, inject_port, deadline):
    inject = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    inject.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    inject.bind(("127.0.0.1", inject_port))

    fixed = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)

    # Host UDP ports the proxy already held before the guest's sendto (if any),
    # so a port that appears later is the one the sendto created.
    baseline = ephemeral_udp_ports(pid)
    end = time.monotonic() + deadline
    eph = None
    while time.monotonic() < end:
        # Poke the fixed unmapped UDP port throughout (TSIv4 bind exposure).
        try:
            fixed.sendto(b"host-to-fixed\n", ("127.0.0.1", fixed_port))
        except OSError:
            pass
        now = ephemeral_udp_ports(pid) - baseline
        # Ignore the fixed port itself if a build publishes it as a host socket.
        now.discard(fixed_port)
        if now:
            eph = sorted(now)[0]
            # Inject from a host source the guest never sent to. Unmodified
            # upstream forwards it; the 0009 patch admits only reached peers.
            t2 = time.monotonic() + 1.5
            while time.monotonic() < t2:
                inject.sendto(b"host-inject-from-nonpeer\n", ("127.0.0.1", eph))
                time.sleep(0.1)
            print(f"proxy_opened_ephemeral_udp host_port={eph}")
            break
        time.sleep(0.2)
    if eph is None:
        print("no_ephemeral_udp_socket_observed")
    return 0


def main():
    args = sys.argv[1:]
    deadline = 8.0
    if "--deadline" in args:
        i = args.index("--deadline")
        deadline = float(args[i + 1])
        del args[i : i + 2]
    cmd = args[0]
    if cmd == "tcp":
        return tcp(args[1], int(args[2]), deadline)
    if cmd == "udp-inject":
        return udp_inject(int(args[1]), int(args[2]), int(args[3]), deadline)
    print(f"unknown subcommand {cmd}", file=sys.stderr)
    return 2


if __name__ == "__main__":
    sys.exit(main())
