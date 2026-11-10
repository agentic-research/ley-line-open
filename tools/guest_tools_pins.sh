# The guest-tools pins (bead ley-line-open-ae6b99), sourced by
# tools/guest_tools.sh and tools/guest_tools_mirror.sh so both read one list.
#
# CAS is this machine's content-addressed store of pins it has fetched, one
# file per pin named by its SHA-256; ALPINE is where each pin was first
# fetched; MIRROR is LLO's content-addressed copy, one release asset per pin
# named by its SHA-256. Each may be overridden from the environment (MIRROR
# and ALPINE as URLs, file:// included; CAS as a directory).

CAS=${LEYLINE_GUEST_TOOLS_CAS:-${HOME:?}/.cache/leyline/pins}
ALPINE=${LEYLINE_GUEST_TOOLS_ALPINE:-https://dl-cdn.alpinelinux.org/alpine/v3.24}
MIRROR=${LEYLINE_GUEST_TOOLS_MIRROR:-https://github.com/agentic-research/ley-line-open/releases/download/guest-tools-pins}

# path-under-ALPINE  sha256
PINS='main/aarch64/git-2.54.0-r0.apk ea41e2f837f1f6a8b66e72a5ece0fc75cca44f76f49cf5500d467fc5627d0ab7
community/aarch64/ripgrep-15.1.0-r0.apk 1bf67cfa4464bc91e1dd1a4f537c40664c473ce92bf16ebba64bc7968d7545eb
main/aarch64/pcre2-10.49-r0.apk 03efd0b29f7f38deb10a9ccb54f56e19683eb7ac27eb1f347e6dbd2e59ea000d
main/aarch64/zlib-1.3.2-r1.apk 1f77a542ac6634761b9fe38d8bdca19b154071dcfa25432e73f2bc8a0740a33f
main/aarch64/libexpat-2.10.0-r0.apk 99144fe0b1bb9316a3bc30c38707996767806661f4a0456fcf4565ac92e79b0f
main/aarch64/libgcc-15.2.0-r5.apk 369aaa6e9d099a737bad6dd3e6c2fe7bb1547ca26d22b94ee0411228f709b403
releases/aarch64/alpine-minirootfs-3.24.2-aarch64.tar.gz 9bf70a7f18ea44094cbb5f70c58f9af129c8214745743db0e68e5502cc2ce773'
