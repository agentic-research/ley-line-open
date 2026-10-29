#!/bin/sh
# License direction — bead ley-line-open-4392a2.
#
# README.md § License states the carveout: the schema and wire-contract crates
# are Apache-2.0 so a consumer can depend on them; the implementation crates
# are AGPL-3.0-or-later. That promise holds only while no Apache-2.0 crate
# reaches an AGPL crate through its dependency graph: an Apache crate that
# links AGPL code hands every consumer the AGPL obligations the carveout exists
# to keep away from them.
#
# The rules, over `cargo metadata`'s resolved graph (cargo's answer, not a
# manifest grep, so optional deps, renames, `license.workspace` inheritance and
# target-specific deps all resolve exactly):
#
#   1. Every workspace member declares `license` as exactly `Apache-2.0` or
#      `AGPL-3.0-or-later`.
#   2. For every Apache-2.0 member, no package in its transitive closure over
#      normal and build edges declares a license expression mentioning AGPL,
#      and every package in that closure declares a license or license-file.
#
# Dev-dependency edges are excluded: they build the crate's own tests and never
# reach a consumer's dependency graph. Every feature is enabled
# (`--all-features`) and no platform filter is applied, so a dependency behind
# a feature or a target cfg still counts.
#
# cargo-deny was considered: its `licenses` allow-list is one set for the whole
# graph, and `bans.deny[].wrappers` names permitted direct parents only, so
# neither expresses "this subset of crates must not transitively reach that
# subset" without hand-maintaining every edge.
set -eu

root=${1:-.}
manifest="$root/rs/Cargo.toml"
test -f "$manifest" || {
    printf 'license direction: no workspace manifest at %s\n' "$manifest" >&2
    exit 2
}

metadata=$(cargo metadata --format-version 1 --all-features --locked \
    --manifest-path "$manifest")

findings=$(printf '%s\n' "$metadata" | jq -r '
  (.packages | map({key: .id, value: .}) | from_entries) as $pkgs
  | .workspace_members as $ws
  | (.resolve.nodes
     | map({key: .id,
            value: [.deps[]
                    | select(any(.dep_kinds[]; .kind == null or .kind == "build"))
                    | .pkg]})
     | from_entries) as $edges
  | def closure($root):
      {queue: [$root], parent: {($root): null}}
      | until(.queue | length == 0;
          .queue[0] as $n
          | .queue |= .[1:]
          | reduce ($edges[$n] // [])[] as $d (.;
              if .parent | has($d) then .
              else .parent[$d] = $n | .queue += [$d] end))
      | .parent;
    def chain($parent; $n):
      if $parent[$n] == null then [$n] else chain($parent; $parent[$n]) + [$n] end;
    def pkgname($id): "\($pkgs[$id].name)@\($pkgs[$id].version)";
    ( $ws[] as $m
      | $pkgs[$m]
      | select(.license != "Apache-2.0" and .license != "AGPL-3.0-or-later")
      | "UNDECLARED \(.name) (\(.manifest_path)): license = \(.license // "<none>"); expected Apache-2.0 or AGPL-3.0-or-later"
    ),
    ( $ws[] as $m
      | select($pkgs[$m].license == "Apache-2.0")
      | closure($m) as $parent
      | $parent | keys[] | select(. != $m) as $d
      | $pkgs[$d] as $p
      | if ($p.license == null and $p.license_file == null) then
          "UNLICENSED \($pkgs[$m].name) (Apache-2.0) reaches \(pkgname($d)), which declares no license: \([chain($parent; $d)[] | pkgname(.)] | join(" -> "))"
        elif (($p.license // "") | test("AGPL"; "i")) then
          "VIOLATION \($pkgs[$m].name) (Apache-2.0) reaches \(pkgname($d)) (\($p.license)): \([chain($parent; $d)[] | pkgname(.)] | join(" -> "))"
        else empty end
    )
')

if [ -n "$findings" ]; then
    printf '%s\n' "$findings" >&2
    printf 'license direction: the Apache-2.0/AGPL carveout (README.md § License) is violated\n' >&2
    exit 1
fi

apache=$(printf '%s\n' "$metadata" | jq -r '
  .workspace_members as $ws
  | [.packages[] | select(.id as $i | $ws | index($i)) | select(.license == "Apache-2.0") | .name]
  | sort | join(", ")')
printf 'license direction: no Apache-2.0 crate reaches AGPL code (%s)\n' "$apache"
