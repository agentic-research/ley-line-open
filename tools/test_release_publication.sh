#!/bin/sh
set -eu

repo_root=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
assets_file="$repo_root/tools/release-assets.txt"
tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/leyline-release-publication.XXXXXX")
trap 'rm -rf "$tmp_dir"' 0 1 2 15

fail() {
    echo "$*" >&2
    exit 1
}

expect_failure() {
    label=$1
    shift
    if "$@"; then
        fail "$label unexpectedly succeeded"
    fi
}

make_source() {
    path=$1
    contents=$2
    printf '%s\n' "$contents" > "$path"
}

stage_target() {
    output_dir=$1
    asset_name=$2
    lib_asset=${3:-}
    include_header=${4:-false}

    OUTPUT_DIR="$output_dir" \
    CLI_SOURCE="$tmp_dir/leyline" \
    ASSET_NAME="$asset_name" \
    LIB_ASSET="$lib_asset" \
    STATICLIB_SOURCE="${lib_asset:+$tmp_dir/libleyline_fs.a}" \
    HEADER_SOURCE="$tmp_dir/leyline_fs.h" \
        "$repo_root/tools/stage_release_artifacts.sh"

    if [ "$include_header" != "true" ]; then
        return
    fi

    # stage_release_artifacts.sh includes the header whenever HEADER_SOURCE is
    # present, so callers that do not own it leave HEADER_SOURCE empty.
}

make_source "$tmp_dir/leyline" '#!/bin/sh'
chmod +x "$tmp_dir/leyline"
make_source "$tmp_dir/libleyline_fs.a" 'staticlib'
make_source "$tmp_dir/leyline_fs.h" 'header'
make_source "$tmp_dir/execution-tools.json" 'tooldefs'

# Generator binaries (ley-line-open-e44960). This list must match the one
# release.yml passes as GENERATOR_BINS and the one
# `task release:generators:target` builds; tools/release-assets.txt asserts the
# resulting `<bin>-<suffix>` names, so all three agreeing is what this fixture
# proves. Fabricated here because the fixture must not depend on a real cargo
# build — the property under test is the STAGING contract, not the compiler.
GENERATOR_BINS='capnpc-schema-bridge-zod capnpc-schema-bridge-go capnpc-schema-bridge-jsonschema capnpc-schema-bridge-tooldefs leyline-mcp-descriptor'
export GENERATOR_BINS
mkdir -p "$tmp_dir/generators"
for generator_bin in $GENERATOR_BINS; do
    make_source "$tmp_dir/generators/$generator_bin" '#!/bin/sh'
    chmod +x "$tmp_dir/generators/$generator_bin"
done
GENERATOR_SOURCE_DIR="$tmp_dir/generators"
export GENERATOR_SOURCE_DIR

# Platform-independent wasm artifacts (ley-line-open-a2099a; envelope joined
# under ley-line-open-be5f86). Deliberately NOT exported: exactly one target
# job stages them (same single-uploader rule as the header), so they ride
# only on the linux-amd64 invocation below. Fabricated bytes for the same
# reason as the generators — the property under test is the staging contract,
# not the compiler.
wasm_assets='leyline_sign.wasm leyline_cas_ffi.wasm leyline_envelope.wasm'
mkdir -p "$tmp_dir/wasm"
for wasm_asset in $wasm_assets; do
    make_source "$tmp_dir/wasm/$wasm_asset" 'wasm'
done

mkdir -p "$tmp_dir/staged"

OUTPUT_DIR="$tmp_dir/staged/leyline-linux-amd64" \
CLI_SOURCE="$tmp_dir/leyline" \
ASSET_NAME="leyline-linux-amd64" \
GENERATOR_SUFFIX="linux-amd64" \
LIB_ASSET="libleyline_fs-linux-amd64.a" \
STATICLIB_SOURCE="$tmp_dir/libleyline_fs.a" \
HEADER_SOURCE="$tmp_dir/leyline_fs.h" \
TOOLDEFS_SOURCE="$tmp_dir/execution-tools.json" \
WASM_ASSETS="$wasm_assets" \
WASM_SOURCE_DIR="$tmp_dir/wasm" \
    "$repo_root/tools/stage_release_artifacts.sh"

OUTPUT_DIR="$tmp_dir/staged/leyline-linux-arm64" \
CLI_SOURCE="$tmp_dir/leyline" \
ASSET_NAME="leyline-linux-arm64" \
GENERATOR_SUFFIX="linux-arm64" \
LIB_ASSET="libleyline_fs-linux-arm64.a" \
STATICLIB_SOURCE="$tmp_dir/libleyline_fs.a" \
    "$repo_root/tools/stage_release_artifacts.sh"

OUTPUT_DIR="$tmp_dir/staged/leyline-darwin-arm64" \
CLI_SOURCE="$tmp_dir/leyline" \
ASSET_NAME="leyline-darwin-arm64" \
GENERATOR_SUFFIX="darwin-arm64" \
LIB_ASSET="libleyline_fs-darwin-arm64.a" \
STATICLIB_SOURCE="$tmp_dir/libleyline_fs.a" \
    "$repo_root/tools/stage_release_artifacts.sh"

OUTPUT_DIR="$tmp_dir/staged/leyline-darwin-amd64" \
CLI_SOURCE="$tmp_dir/leyline" \
ASSET_NAME="leyline-darwin-amd64" \
GENERATOR_SUFFIX="darwin-amd64" \
    "$repo_root/tools/stage_release_artifacts.sh"

publication="$tmp_dir/publication"
"$repo_root/tools/prepare_public_release.sh" \
    "$tmp_dir/staged" "$publication" "$assets_file"
"$repo_root/tools/verify_public_release.sh" "$publication" "$assets_file"

find "$publication" -maxdepth 1 -type f -exec basename {} \; |
    LC_ALL=C sort > "$tmp_dir/published-names"
(
    cat "$assets_file"
    echo SHA256SUMS
) | LC_ALL=C sort > "$tmp_dir/expected-published-names"
diff -u "$tmp_dir/expected-published-names" "$tmp_dir/published-names"

mutation_case() {
    name=$1
    cp -R "$publication" "$tmp_dir/$name"
    printf '%s\n' "$tmp_dir/$name"
}

case_dir=$(mutation_case self-entry)
printf '%064d  SHA256SUMS\n' 0 >> "$case_dir/SHA256SUMS"
expect_failure self-entry \
    "$repo_root/tools/verify_public_release.sh" "$case_dir" "$assets_file"

case_dir=$(mutation_case duplicate-entry)
first_manifest_line=$(sed -n '1p' "$case_dir/SHA256SUMS")
printf '%s\n' "$first_manifest_line" >> "$case_dir/SHA256SUMS"
expect_failure duplicate-entry \
    "$repo_root/tools/verify_public_release.sh" "$case_dir" "$assets_file"

case_dir=$(mutation_case missing-file)
rm "$case_dir/leyline-darwin-amd64"
expect_failure missing-file \
    "$repo_root/tools/verify_public_release.sh" "$case_dir" "$assets_file"

# The wasm slot specifically: a single-uploader asset that only one job
# stages is exactly the kind that can vanish while every per-platform set
# stays complete.
case_dir=$(mutation_case missing-wasm)
rm "$case_dir/leyline_sign.wasm"
expect_failure missing-wasm \
    "$repo_root/tools/verify_public_release.sh" "$case_dir" "$assets_file"

# One case per wasm asset, not one shared case: a single multi-removal run
# would pass as long as ANY missing artifact is noticed, leaving the others
# free to vanish undetected. The envelope artifact is the newest and so the
# likeliest to be dropped from a build matrix edit (ley-line-open-be5f86).
case_dir=$(mutation_case missing-wasm-envelope)
rm "$case_dir/leyline_envelope.wasm"
expect_failure missing-wasm-envelope \
    "$repo_root/tools/verify_public_release.sh" "$case_dir" "$assets_file"

case_dir=$(mutation_case extra-file)
make_source "$case_dir/extra" extra
expect_failure extra-file \
    "$repo_root/tools/verify_public_release.sh" "$case_dir" "$assets_file"

case_dir=$(mutation_case corrupt-file)
printf 'corruption' >> "$case_dir/leyline-linux-amd64"
expect_failure corrupt-file \
    "$repo_root/tools/verify_public_release.sh" "$case_dir" "$assets_file"

case_dir=$(mutation_case malformed-entry)
printf 'not-a-digest  leyline-linux-amd64\n' >> "$case_dir/SHA256SUMS"
expect_failure malformed-entry \
    "$repo_root/tools/verify_public_release.sh" "$case_dir" "$assets_file"

cp -R "$tmp_dir/staged" "$tmp_dir/corrupt-staged"
printf 'corruption' >> \
    "$tmp_dir/corrupt-staged/leyline-linux-amd64/leyline-linux-amd64"
expect_failure corrupt-staged \
    "$repo_root/tools/prepare_public_release.sh" \
    "$tmp_dir/corrupt-staged" "$tmp_dir/must-not-exist" "$assets_file"
test ! -e "$tmp_dir/must-not-exist" ||
    fail "failed preparation mutated its destination"

cp -R "$tmp_dir/staged" "$tmp_dir/malformed-staged"
printf 'not-a-digest  leyline-linux-amd64\n' >> \
    "$tmp_dir/malformed-staged/leyline-linux-amd64/SHA256SUMS"
expect_failure malformed-staged \
    "$repo_root/tools/verify_release_artifacts.sh" \
    "$tmp_dir/malformed-staged"

cat > "$tmp_dir/fake-gh" <<'EOF'
#!/bin/sh
set -eu
printf '%s\n' "$*" >> "$GH_LOG"
if [ "$1 $2" = "release view" ]; then
    exit 1
fi
EOF
chmod +x "$tmp_dir/fake-gh"

case_dir=$(mutation_case blocked-upload)
printf 'corruption' >> "$case_dir/leyline-linux-amd64"
export GH_LOG="$tmp_dir/gh.log"
expect_failure blocked-upload \
    env GH_BIN="$tmp_dir/fake-gh" \
    "$repo_root/tools/publish_release_assets.sh" \
    "$case_dir" v0.10.4 "$assets_file"
test ! -e "$GH_LOG" || fail "gh was invoked after verification failed"

GH_BIN="$tmp_dir/fake-gh" \
    "$repo_root/tools/publish_release_assets.sh" \
    "$publication" v0.10.4 "$assets_file"
grep -qx 'release view v0.10.4' "$GH_LOG"
grep -qx 'release create v0.10.4 --title v0.10.4 --generate-notes' "$GH_LOG"
grep -q '^release upload v0.10.4 ' "$GH_LOG"
test "$(grep -c '^release upload v0.10.4 ' "$GH_LOG")" -eq 1

echo "release publication fixture proved exact assets and fail-closed mutation"
