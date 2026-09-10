#!/bin/sh
# Every cfg-gated module the mutation gate can reach must be either COMPILED or
# EXCLUDED — never the third thing (bead `ley-line-open-b23c41`).
#
# cargo-mutants parses with `syn` and does not evaluate `cfg`, so it enumerates
# mutants inside `#[cfg(feature = "x")] pub mod y;` whether or not `x` is on.
# When the feature is OFF the mutated code never compiles: the mutant "builds"
# in 0s, every test trivially passes, and it is reported MISSED. That is a
# phantom survivor on healthy code, and in a log it is indistinguishable from a
# real missing assertion. Twenty-eight of them sat unread in witchcraft.rs for
# five weeks; six more appeared in cli-lib's `daemon/embed.rs` on the
# projection-v5 PR.
#
# Either move is honest:
#
#   ENABLE   the feature, when tests exist that could kill the mutants.
#   EXCLUDE  the file, when they do not — with a bead owning the coverage gap,
#            so exclusion records a debt instead of hiding one.
#
# What must not survive is the silent third state. This guard makes the choice
# mandatory rather than remembered.
#
# SCOPE, stated plainly: this checks packages whose mutants_diff.sh slice passes
# `--no-default-features`, because that is the only place a DEFAULT feature can
# be silently compiled out. Non-default features gating covered code are the
# feature ledger's job (tools/feature-ledger.txt, `mutants=enable` rows) and
# claim 1 of check_feature_reachability.sh. The two together cover the class;
# neither covers it alone, which is precisely how `fuse`/`nfs`/`verified` fell
# between them — the ledger correctly exempts default features, and nothing
# noticed that one slice had opted out of defaults.
#
# This guard is not decoration. Enumerating the fs exclusion list BY HAND while
# writing the fix missed `verified.rs`.
set -eu

repo_root=$(CDPATH='' cd -P -- "$(dirname "$0")/.." && pwd -P)
script="$repo_root/tools/mutants_diff.sh"
status=0

# Slices that opt out of default features. Written as a grep over the gate
# rather than a second hand-maintained list: a new `--no-default-features`
# slice is exactly the event that needs checking, so discovering it here is the
# point. `run_slice` invocations continue across backslash-newlines, so fold
# them first.
slices=$(tr '\n' ' ' < "$script" \
    | sed 's/\\ / /g' \
    | tr ';' '\n' \
    | grep -- '--no-default-features' \
    | grep -o -- '--package [A-Za-z0-9_-]*' \
    | awk '{ print $2 }' \
    | sort -u)

if [ -z "$slices" ]; then
    echo "check_mutants_cfg_coverage: no --no-default-features slice found."
    echo "  Nothing to check. If mutants_diff.sh still has one, this parse broke."
    exit 0
fi

for pkg in $slices; do
    # Map cargo package name -> workspace directory via its manifest.
    manifest=$(grep -rl "^name = \"$pkg\"" "$repo_root"/rs/*/*/Cargo.toml 2>/dev/null | head -1)
    if [ -z "$manifest" ]; then
        echo "check_mutants_cfg_coverage: cannot locate manifest for '$pkg'" >&2
        status=1
        continue
    fi
    pkg_dir=$(dirname "$manifest")
    rel=$(printf '%s\n' "$pkg_dir" | sed "s|^$repo_root/rs/||")

    # The slice's own invocation: its enabled features and its exclusions.
    invocation=$(tr '\n' ' ' < "$script" | sed 's/\\ / /g' | tr ';' '\n' \
        | grep -- "--package $pkg" | grep -- '--no-default-features' | head -1)
    enabled=$(printf '%s\n' "$invocation" \
        | grep -o -- '--features [A-Za-z0-9_,-]*' | awk '{ print $2 }' | tr ',' ' ')

    # Every `#[cfg(feature = "F")] pub mod M;` pair in the crate root.
    lib="$pkg_dir/src/lib.rs"
    [ -f "$lib" ] || continue
    gated=$(awk '
        /^#\[cfg\(feature = "/ {
            match($0, /"[^"]+"/)
            feat = substr($0, RSTART + 1, RLENGTH - 2)
            next_is_mod = 1
            next
        }
        next_is_mod && /^pub mod / {
            mod = $3
            sub(";", "", mod)
            print feat, mod
            next_is_mod = 0
            next
        }
        { next_is_mod = 0 }
    ' "$lib")

    # Read from a file, NOT from a pipe. `while read` on the right of a pipe
    # runs in a SUBSHELL: an `exit` inside it ends only the loop, and a
    # `status=1` inside it is discarded when the subshell dies. The first
    # version of this guard did exactly that and was a silent no-op — it
    # reported success with an exclusion deliberately removed, which is the one
    # thing a gate must never do.
    gated_file=$(mktemp "${TMPDIR:-/tmp}/mutants-cfg.XXXXXX")
    printf '%s\n' "$gated" > "$gated_file"
    while read -r feat mod; do
        [ -n "$feat" ] || continue
        covered=0
        # Enabled by the slice? Then it compiles, and its mutants are real.
        for on in $enabled; do
            if [ "$on" = "$feat" ]; then covered=1; break; fi
        done
        # Otherwise it must be excluded, or its mutants are phantoms.
        if [ "$covered" -eq 0 ] \
            && printf '%s\n' "$invocation" | grep -q -- "--exclude '$rel/src/$mod.rs'"; then
            covered=1
        fi
        [ "$covered" -eq 1 ] && continue
        {
            echo "PHANTOM MUTANTS: $rel/src/$mod.rs is gated on feature '$feat',"
            echo "  which the '$pkg' mutation slice does not enable, and the slice"
            echo "  does not exclude the file. cargo-mutants will enumerate its"
            echo "  mutants, compile them out, and report them MISSED — survivors"
            echo "  that tested nothing."
            echo
            echo "  Fix by choosing, in tools/mutants_diff.sh:"
            echo "    ENABLE   add '$feat' to that slice's --features, if tests exist"
            echo "             that can kill mutants in this module;"
            echo "    EXCLUDE  add --exclude '$rel/src/$mod.rs', if they do not —"
            echo "             and cite the bead that owns the missing coverage."
        } >&2
        status=1
    done < "$gated_file"
    rm -f "$gated_file"
done

if [ "$status" -eq 0 ]; then
    echo "mutants cfg coverage: every gated module is compiled or excluded"
fi
exit "$status"
