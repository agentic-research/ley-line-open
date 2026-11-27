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

# The execution/v1 schema bytes (ley-line-open-6d811a) are staged from the
# REAL schema-spec tree, not fabricated: the property under test includes the
# stage script's import guard, which must accept the shipped execution.capnp.
schema_root="$repo_root/rs/ll-core/schema-spec"

mkdir -p "$tmp_dir/staged"

OUTPUT_DIR="$tmp_dir/staged/leyline-linux-amd64" \
CLI_SOURCE="$tmp_dir/leyline" \
ASSET_NAME="leyline-linux-amd64" \
GENERATOR_SUFFIX="linux-amd64" \
LIB_ASSET="libleyline_fs-linux-amd64.a" \
STATICLIB_SOURCE="$tmp_dir/libleyline_fs.a" \
HEADER_SOURCE="$tmp_dir/leyline_fs.h" \
TOOLDEFS_SOURCE="$tmp_dir/execution-tools.json" \
EXECUTION_SCHEMA_ROOT="$schema_root" \
WASM_ASSETS="$wasm_assets" \
WASM_SOURCE_DIR="$tmp_dir/wasm" \
    "$repo_root/tools/stage_release_artifacts.sh"

# The schema assets are the repository bytes, unrenamed in content.
cmp "$tmp_dir/staged/leyline-linux-amd64/execution.capnp" \
    "$schema_root/execution/v1/execution.capnp" ||
    fail "staged execution.capnp differs from the schema-spec source"
cmp "$tmp_dir/staged/leyline-linux-amd64/traits.capnp" \
    "$schema_root/_traits.capnp" ||
    fail "staged traits.capnp differs from the schema-spec source"

# Staging refuses a schema tree it cannot ship whole: a missing traits file,
# and an execution.capnp that imports something outside the shipped pair.
stage_schema_case() {
    case_root=$1
    OUTPUT_DIR="$tmp_dir/schema-case-$(basename "$case_root")" \
    CLI_SOURCE="$tmp_dir/leyline" \
    ASSET_NAME="leyline-linux-amd64" \
    EXECUTION_SCHEMA_ROOT="$case_root" \
        "$repo_root/tools/stage_release_artifacts.sh"
}

mkdir -p "$tmp_dir/schema-missing-traits/execution/v1"
cp "$schema_root/execution/v1/execution.capnp" \
    "$tmp_dir/schema-missing-traits/execution/v1/execution.capnp"
expect_failure schema-missing-traits \
    stage_schema_case "$tmp_dir/schema-missing-traits"

mkdir -p "$tmp_dir/schema-extra-import/execution/v1"
cp "$schema_root/_traits.capnp" "$tmp_dir/schema-extra-import/_traits.capnp"
{
    cat "$schema_root/execution/v1/execution.capnp"
    echo 'using Other = import "../other.capnp";'
} > "$tmp_dir/schema-extra-import/execution/v1/execution.capnp"
expect_failure schema-extra-import \
    stage_schema_case "$tmp_dir/schema-extra-import"

OUTPUT_DIR="$tmp_dir/staged/leyline-linux-arm64" \
CLI_SOURCE="$tmp_dir/leyline" \
ASSET_NAME="leyline-linux-arm64" \
GENERATOR_SUFFIX="linux-arm64" \
LIB_ASSET="libleyline_fs-linux-arm64.a" \
STATICLIB_SOURCE="$tmp_dir/libleyline_fs.a" \
    "$repo_root/tools/stage_release_artifacts.sh"

# The microVM worker (ley-line-open-984acf) rides the darwin-arm64 leg. On
# macOS the stand-in is a real Mach-O (a copy of /usr/bin/true), so staging
# signs it for real and the entitlement is asserted below with codesign on
# the STAGED bytes. Elsewhere codesign does not exist: the stand-in only
# proves the asset inventory, and the skip says so.
worker_asset=leyline-krun-worker-darwin-arm64
if [ "$(uname -s)" = Darwin ]; then
    cp /usr/bin/true "$tmp_dir/leyline-krun-worker"
    worker_check=codesign
else
    make_source "$tmp_dir/leyline-krun-worker" '#!/bin/sh'
    worker_check=skipped
    RELEASE_FIXTURE_UNCHECKED_WORKER=1
    export RELEASE_FIXTURE_UNCHECKED_WORKER
fi

# The connection broker (ley-line-open-caa20b) rides the same leg. Its
# stand-in is fabricated bytes: staging adds no signature to it, so the
# property under test is the inventory and the unconfined refusal.
broker_asset=leyline-conn-broker-darwin-arm64
make_source "$tmp_dir/leyline-conn-broker" '#!/bin/sh'
chmod +x "$tmp_dir/leyline-conn-broker"

OUTPUT_DIR="$tmp_dir/staged/leyline-darwin-arm64" \
CLI_SOURCE="$tmp_dir/leyline" \
ASSET_NAME="leyline-darwin-arm64" \
GENERATOR_SUFFIX="darwin-arm64" \
LIB_ASSET="libleyline_fs-darwin-arm64.a" \
STATICLIB_SOURCE="$tmp_dir/libleyline_fs.a" \
KRUN_WORKER_SOURCE="$tmp_dir/leyline-krun-worker" \
KRUN_WORKER_ASSET="$worker_asset" \
CONN_BROKER_SOURCE="$tmp_dir/leyline-conn-broker" \
CONN_BROKER_ASSET="$broker_asset" \
    "$repo_root/tools/stage_release_artifacts.sh"

staged_worker="$tmp_dir/staged/leyline-darwin-arm64/$worker_asset"
grep -q "  $worker_asset\$" "$tmp_dir/staged/leyline-darwin-arm64/SHA256SUMS" ||
    fail "the staged SHA256SUMS does not list $worker_asset"
grep -q "  $broker_asset\$" "$tmp_dir/staged/leyline-darwin-arm64/SHA256SUMS" ||
    fail "the staged SHA256SUMS does not list $broker_asset"
cmp "$tmp_dir/staged/leyline-darwin-arm64/$broker_asset" "$tmp_dir/leyline-conn-broker" ||
    fail "staged $broker_asset differs from its build output"
test -x "$tmp_dir/staged/leyline-darwin-arm64/$broker_asset" ||
    fail "staged $broker_asset is not executable"

# The test-only unconfined broker is never staged, whichever input names it.
make_source "$tmp_dir/leyline-conn-broker-unconfined" '#!/bin/sh'
stage_broker_case() {
    case_name=$1
    shift
    env OUTPUT_DIR="$tmp_dir/broker-case-$case_name" \
        CLI_SOURCE="$tmp_dir/leyline" \
        ASSET_NAME="leyline-darwin-arm64" \
        GENERATOR_BINS= \
        "$@" \
        "$repo_root/tools/stage_release_artifacts.sh"
}
expect_failure unconfined-broker-source \
    stage_broker_case source \
    CONN_BROKER_SOURCE="$tmp_dir/leyline-conn-broker-unconfined" \
    CONN_BROKER_ASSET="$broker_asset"
expect_failure unconfined-broker-asset \
    stage_broker_case asset \
    CONN_BROKER_SOURCE="$tmp_dir/leyline-conn-broker" \
    CONN_BROKER_ASSET=leyline-conn-broker-unconfined-darwin-arm64
mkdir -p "$tmp_dir/unconfined-generator"
make_source "$tmp_dir/unconfined-generator/leyline-conn-broker-unconfined" '#!/bin/sh'
expect_failure unconfined-broker-generator \
    stage_broker_case generator \
    GENERATOR_BINS=leyline-conn-broker-unconfined \
    GENERATOR_SOURCE_DIR="$tmp_dir/unconfined-generator" \
    GENERATOR_SUFFIX=darwin-arm64
for case_name in source asset generator; do
    test ! -e "$tmp_dir/broker-case-$case_name/SHA256SUMS" ||
        fail "refused broker staging ($case_name) wrote a SHA256SUMS"
done
# Only a darwin broker is staged: off macOS it refuses before ready.
expect_failure linux-broker-asset \
    stage_broker_case linux \
    CONN_BROKER_SOURCE="$tmp_dir/leyline-conn-broker" \
    CONN_BROKER_ASSET=leyline-conn-broker-linux-arm64
if [ "$worker_check" = codesign ]; then
    /usr/bin/codesign -d --entitlements - --xml "$staged_worker" 2>/dev/null |
        tr -d ' \t\r\n' |
        grep -q '<key>com.apple.security.hypervisor</key><true/>' ||
        fail "staged $worker_asset lacks com.apple.security.hypervisor"

    # A worker that loses its entitlement after staging is caught by the
    # same check staging runs.
    cp "$staged_worker" "$tmp_dir/unentitled-worker"
    /usr/bin/codesign --force --sign - "$tmp_dir/unentitled-worker" 2>/dev/null
    expect_failure unentitled-worker-check \
        "$repo_root/tools/sign_krun_worker.sh" --check "$tmp_dir/unentitled-worker"
else
    echo "SKIPPED (NOT a pass): the $worker_asset entitlement check needs macOS codesign" >&2
    # Off macOS, staging a darwin worker without the fixture's explicit
    # opt-out is refused: nothing there could sign or check it.
    expect_failure darwin-worker-off-macos \
        env -u RELEASE_FIXTURE_UNCHECKED_WORKER \
        OUTPUT_DIR="$tmp_dir/off-macos-worker" \
        CLI_SOURCE="$tmp_dir/leyline" \
        ASSET_NAME="leyline-darwin-arm64" \
        KRUN_WORKER_SOURCE="$tmp_dir/leyline-krun-worker" \
        KRUN_WORKER_ASSET="$worker_asset" \
        "$repo_root/tools/stage_release_artifacts.sh"
fi

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

# The execution/v1 schema slot (ley-line-open-6d811a): single-uploader like
# the wasm set, so one case per asset for the same reason as above. Both
# assets must also be digest-listed: the published SHA256SUMS names each.
grep -q '  execution\.capnp$' "$publication/SHA256SUMS" ||
    fail "published SHA256SUMS does not list execution.capnp"
grep -q '  traits\.capnp$' "$publication/SHA256SUMS" ||
    fail "published SHA256SUMS does not list traits.capnp"

case_dir=$(mutation_case missing-execution-schema)
rm "$case_dir/execution.capnp"
expect_failure missing-execution-schema \
    "$repo_root/tools/verify_public_release.sh" "$case_dir" "$assets_file"

case_dir=$(mutation_case missing-traits-schema)
rm "$case_dir/traits.capnp"
expect_failure missing-traits-schema \
    "$repo_root/tools/verify_public_release.sh" "$case_dir" "$assets_file"

# Staged but unlisted: a release-assets.txt that forgets the schema assets
# must refuse a staged set carrying them, at preparation and at verification.
grep -vx 'execution\.capnp' "$assets_file" |
    grep -vx 'traits\.capnp' > "$tmp_dir/assets-without-schema"
expect_failure unlisted-schema-prepare \
    "$repo_root/tools/prepare_public_release.sh" \
    "$tmp_dir/staged" "$tmp_dir/unlisted-must-not-exist" \
    "$tmp_dir/assets-without-schema"
test ! -e "$tmp_dir/unlisted-must-not-exist" ||
    fail "failed preparation mutated its destination"
expect_failure unlisted-schema-verify \
    "$repo_root/tools/verify_public_release.sh" \
    "$publication" "$tmp_dir/assets-without-schema"

# The worker rides one leg only, so it too can vanish while every other
# per-platform set stays complete (ley-line-open-984acf).
case_dir=$(mutation_case missing-krun-worker)
rm "$case_dir/$worker_asset"
expect_failure missing-krun-worker \
    "$repo_root/tools/verify_public_release.sh" "$case_dir" "$assets_file"

# The broker rides one leg too (ley-line-open-caa20b), and is digest-listed.
grep -q "  $broker_asset\$" "$publication/SHA256SUMS" ||
    fail "published SHA256SUMS does not list $broker_asset"
case_dir=$(mutation_case missing-conn-broker)
rm "$case_dir/$broker_asset"
expect_failure missing-conn-broker \
    "$repo_root/tools/verify_public_release.sh" "$case_dir" "$assets_file"

# A staging directory containing the unconfined broker is refused by name,
# even when everything else agrees: its own SHA256SUMS lists it and so does
# the asset contract handed to preparation. Only the name guard can refuse it.
unconfined_asset=leyline-conn-broker-unconfined-darwin-arm64
cp "$assets_file" "$tmp_dir/assets-with-unconfined"
echo "$unconfined_asset" >> "$tmp_dir/assets-with-unconfined"
cp -R "$tmp_dir/staged" "$tmp_dir/unconfined-staged"
unconfined_dir="$tmp_dir/unconfined-staged/leyline-darwin-arm64"
make_source "$unconfined_dir/$unconfined_asset" '#!/bin/sh'
(
    cd "$unconfined_dir"
    for file in *; do
        test "$file" = SHA256SUMS && continue
        if command -v sha256sum >/dev/null 2>&1; then
            sha256sum "$file"
        else
            shasum -a 256 "$file"
        fi
    done | LC_ALL=C sort -k 2 > SHA256SUMS
)
grep -q "  $unconfined_asset\$" "$unconfined_dir/SHA256SUMS" ||
    fail "fixture: the unconfined case's SHA256SUMS does not list it"
expect_failure unconfined-staged-verify \
    "$repo_root/tools/verify_release_artifacts.sh" "$tmp_dir/unconfined-staged"
expect_failure unconfined-staged-prepare \
    "$repo_root/tools/prepare_public_release.sh" \
    "$tmp_dir/unconfined-staged" "$tmp_dir/unconfined-must-not-exist" \
    "$tmp_dir/assets-with-unconfined"
test ! -e "$tmp_dir/unconfined-must-not-exist" ||
    fail "failed preparation mutated its destination"
# The refusal names the binary.
"$repo_root/tools/verify_release_artifacts.sh" "$tmp_dir/unconfined-staged" \
    >/dev/null 2>"$tmp_dir/unconfined-refusal" || true
grep -q "test-only unconfined broker: $unconfined_asset" "$tmp_dir/unconfined-refusal" ||
    fail "the unconfined refusal does not name $unconfined_asset"

# The same name in a publication set, listed in its SHA256SUMS and in the
# asset contract, is refused at verification and so before any upload.
case_dir=$(mutation_case unconfined-public)
make_source "$case_dir/$unconfined_asset" '#!/bin/sh'
(
    cd "$case_dir"
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$unconfined_asset"
    else
        shasum -a 256 "$unconfined_asset"
    fi >> SHA256SUMS
)
expect_failure unconfined-public \
    "$repo_root/tools/verify_public_release.sh" \
    "$case_dir" "$tmp_dir/assets-with-unconfined"

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
