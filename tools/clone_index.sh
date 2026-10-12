#!/bin/sh
# Exact structural clone index — tier 1 of bead ley-line-open-6e9f79.
#
# Parses a source tree into a scratch arena and groups every function by its
# `_ast.node_hash`. node_hash is a Merkle hash over a node's kind, its token
# text and its ordered children, so two functions share one only when they are
# the same tree token for token: a Type-1 clone. A renamed copy hashes
# differently and is not reported here (that is tier 2).
#
# A copy nested inside another reported copy in the same file is the same
# duplication counted twice; a group whose every copy is nested that way is
# dropped. Groups are ranked by lines x copies.
#
# Usage: tools/clone_index.sh [--min-lines N] [--kind K]... [--db DB] [--out FILE] [SOURCE_DIR]
#   SOURCE_DIR   tree to parse (default: rs)
#   --db DB      reuse an existing arena instead of parsing SOURCE_DIR
#   --kind K     tree-sitter kind to group (default: function_item)
#   --out FILE   write the markdown report to FILE instead of stdout
# LEYLINE names the binary (default: leyline on PATH).
set -eu

min_lines=6
kinds=""
db=""
out=""
leyline=${LEYLINE:-leyline}

while [ $# -gt 0 ]; do
  case "$1" in
    --min-lines) min_lines=$2; shift 2 ;;
    --kind) kinds="$kinds${kinds:+,}'$2'"; shift 2 ;;
    --db) db=$2; shift 2 ;;
    --out) out=$2; shift 2 ;;
    -*) printf 'clone_index: unknown option %s\n' "$1" >&2; exit 2 ;;
    *) break ;;
  esac
done
src=${1:-rs}
[ -n "$kinds" ] || kinds="'function_item'"

case "$min_lines" in
  ''|*[!0-9]*) printf 'clone_index: --min-lines must be a number\n' >&2; exit 2 ;;
esac

work=$(mktemp -d "${TMPDIR:-/tmp}/leyline-clone-index.XXXXXX")
trap 'rm -rf "$work"' 0 1 2 15

if [ -z "$db" ]; then
  db="$work/arena.db"
  "$leyline" parse "$src" -o "$db" >"$work/parse.log" 2>&1 || {
    cat "$work/parse.log" >&2
    printf 'clone_index: parse of %s failed\n' "$src" >&2
    exit 1
  }
fi

# One row per reported copy: group rank, lines, copies, file, 1-based line.
sqlite3 -separator '	' "$db" >"$work/rows.tsv" <<SQL
WITH fn AS (
  SELECT a.nid, a.node_hash AS h, a.start_byte AS sb, a.end_byte AS eb,
         a.start_row AS sr, a.end_row - a.start_row + 1 AS lines, s.id AS file
  FROM _ast a
  JOIN kinds k ON k.kind_id = a.kind_id
  JOIN _source s ON s.file_id = (a.nid >> 24)
  WHERE k.raw_kind IN ($kinds)
    AND a.node_hash IS NOT NULL
    AND a.end_row - a.start_row + 1 >= $min_lines
),
dup AS (
  SELECT * FROM fn WHERE h IN (SELECT h FROM fn GROUP BY h HAVING count(*) >= 2)
),
nested AS (
  SELECT d.nid FROM dup d
  WHERE EXISTS (SELECT 1 FROM dup o
                WHERE o.file = d.file AND o.nid <> d.nid
                  AND o.sb <= d.sb AND o.eb >= d.eb)
),
grp AS (
  SELECT h, max(lines) AS lines, count(*) AS copies
  FROM dup
  GROUP BY h
  HAVING sum(nid NOT IN (SELECT nid FROM nested)) > 0
),
ranked AS (
  SELECT h, lines, copies,
         row_number() OVER (ORDER BY lines * copies DESC, lines DESC, h) AS rnk
  FROM grp
)
SELECT r.rnk, r.lines, r.copies, d.file, d.sr + 1
FROM ranked r JOIN dup d ON d.h = r.h
ORDER BY r.rnk, d.file, d.sr;
SQL

report() {
  groups=$(cut -f1 "$work/rows.tsv" | sort -u | wc -l | tr -d ' ')
  total=$(awk -F '	' '!seen[$1]++ { n += $2 * ($3 - 1) } END { print n + 0 }' "$work/rows.tsv")
  printf '# Exact structural clones in %s\n\n' "$src"
  printf '%s groups of %s+ lines; %s duplicated lines beyond the first copy of each.\n' \
    "$groups" "$min_lines" "$total"
  printf 'Grouped by `_ast.node_hash`, ranked by lines x copies.\n'
  prev=""
  while IFS='	' read -r rnk lines copies file line; do
    if [ "$rnk" != "$prev" ]; then
      sig=$(sed -n "${line}p" "$src/$file" 2>/dev/null | sed 's/^[[:space:]]*//; s/[[:space:]]*{[[:space:]]*$//')
      printf '\n## %s. `%s` (%s lines x %s copies)\n\n' "$rnk" "$sig" "$lines" "$copies"
      prev=$rnk
    fi
    printf -- '- %s/%s:%s\n' "$src" "$file" "$line"
  done <"$work/rows.tsv"
}

if [ -n "$out" ]; then
  report >"$out"
else
  report
fi
