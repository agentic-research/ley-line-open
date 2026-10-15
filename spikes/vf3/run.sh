#!/usr/bin/env bash
# V-F3 driver. Paths measured: (0) plain APFS directories, (1) a libkrun guest
# whose /work is virtio-fs served from the VMM's memory (src/main.rs).
#
# NO host NFS, fuse-t, macFUSE or FSKit leg: a loopback NFS leg of an earlier
# attempt panicked the host kernel (com.apple.filesystems.nfs,
# bead ley-line-open-2d49ce). Nothing here mounts anything on the host.
#
# usage: run.sh <scratch dir prepared by prepare.sh> <out dir> [N]
set -uo pipefail

S=${1:?scratch dir}
OUT=${2:?out dir}
N=${3:-5}
mkdir -p "$OUT"

WORKLOADS="status rg find edit create untar"
VARIANTS="t0:ttl=0 t3600:ttl=3600 plus:ttl=3600,plus cache:ttl=3600,plus,cache"
DAX_VARIANT="dax:ttl=3600,plus,cache,dax"
DAX_WORKLOADS="status rg find"

vmm() { # label variant guest-env...
  local label=$1 variant=$2
  shift 2
  DYLD_LIBRARY_PATH="$S/fw" VF3_CORPUS="$S/corpus" VF3_ROOTFS="$S/rootfs" \
    VF3_VARIANT="$variant" VF3_GUEST="VF3_LABEL=$label $*" \
    timeout 900 "$S/vf3-vmm" >>"$OUT/$label.out" 2>>"$OUT/$label.err"
  local rc=$?
  [ $rc -eq 0 ] || echo "FAIL $label rc=$rc $*" | tee -a "$OUT/failures.txt"
}

apfs() { # label guest-env...
  local label=$1
  shift
  rm -rf "$S/clone"
  cp -c -R -p "$S/corpus" "$S/clone"
  env VF3_ROOT="$S/clone" VF3_TARBALL="$S/tarball.tar" VF3_LABEL="$label" "$@" \
    "$S/vf3guest-mac" >>"$OUT/$label.out" 2>>"$OUT/$label.err" \
    || echo "FAIL $label $*" | tee -a "$OUT/failures.txt"
}

{
  echo "date: $(date -u +%FT%TZ)"
  sw_vers
  sysctl -n machdep.cpu.brand_string hw.ncpu hw.memsize
  echo "libkrunfw: $(readlink -f /opt/homebrew/lib/libkrunfw.5.dylib)"
  echo "host git: $(git --version); host rg: $(rg --version | head -1)"
  echo "N=$N variants=$VARIANTS $DAX_VARIANT"
} >"$OUT/env.txt"

# D: correctness, once per variant.
for spec in $VARIANTS; do
  vmm "vf-${spec%%:*}-correct" "${spec#*:}" VF3_MODE=cold VF3_WORKLOADS=correct
done

for i in $(seq 1 "$N"); do
  for w in $WORKLOADS; do
    apfs apfs-cold VF3_MODE=cold VF3_WORKLOADS=$w
    for spec in $VARIANTS; do
      vmm "vf-${spec%%:*}-cold" "${spec#*:}" VF3_MODE=cold VF3_WORKLOADS=$w
    done
  done
  for w in $DAX_WORKLOADS; do
    vmm "vf-${DAX_VARIANT%%:*}-cold" "${DAX_VARIANT#*:}" VF3_MODE=cold VF3_WORKLOADS=$w
  done
  echo "cold iteration $i done $(date +%T)"
done

all=$(echo $WORKLOADS | tr ' ' ',')
apfs apfs-warm VF3_MODE=warm VF3_N="$N" VF3_WORKLOADS="$all"
for spec in $VARIANTS; do
  vmm "vf-${spec%%:*}-warm" "${spec#*:}" VF3_MODE=warm VF3_N="$N" VF3_WORKLOADS="$all"
done
vmm "vf-${DAX_VARIANT%%:*}-warm" "${DAX_VARIANT#*:}" VF3_MODE=warm VF3_N="$N" \
  VF3_WORKLOADS="$(echo $DAX_WORKLOADS | tr ' ' ',')"
rm -rf "$S/clone"
echo "done $(date +%T)"
