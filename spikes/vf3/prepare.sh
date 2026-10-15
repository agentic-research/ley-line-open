#!/usr/bin/env bash
# V-F3 inputs, all under a scratch directory ($1):
#   corpus/   >=20k real source files in one git repo (one commit, packed)
#   rootfs/   Alpine 3.24 aarch64 minirootfs + git + ripgrep (+ their libs)
#             + /vf3guest + /tarball.tar
#   tarball.tar  1,000 files from the corpus (ustar, no macOS metadata)
#   vf3guest-mac the same workloads built for the host (APFS leg)
#
# Downloads are untrusted data: each lands in its own empty directory, is
# hashed, and only the Alpine binaries run, only inside the guest.
set -euo pipefail

S=${1:?scratch dir}
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$HERE/../.." && pwd)
MACHE=${VF3_MACHE:-$HOME/remotes/art/mache}
REGISTRY=${VF3_REGISTRY:-$(ls -d "$HOME"/.cargo/registry/src/index.crates.io-*/ | head -1)}
ALPINE=https://dl-cdn.alpinelinux.org/alpine/v3.24
PKGS="main/git-2.54.0-r0 community/ripgrep-15.1.0-r0 main/pcre2-10.49-r0 main/zlib-1.3.2-r1 main/libexpat-2.8.5-r0 main/libgcc-15.2.0-r5"
MINIROOTFS=alpine-minirootfs-3.24.2-aarch64.tar.gz
TARGET_FILES=${VF3_TARGET_FILES:-21000}

mkdir -p "$S"
cd "$S"

# --- downloads -------------------------------------------------------------
if [ ! -d dl ]; then
  mkdir dl
  mkdir dl/minirootfs
  (cd dl/minirootfs && curl -sSfO "$ALPINE/releases/aarch64/$MINIROOTFS" \
    && curl -sSfO "$ALPINE/releases/aarch64/$MINIROOTFS.sha256" \
    && shasum -a 256 -c "$MINIROOTFS.sha256")
  for spec in $PKGS; do
    n=$(basename "$spec")
    mkdir "dl/$n"
    (cd "dl/$n" && curl -sSfO "$ALPINE/${spec%%/*}/aarch64/$n.apk")
  done
  (cd dl && shasum -a 256 ./*/*.apk ./*/*.tar.gz > sha256.txt)
fi
cat dl/sha256.txt

# --- rootfs ----------------------------------------------------------------
if [ ! -d rootfs ]; then
  mkdir rootfs
  tar -xzf "dl/minirootfs/$MINIROOTFS" -C rootfs
  for a in dl/*/*.apk; do
    tar -xzf "$a" -C rootfs --exclude '.SIGN*' --exclude '.PKGINFO' \
      --exclude '.pre-*' --exclude '.post-*' --exclude '.trigger'
  done
fi

# --- corpus ----------------------------------------------------------------
if [ ! -d corpus/.git ]; then
  rm -rf corpus
  mkdir -p corpus/llo corpus/mache corpus/crates
  # This repository's working tree, without build outputs.
  (cd "$REPO" && git ls-files -z rs docs clients | rsync -a --from0 --files-from=- ./ "$S/corpus/llo/")
  # Another real tree: mache's tracked source.
  (cd "$MACHE" && git ls-files -z | rsync -a --from0 --files-from=- ./ "$S/corpus/mache/")
  # Real Rust crate sources, alphabetical, until the corpus holds TARGET_FILES.
  for crate in $(ls "$REGISTRY" | sort); do
    have=$(find corpus -type f | wc -l)
    [ "$have" -ge "$TARGET_FILES" ] && break
    rsync -a --exclude '.git' --exclude 'target' "$REGISTRY/$crate/" "corpus/crates/$crate/"
  done
  # Regular files and directories only; nothing over 4 MiB.
  find corpus -type l -delete
  find corpus -type f -size +4M -delete
  find corpus -name '.gitignore' -delete
  find corpus -name '.git' -prune -exec rm -rf {} +
  # Untracked here only because a global excludes file hides it; the guest has none.
  rm -rf corpus/mache/_agent_log
  (cd corpus && git init -q -b main \
    && git config core.checkStat minimal && git config core.trustctime false \
    && git config user.email vf3@example.invalid && git config user.name vf3 \
    && git config core.untrackedCache false && git config core.fsmonitor false \
    && git config core.autocrlf false && git config core.hooksPath /dev/null \
    && git config gc.auto 0 && git config maintenance.auto false \
    && git config core.excludesFile /dev/null \
    && git add -A && git commit -qm "V-F3 corpus" && git gc -q)
fi
echo "corpus files (worktree): $(find corpus -type f -not -path '*/.git/*' | wc -l)"
echo "corpus files (incl .git): $(find corpus -type f | wc -l)"
du -sh corpus

# --- tarball: 1,000 corpus files ------------------------------------------
if [ ! -f tarball.tar ]; then
  (cd corpus && git ls-files > ../corpus.list && head -1000 ../corpus.list > ../tarball.list \
    && COPYFILE_DISABLE=1 tar --format ustar --no-xattrs --no-mac-metadata -cf ../tarball.tar -T ../tarball.list)
fi
cp -f tarball.tar rootfs/tarball.tar

# --- workloads -------------------------------------------------------------
rustc --edition 2021 -O --target aarch64-unknown-linux-musl -C target-feature=+crt-static \
  -C linker=rust-lld -C strip=symbols -o rootfs/vf3guest "$HERE/guest/vf3guest.rs"
rustc --edition 2021 -O -o vf3guest-mac "$HERE/guest/vf3guest.rs"
mkdir -p rootfs/work rootfs/tmp

# --- VMM: LLO's linked libkrun + the in-memory backend, hypervisor-entitled --
(cd "$HERE" && CARGO_TARGET_DIR="$S/target" cargo build --release)
cp -f "$S/target/release/vf3-vmm" "$S/vf3-vmm"
codesign --force --sign - --entitlements "$HERE/hypervisor.entitlements.plist" "$S/vf3-vmm"
# libkrun dlopens the guest kernel; expose only libkrunfw on the loader path.
mkdir -p "$S/fw"
ln -sf "$(brew --prefix)/lib/libkrunfw.5.dylib" "$S/fw/libkrunfw.5.dylib"
echo "prepared $S"
