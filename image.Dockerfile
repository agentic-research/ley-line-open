# image.Dockerfile — assemble LLO OCI image from a krust-built static musl binary.
#
# Build is a two-step flow:
#   1. krust + cargo-zigbuild produces a static musl binary at
#      rs/ll-open/cli/target/krust/<arch>-unknown-linux-musl/release/leyline
#      (no docker daemon involved; native cross-compile via zig).
#   2. This Dockerfile drops that binary onto chainguard/static:latest
#      (distroless, nonroot uid 65532) — a single COPY, no Rust toolchain in the
#      container, no virtiofs bottleneck.
#
# `task image` passes BIN_PATH derived from the requested platform so this
# Dockerfile works for both linux/arm64 and linux/amd64 builds. The default
# below matches the M-series dev path; CI / cross-arch builds override.
#
# CMD includes `--mcp-bind 0.0.0.0 --mcp-allow-public` so docker
# `-p host:8384` reaches the MCP HTTP server. The daemon defaults bind to
# 127.0.0.1, which is loopback-only inside the container — without
# `--mcp-bind 0.0.0.0`, port publishing yields connection-reset on every
# request from the host. `--mcp-allow-public` is the deliberate opt-in
# required by bead `ley-line-open-b7dd03` — outside a container, the
# combo would refuse to start without it; in here it's correct plumbing.
#
# Security: 0.0.0.0 here is the container's network namespace, NOT the host.
# The container has its own netns; this only exposes :8384 to interfaces inside
# that netns. Whether MCP is reachable from outside the host depends on HOW
# you publish the port:
#
#   docker run -p 18384:8384 ...               ← exposes on host's 0.0.0.0:18384
#                                                (visible on the LAN)
#   docker run -p 127.0.0.1:18384:8384 ...     ← exposes on host's loopback only
#                                                (recommended for local cloister)
#
# Cloister hits `http://localhost:8384/mcp`, so loopback-publishing is the
# right shape for production: the container-side 0.0.0.0 is just plumbing
# for docker's NAT to work, the host-side 127.0.0.1 keeps the surface narrow.
# Per `daemon/mcp.rs`, the MCP server "assumes localhost-only or already-
# attested" — there is no auth on the wire, so do not publish to a public
# host IP without a reverse proxy / mTLS / cloister attestation in front.

ARG BIN_PATH=rs/ll-open/cli/target/krust/aarch64-unknown-linux-musl/release/leyline

FROM cgr.dev/chainguard/static:latest

ARG BIN_PATH
COPY ${BIN_PATH} /usr/bin/leyline

ENV HOME=/tmp \
    RUST_LOG=info

EXPOSE 8384

ENTRYPOINT ["/usr/bin/leyline"]
CMD ["daemon", "--mcp-port", "8384", "--mcp-bind", "0.0.0.0", "--mcp-allow-public"]
