#!/bin/sh
# The C compiler for libkrun's guest init (bead ley-line-open-f1c46f).
#
# rs/third_party/libkrun/src/init_blob/build.rs compiles init.c into a static
# Linux executable and embeds it in the VMM as /init.krun, the first process
# every guest runs. It invokes `$CC_LINUX -O2 -static -Wall -o <out> <srcs>`;
# .cargo/config.toml points CC_LINUX here. The init comes from the vendored
# source on every host: nothing is read from an installed libkrun.
#
# The guest is Linux on the target's architecture, whatever the host is:
#
#   Linux host, same arch   the host C compiler. Static glibc, as upstream's
#                           Makefile builds it on Linux.
#   any other host          `zig cc` against musl, which ships its own libc
#                           and kernel headers, so no Linux sysroot is needed.
#                           Upstream's macOS build downloads a Debian sysroot
#                           instead; zig replaces that download.
#
# CARGO_CFG_TARGET_ARCH is set by cargo for build scripts and inherited here.
# Set CC_LINUX in the environment to use a different compiler; cargo's [env]
# does not override it.
set -eu

host_arch=$(uname -m)
case "$host_arch" in
    arm64) host_arch=aarch64 ;;
esac
target_arch=${CARGO_CFG_TARGET_ARCH:-$host_arch}

if [ "$(uname -s)" = Linux ] && [ "$target_arch" = "$host_arch" ]; then
    exec "${CC:-cc}" "$@"
fi

if ! command -v zig >/dev/null 2>&1; then
    echo "krun_init_cc: building libkrun's $target_arch Linux guest init on" >&2
    echo "$(uname -s)/$host_arch needs zig (brew install zig, or pip install ziglang)," >&2
    echo "or CC_LINUX naming a compiler that emits static $target_arch Linux binaries." >&2
    exit 1
fi
# -s: no debug sections in a binary embedded in every VMM.
exec zig cc -target "$target_arch-linux-musl" -s "$@"
