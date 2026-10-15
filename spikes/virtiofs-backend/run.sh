#!/usr/bin/env bash
# ADR-0044 spike: build a patched libkrun v1.19.4, an LLO-owned VMM that serves
# a virtio-fs share from its own memory, and a static guest; boot the guest.
#
# usage: run.sh <work-dir>
#   <work-dir> receives the cargo target dir, the rootfs and the init binary.
# Needs: macOS arm64 with Homebrew libkrun 1.19.4 + libkrunfw 5 (the init blob is
# read from the installed libkrun; libkrunfw is dlopened), rustup target
# aarch64-unknown-linux-musl, python3.
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
work="$(mkdir -p "$1" && cd "$1" && pwd)"
vendor="$here/vendor/libkrun"

if [ ! -d "$vendor" ]; then
  git clone -q --depth 1 --branch v1.19.4 https://github.com/containers/libkrun.git "$vendor"
  git -C "$vendor" apply "$here/libkrun-v1.19.4-custom-fs.patch"
fi

# libkrun embeds init.krun at build time; reuse the installed library's blob
# instead of cross-compiling init.c.
python3 -I - "$work/init.krun" <<'EOF'
import ctypes, sys
lib = ctypes.CDLL("/opt/homebrew/lib/libkrun.dylib")
p = ctypes.POINTER(ctypes.c_uint8)(); n = ctypes.c_size_t()
assert lib.krun_get_default_init(ctypes.byref(p), ctypes.byref(n)) == 0
open(sys.argv[1], "wb").write(ctypes.string_at(p, n.value))
EOF

export CARGO_TARGET_DIR="$work/target" KRUN_INIT_BINARY_PATH="$work/init.krun" RUSTC_WRAPPER=
cargo build --release --manifest-path "$here/Cargo.toml"
bin="$CARGO_TARGET_DIR/release/llo-virtiofs-spike"

cat > "$work/entitlements.plist" <<'EOF'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>com.apple.security.hypervisor</key><true/>
</dict></plist>
EOF
codesign --force --sign - --entitlements "$work/entitlements.plist" "$bin"

rm -rf "$work/rootfs" "$work/daxdir"
mkdir -p "$work/rootfs/llo" "$work/rootfs/llodax" "$work/rootfs/dax" "$work/daxdir"
rustc --edition 2021 --target aarch64-unknown-linux-musl -C linker=rust-lld \
  -C target-feature=+crt-static -O "$here/guest/guest.rs" -o "$work/rootfs/guest"
echo "dax probe bytes from a host file" > "$work/daxdir/probe.txt"

DYLD_FALLBACK_LIBRARY_PATH=/opt/homebrew/lib "$bin" "$work/rootfs" "$work/daxdir"
