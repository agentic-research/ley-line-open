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
# CMD includes `--mcp-bind 0.0.0.0` so docker `-p host:8384` reaches the MCP
# HTTP server. The daemon defaults bind to 127.0.0.1, which is loopback-only
# inside the container — without this override, port publishing yields
# connection-reset on every request from the host.

ARG BIN_PATH=rs/ll-open/cli/target/krust/aarch64-unknown-linux-musl/release/leyline

FROM cgr.dev/chainguard/static:latest

ARG BIN_PATH
COPY ${BIN_PATH} /usr/bin/leyline

ENV HOME=/tmp \
    RUST_LOG=info

EXPOSE 8384

ENTRYPOINT ["/usr/bin/leyline"]
CMD ["daemon", "--mcp-port", "8384", "--mcp-bind", "0.0.0.0"]
