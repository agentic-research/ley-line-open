#!/bin/sh
# Prove every fixture that commits in a throwaway repo is immune to the
# developer's global git hooks (bead ley-line-open-d1697b).
#
# A global `core.hooksPath` whose commit-msg hook enforces a message policy
# (rosary installs one) rejected the fixtures' `git commit -m fixture` and
# failed task ci before any Rust ran. Each fixture below runs under a HOME
# whose global gitconfig points at a hook that rejects every commit; the
# fixture passes only if its own git never consults that hook.
set -eu

repo_root=$(CDPATH='' cd -- "$(dirname "$0")/.." && pwd)
tmp_dir=$(mktemp -d "${TMPDIR:-/tmp}/leyline-fixture-hooks.XXXXXX")
trap 'rm -rf "$tmp_dir"' 0 1 2 15

hostile_home="$tmp_dir/home"
mkdir -p "$hostile_home/hooks"
cat > "$hostile_home/hooks/commit-msg" <<'EOF'
#!/bin/sh
echo "hostile commit-msg hook rejected: $(cat "$1")" >&2
exit 1
EOF
chmod +x "$hostile_home/hooks/commit-msg"
printf '[core]\n\thooksPath = %s\n' "$hostile_home/hooks" > "$hostile_home/.gitconfig"

# The hostile config must actually bite, or a pass proves nothing.
probe="$tmp_dir/probe"
git init -q "$probe"
git -C "$probe" config user.name probe
git -C "$probe" config user.email probe@example.test
git -C "$probe" config commit.gpgsign false
printf 'x\n' > "$probe/f"
git -C "$probe" add f
if HOME="$hostile_home" XDG_CONFIG_HOME="$hostile_home/xdg" \
    git -C "$probe" commit -qm probe 2>/dev/null; then
    echo "the hostile global hook did not fire; this test cannot prove anything" >&2
    exit 1
fi

# Every fixture under tools/ that runs `git commit`. Keep this list in step
# with `grep -l 'git commit' tools/test_*.sh`; the check below enforces it.
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
    if ! HOME="$hostile_home" XDG_CONFIG_HOME="$hostile_home/xdg" \
        sh "$repo_root/$f" >"$tmp_dir/out" 2>&1; then
        echo "$f fails under a hostile global commit-msg hook:" >&2
        tail -n 5 "$tmp_dir/out" >&2
        exit 1
    fi
done

echo "fixture hooks: every committing fixture ignores the developer's global git hooks"
