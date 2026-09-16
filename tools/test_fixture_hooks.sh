#!/bin/sh
# Prove every test fixture that commits in a throwaway repo is immune to
# the developer's global git hooks (bead ley-line-open-d1697b).
#
# A global `core.hooksPath` whose commit-msg hook enforces a message policy
# (rosary installs one) rejected the fixtures' `git commit -m fixture` and
# failed task ci before any Rust ran; the same hook then failed the two
# daemon watcher tests, which commit through a Rust helper. Each fixture
# below runs with `GIT_CONFIG_GLOBAL` pointing at a config whose commit-msg
# hook rejects every commit; a fixture passes only if its own git never
# consults that hook.
#
#   tools/test_fixture_hooks.sh          the shell fixtures under tools/
#   tools/test_fixture_hooks.sh --rust   the cli-lib tests that commit
#
# The two halves are wired into the ci chain separately: the shell half
# runs with the other cheap fixture tests, the Rust half after `task test`
# has built the cli-lib test targets.
set -eu

repo_root=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/leyline-fixture-hooks.XXXXXX")
trap 'rm -rf "$tmp_dir"' 0 1 2 15

hostile="$tmp_dir/hostile"
mkdir -p "$hostile/hooks"
cat > "$hostile/hooks/commit-msg" <<'EOF'
#!/bin/sh
echo "hostile commit-msg hook rejected: $(cat "$1")" >&2
exit 1
EOF
chmod +x "$hostile/hooks/commit-msg"
printf '[core]\n\thooksPath = %s\n' "$hostile/hooks" > "$hostile/gitconfig"
export GIT_CONFIG_GLOBAL="$hostile/gitconfig"

# The hostile config must actually bite, or a pass proves nothing.
probe="$tmp_dir/probe"
git init -q "$probe"
git -C "$probe" config user.name probe
git -C "$probe" config user.email probe@example.test
git -C "$probe" config commit.gpgsign false
printf 'x\n' > "$probe/f"
git -C "$probe" add f
if git -C "$probe" commit -qm probe 2>/dev/null; then
    echo "the hostile global hook did not fire; this test cannot prove anything" >&2
    exit 1
fi

if [ "${1:-}" = "--rust" ]; then
    # Every cli-lib test that commits in a throwaway repo. Keep in step
    # with `grep -rl '"commit"' rs/ll-open/cli-lib`.
    cd "$repo_root/rs"
    cargo test -q -p leyline-cli-lib --lib cmd_daemon::tests
    cargo test -q -p leyline-cli-lib --test source_blobs_dual_store_test
    cargo test -q -p leyline-cli-lib --test adr_0029_baseline_test
    echo "fixture hooks: every committing cli-lib test ignores the developer's global git hooks"
    exit 0
fi

# Every fixture under tools/ that runs `git commit`. The check below keeps
# this list in step with the scripts.
fixtures="tools/test_ci_attestation.sh tools/test_release_tags.sh"
found=$(cd "$repo_root" && grep -l 'git \(-C "[^"]*" \)\?commit' tools/test_*.sh \
    | grep -v '^tools/test_fixture_hooks\.sh$' | sort | tr '\n' ' ')
expected=$(printf '%s\n' $fixtures | sort | tr '\n' ' ')
if [ "$found" != "$expected" ]; then
    echo "fixtures that run git commit changed; update this test's list" >&2
    echo "  found:    $found" >&2
    echo "  expected: $expected" >&2
    exit 1
fi

for f in $fixtures; do
    if ! sh "$repo_root/$f" >"$tmp_dir/out" 2>&1; then
        echo "$f fails under a hostile global commit-msg hook:" >&2
        tail -n 5 "$tmp_dir/out" >&2
        exit 1
    fi
done

echo "fixture hooks: every committing fixture ignores the developer's global git hooks"
