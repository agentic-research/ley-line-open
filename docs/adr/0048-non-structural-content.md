# ADR-0048 — Non-structural content: comments, embedded regions and AST-less data

**Status:** Accepted (owner, 2026-11-11), not yet implemented. The owner's decisions on the open
questions are recorded under "Decided questions". Nothing in the Thesis or the "Decisions"
sections is shipped: the "Context" section describes what ships, and every later section
separates **Shipped** from **Proposed** where both appear. Implementation ships in v0.30.0 as one
lineage bump (`projection-v14`, `EXTRACTION_EPOCH` 8, `merkle-ast-v4`; see "Scope").
**Bead:** `ley-line-open-1b5f3c` (first implementation slice). Filed from this ADR and its
review: `ley-line-open-4cd459` (code-kind extras), `ley-line-open-4e0852` (the content-class
marker), `ley-line-open-5b8445` (non-UTF-8 leaves), `ley-line-open-5b9949` (κ not pinned).
Carried in the same bump by owner decision: `ley-line-open-60321f` (content-derived file kind and
shebang parser selection), `ley-line-open-4624d9` and `ley-line-open-651909` (Go extraction).
Consumer half: `mache-48123a`.
**Amends:** ADR-0027 and ADR-0047 D-i (the `node_hash` fold gains a classification step, a
leaf rule and a comment tag); the ADR-0047 §3 `node_hash` row; the `node_hash` row of the
`AGENTS.md` vocabulary table; `docs/TABLE_CONTRACT.md` "Comments at `projection-v13`".
**Related:** ADR-0032 (declared decompositions), ADR-0045 T1/T3 (`_tree`,
`_parse_diagnostics`), ADR-0047 D-ii (`storeRoot`) and D-iii (`node_content` columns are
functions of `node_hash`), bead `ley-line-open-aa1634` (comments as occurrences, epoch 7),
bead `ley-line-open-c822a6` (injections).

Tree cited throughout: `origin/integration` at `9d41b225` (v0.29.0 plus #568):
`PROJECTION_SCHEMA_VERSION = "projection-v13"`, `EXTRACTION_EPOCH = 7`,
`IR_SCHEMA_VERSION = "merkle-ast-v3"`. Grammar facts are read from the `grammar.json` and
`node-types.json` of the exact grammar crate versions in `rs/Cargo.lock`. Reviewed by
`identity-domain-auditor` and `cross-runtime-contract-auditor` (findings folded in below; the
questions they raised are recorded under "Decided questions").

______________________________________________________________________

## Thesis (accepted, not yet implemented)

Every file's bytes have a blob hash: plain BLAKE3 over the whole file, named by `storeRoot`
through `_tree`. On top of that, the parse projection should put each byte range of a file
into exactly one of four categories, each with one projection-side identity:

| | Category | Projection identity and its authoritative bytes | Where it is |
| --- | --- | --- | --- |
| (a) | Code structure | `node_hash` code variants (tags `0x00`, `0x01`): κ kind, terminal bytes, child `node_hash`es | `_ast` occurrence |
| (b) | Non-structural content attached to code: comments, doc comments, shebangs, comment-borne directives | `node_hash` comment variant (tag `0x02`): grammar kind and the comment's exact bytes | `_ast` occurrence with a comment address |
| (c) | Embedded foreign-language regions (injections) | The host string's bytes are (a) of the host; the region's parse is (a) of the guest language under its own root `node_hash` | Host occurrence plus injection site |
| (d) | Content with no AST: no grammar, binary, oversized, unparseable | None beyond the blob hash | `_tree.path`, byte range `[0, size)` |

The rule that holds them apart: **only (a) enters a code `node_hash` preimage.** LLO decides the
category from the node's grammar kind and publishes it as data. A consumer never infers it from
a kind's name or from a missing row.

The owner asked whether a comment needs its own address ("denote the section or address of
comment") and whether data with no AST belongs in the same account. The answer to both is yes.
A comment has a content identity (what it says) and an address (where it sits), and neither may
leak into the identity of the code around it. A file with no AST has only its blob hash and its
path; it never had a `node_hash` and must not acquire one.

## Context: what ships today

### Comments (shipped, epoch 7)

`fold_children` (`rs/ll-open/cli-lib/src/cmd_parse.rs`) walks each node's children. A child
that is not a tree-sitter `extra` is folded into the parent's `node_hash`. An extra that passes
`is_comment_extra` (`node.is_extra() && node.is_named() && node.kind().contains("comment")`)
becomes an `_ast` occurrence through `emit_comment_occurrence`. Every other extra is dropped,
with no row and no hash. A comment occurrence's content row is
`hash_leaf(hashed_kind(lang, comment), text)`: the same tag (`0x00`) and the same
`derive_key("leyline/ast-node/v3", …)` as a folded code leaf.

Five defects follow.

1. **A comment that is a grammar child is folded as code** (`ley-line-open-1b5f3c`). Swift's
   grammar uses `multiline_comment` as a class-member separator, and Scala aliases a shebang to
   a named `comment` (`_shebang: $ => alias(token(seq("#!", /.*/)), $.comment)`). Both are
   ordinary children, so a comment-only edit moves `class_body`'s `node_hash`
   (`a_swift_member_separator_comment_is_folded_not_extra` pins the current behaviour).
   Because the construction is shared, the folded Swift comment leaf and an extra Swift comment
   with the same text are one `node_content` row, reached both by a `node_edge` as a code child
   and by an `_ast` row as a comment's own root.
2. **Comment bytes reach a code `node_hash` through the leaf fallback.** `fold_children` hashes
   a node as a leaf when its list of folded children is empty (`children.is_empty()`), and a
   leaf hashes `node.utf8_text(content)`: its whole span. A node whose only children are
   comments therefore hashes the comments' text. A Python module that is only `# a\n` and one
   that is only `# b\n` have different root `node_hash`es; so do comment-only YAML, TOML and JSON
   files and a licence-only `__init__.py`.
3. **The name rule misclassifies both ways.** In `tree-sitter-sequel` 0.3.11, `keyword_comment`
   and `comment_statement` (`COMMENT ON TABLE …`) are code and match `LIKE '%comment%'`;
   `marginalia` (the `/* */` comment, in the grammar's `extras`) does not match, so SQL block
   comments have no row today.
4. **Extras that carry meaning are dropped from the fold** (`ley-line-open-4cd459`). The
   compiled grammars' `extras` include Ruby `heredoc_body` (string content), C#
   `preproc_define`, `preproc_undef`, `preproc_pragma`, `preproc_nullable`, `preproc_line`,
   `preproc_error`, `preproc_warning`, `preproc_region`, `preproc_endregion`, and PHP
   `text_interpolation` (inline HTML outside `<?php ?>`). Editing a heredoc body or a `#define`
   moves no `node_hash` and leaves no row.
5. **Non-UTF-8 text collapses** (`ley-line-open-5b8445`). Leaves and comments hash
   `utf8_text(content).unwrap_or("")`, and only a NUL byte makes a file binary. Every comment or
   leaf of one kind whose bytes are not valid UTF-8 gets the address `hash_leaf(kind, "")`, which
   violates ADR-0047 law (a); `adversarial_malformed_bytes`
   (`rs/ll-open/cli-lib/tests/source_blobs_dual_store_test.rs`) projects such a file today.

Comment-like named kinds in the compiled grammars whose name does **not** contain `comment`
(`mache-48123a` asked for this list): SQL `marginalia` (an extra); Rust, Kotlin and Groovy
`shebang`, Swift `shebang_line`, JavaScript, TypeScript and Lua `hash_bang_line`, C#
`shebang_directive` (all ordinary children). Named kinds that contain `comment` but are not
comment occurrences: SQL `keyword_comment` and `comment_statement` (code); Rust `doc_comment`,
`inner_doc_comment_marker`, `outer_doc_comment_marker` and Lua `comment_content` (children
inside a comment, never rows because a comment is terminal). Heredoc kinds (Bash, HCL, Ruby,
PHP, Containerfile) are code: they are string content. Bash, Python, Ruby, Elixir and Scala have
no shebang kind; there a `#!` line is a plain `comment`.

### Embedded regions (shipped)

Injections are compiled for two hosts: Go (`queries/go/injections.scm`, SQL in string
literals) and Markdown (block `inline` nodes reparse under the inline grammar). `fold_injected`
reparses the captured range and folds it with the same `hash_leaf`/`hash_internal`, giving the
region its own root; nothing it produces enters the host's preimages or `_ast`. Its named nodes
have no `_ast` row; their facts are `node_defs`/`node_refs` rows with `kind_id` and a span, and
injected ids take the nid ordinals after the comments. The host string literal is a code leaf of
the host, so an edit inside the SQL moves the host's `node_hash` (its text changed) and the
injected root. The injected fold walks `!is_extra()` children only, so an injected comment
extra is dropped, and an injected comment-kind child would be folded. Comments are never probed
for injections.

### Data with an AST, and data without (shipped)

JSON, YAML, TOML, Markdown, HTML, Protobuf, HCL and Containerfile are tree-sitter grammars
(`TsLanguage::from_extension`, `from_filename`): their files are category (a) like any source
file, and their comment extras (JSON, YAML, TOML, HTML, HCL) are category (b).
`rs/ll-open/ts/src/pyproject.rs` is a library projection of `pyproject.toml` into the `nodes`
directory nid space (`dirs`/`names`); `parse_into_conn` does not call it, and it writes no
`node_hash`.

A file with no grammar (`file_language` returns `None`), a binary (a NUL byte in the first
8 KiB), a file over `MAX_PARSE_FILE_SIZE`, or one whose parse failed is a `_tree` entry
`(path, mode, blob_hash)` whose bytes are in `source_blobs`. It has no `_source` row and no
`_ast`, AST `nodes`, `node_content` or fact rows. The last three also have a
`_parse_diagnostics` row with `projected = 0`; a file with no grammar has none. Its identity is
the blob hash, named by `storeRoot` through `_tree`. A consumer learns that a file has no AST
only from the absence of a `_source` row.

## Decisions (proposed)

### N1 — The four categories, with what may and may not change `node_hash`

**(a) Code structure.** Every node whose class (N2) is `code`, named or anonymous, extra or
not. Identity: the code variants of `node_hash`. Addressed by its `_ast` occurrence (`nid`, a
locator) and span. A code `node_hash` changes when and only when a code descendant's kind, bytes
or order changes. To make that true, the leaf rule changes: **a node is a leaf if and only if
tree-sitter gives it no children at all** (`child_count() == 0`). A node whose children are all
non-code is an internal node over an empty child list, `hash_internal(kind, [])`, and never
hashes its span. This closes defect 2.

**(b) Non-structural content.** Every node whose class is `comment`, `shebang` or `directive`,
whether or not the grammar marks it `extra`. It has a position in the code and no meaning to the syntax tree.
Identity: the comment variant of `node_hash` (N4) over its grammar kind and exact bytes.
Addressed by its comment address (N3). It is in no other `node_hash` preimage, so editing,
adding or removing it changes no code `node_hash`; only its own occurrence and content row
change. It is terminal: its subtree is never walked (shipped rule).

Whitespace and line continuations (Python and Containerfile `line_continuation`, Elixir's hidden
newline extras) are class `trivia`: no row, no hash, as today. Their bytes stay in the blob and
inside their ancestors' spans; nothing addresses them separately.

Comment-borne directives (Go `//go:build`, `//go:generate`, `// +build`; Python coding cookies
and `# type:`; TypeScript `// @ts-ignore`, `// @ts-expect-error`, `// @ts-nocheck`; Scala
`//> using`) are class `directive` (Q4, decided): out of every `node_hash` preimage like a
comment, and marked so a consumer can find them. `node_hash` is the identity of the syntax tree,
not of a build; a consumer that needs build semantics reads the directive occurrences or the blob
hash. A shebang changes which interpreter runs the file but not the tree, so it is class
`shebang`, non-structural by the same argument (Q1, decided).

A directive is usually not a grammar kind of its own: `//go:build` is a Go `comment`. So the
class has two carriers (N2): the kind-level class in `kinds.class`, and a per-occurrence role
that refines a `comment` occurrence to `directive` or `shebang` by a per-language rule over its
bytes. The role never changes a `node_hash`: the occurrence is outside the fold in either case,
and its content row is the same tag-`0x02` row.

**(c) Embedded regions.** The host's bytes stay (a) of the host: the string literal is a code
leaf, and editing the region moves the host `node_hash`. The region's parse is (a) of the guest
language under its own root, the same `node_hash` the same text would have as a standalone file
(`inj_own_ca_root_dedups_with_standalone_sql`). The guest tree is classified by the guest
language's inventory (N2), so a comment inside injected SQL is a guest comment: out of the guest
fold, never in the host's. Addressed by the injection site (N6).

**(d) AST-less content.** No projection identity: the blob hash over the file's exact bytes is
its only identity. Addressed by `_tree.path` and the byte range `[0, size)`. No `node_content`
row may name it, and ADR-0047 law (c) already guarantees that no blob hash equals a
`node_hash`.

**Content-derived file kind (`ley-line-open-60321f`, decided for v0.30.0).** What a file *is*
is decided from its bytes as well as its name, by an established detector rather than rules
LLO invents:
- **Detection.** A pure-Rust signature detector with a compiled-in rule set (the `infer` crate is
  the candidate; the implementation records the final choice after `cargo deny` and
  `task audit`) plus a byte-order-mark and UTF-8 validity check. A detector that reads a host
  database (libmagic, the freedesktop shared-mime-info store) is excluded, because the result
  must be a function of the bytes and the binary alone, the same on every host.
- **Key.** The result is keyed by blob hash, since it is a function of the bytes:
  `_blob_kind(blob_hash PRIMARY KEY, is_text, encoding, mime)`. Two paths with the same bytes
  share a row; the row says nothing about the path.
- **Parser selection.** Extension, then file name (shipped), then the shebang's interpreter for a
  text file with neither: an extensionless `#!/usr/bin/env python3` parses as Python, `#!/bin/sh`
  as Bash. A detected MIME kind alone never selects a grammar: a `.txt` file holding JSON gets a
  JSON MIME kind and stays unparsed unless a rule says otherwise.
- **Unknown stays opaque.** A file the detector cannot classify, a binary, and a file no rule
  selects a grammar for remain (d), with a `_content_spans` `opaque` row (N6) and a `_blob_kind`
  row where detection succeeded. The binary test moves from "a NUL byte in the first 8 KiB" to
  the detector's text/binary verdict, so a UTF-16 text file is text, not binary.
- **Versioning.** The detector's crate version, its rule set and the shebang interpreter map are
  projection rules: they decide which category a file falls in. They are pinned with the
  inventory (gate 4, N2) to `EXTRACTION_EPOCH`, so changing any of them forces re-derivation. A
  file moving from (d) to (a) gains `node_hash`es; no existing `node_hash` changes meaning.

### N2 — Classification by grammar kind, from a checked-in inventory

**Decision.** The class of a node is a function of `(lang, grammar kind, named)` alone, read
from a per-language inventory, `rs/ll-open/ts/src/content_classes.json`, compiled into
`leyline-ts` and exposed as `content_class(lang, kind, named)`. `is_extra()` decides nothing.
Classes at v14: `code`, `comment`, `shebang`, `directive`, `trivia`. The set is discovered, not
hand-enumerated (Q1, decided): the owner expects more classes than these, and the mechanism for
finding them is the generated per-grammar inventory and its gates. A grammar kind that fits no
existing class appears as an `unclassified` row and fails gate 2; a new class enters the set only
through such a failure, with a projection bump for the CHECK. The key includes `named` because tree-sitter
kind strings collide between named and anonymous nodes (HTML has both a named and an anonymous
`doctype`; C# has an anonymous `#pragma`), and an anonymous token must never leave the fold
because it shares a name with a comment kind.

**Why the kind and not `is_extra`.** `is_extra` describes where a grammar lets a node appear,
not what it means. The Swift separator is a comment that is not an extra; Ruby's `heredoc_body`
is code that is an extra. A grammar kind is the finest unit whose meaning the grammar fixes, and
it is what `kinds` interns, so the class can be published per `kind_id` (N6).

**Derivation.** `task gen:content-classes` runs `cargo metadata` and reads each locked grammar
crate's `grammar.json` (`extras`) and `node-types.json` (named kinds), from a checked-in map of
`(crate, subdirectory)` to `TsLanguage::name()`. The map is needed because several crates hold
more than one grammar or none at the root (`tree-sitter-php`: `php/src`, `php_only/src`;
`tree-sitter-typescript`: `typescript/src`, `tsx/src`; `tree-sitter-md`: block and inline) and
crate names differ from language names (`tree-sitter-kotlin-ng`, `tree-sitter-sequel`). It writes
one row for each kind that is an extra or matches
`comment|shebang|hash_bang|marginalia|doc|directive|pragma|heredoc|preproc`, recording
`is_extra` for audit. A new candidate row is written as `unclassified`.

**Gates.**
1. `task gen:content-classes:check` regenerates into a temporary file and diffs, so a grammar
   bump that adds, renames or drops a candidate kind fails until the inventory is regenerated.
2. `content_classes_has_no_unclassified_rows` fails while any row is `unclassified`; a person
   assigns the class by reading the grammar.
3. `every_inventory_kind_exists_in_its_compiled_grammar` enumerates each compiled
   `tree_sitter::Language` (`node_kind_count`, `node_kind_for_id`, `node_kind_is_named`) and
   fails if a row names a `(kind, named)` pair, or a language, that the compiled grammar lacks.
4. `content_classes_are_pinned_to_the_ir_lineage` pins
   `(IR_SCHEMA_VERSION, EXTRACTION_EPOCH, BLAKE3(content_classes.json))`, and the occurrence-role
   rules and the file-kind rules (N1 (d)) beside it, modelled on
   `compiled_queries_are_pinned_to_the_extraction_epoch`. Any edit to the inventory moves which
   nodes enter the fold, so it must move both labels, and through them `parserId`. Adding the
   inventory digest as a new `parserId` input kind was considered and not taken: a new input kind
   is a `leyline/parser-id/v2` scheme change (`store_root.rs`), while the pin reaches the same
   `parserId` movement through inputs `parserId` already folds. The κ map has the same gap today
   and gets the same pin (`ley-line-open-5b9949`).

**Fail direction.** A node whose `(lang, kind, named)` has no row is `code`. Misclassing a
comment as code makes a `node_hash` move when it need not; misclassing code as a comment makes a
`node_hash` stay put when the code changed, the failure an identity must not have.

**Initial inventory** (from the locked grammars; the generator is authoritative):
- `comment`: `comment` in every grammar that has it; `line_comment`, `block_comment` (Rust,
  Java, Kotlin, Groovy; Scala has `block_comment`); `html_comment` (JavaScript, TypeScript);
  `multiline_comment` (Swift, extra or not); `js_comment` (CSS); `marginalia` (SQL).
- `shebang`: Rust, Kotlin, Groovy `shebang`; Swift `shebang_line`; JavaScript, TypeScript, Lua
  `hash_bang_line`; C# `shebang_directive`. A grammar with no shebang kind (Bash, Python, Ruby,
  Elixir, and Scala, which aliases its shebang to `comment`) yields a `comment` occurrence, which
  the occurrence rule below refines to `shebang`.
- `directive`: C# `preproc_region` and `preproc_endregion` (Q2, decided).
- `trivia`: Python and Containerfile `line_continuation`.
- `code`, listed so that the generator's name match does not catch them: SQL `keyword_comment`,
  `comment_statement`; Ruby `heredoc_body`; C# `preproc_*` other than the region markers; PHP
  `text_interpolation` pending Q3.

**Occurrence roles.** A `comment` occurrence is refined by a per-language rule over its bytes,
stored in `_comment_roles(nid PRIMARY KEY, role CHECK (role IN ('shebang','directive')),
rule TEXT NOT NULL)`, where `rule` names the rule that matched. Initial rules: any language, a
`comment` at byte 0 that starts with `#!` is `shebang`; Go `//go:` and `// +build` at the start
of a line comment; Python a coding cookie (PEP 263's regular expression, on line 1 or 2) and
`# type:`; TypeScript and JavaScript `// @ts-ignore`, `// @ts-expect-error`, `// @ts-nocheck`,
`/// <reference`; Scala a `comment` holding a `using_directive`. Each rule cites the language's
own documentation in the rule table. A role is a function of the occurrence's language, kind and
bytes, so it never moves a `node_hash`; the rule table is pinned with the inventory (gate 4) to
`EXTRACTION_EPOCH`, so a rule change re-derives the roles. The effective class of an occurrence
is its role if it has one, else `kinds.class`; `v_comment_address` and `v_content_class` expose
the effective class.

**`kinds.class` lifecycle.** `kinds` rows are interned with `INSERT OR IGNORE`, never deleted,
and survive epoch re-derivation, so a class written once could outlive the inventory that wrote
it. Three rules close this:
- `intern_kind` (`rs/ll-core/schema/src/lib.rs`) takes the class as a parameter. `leyline-schema`
  cannot depend on `leyline-ts`, so every caller passes `content_class(...)`: the parse producer,
  the library projection `rs/ll-open/ts/src/project.rs` (which interns every named child, extras
  included, and feeds the mache contract fixture `rs/ll-open/ts/tests/mache_compat.rs`), and the
  LSP and daemon test callers.
- `class` is immutable after interning. `_meta.content_classes` stamps the inventory digest; an
  arena whose stamp differs from the binary's is refused at open and rebuilt cold, as a
  projection-version mismatch is.
- The CHECK admits all five classes, because `project.rs` may intern a trivia kind; the parse
  producer never writes a `trivia` row (`no_parse_written_kind_is_trivia`).

### N3 — The comment address

**Shipped.** A comment occurrence has `nodes.parent_nid` (the node tree-sitter attached it to),
`nodes.ord` (dense source order among code and comment siblings) and `_ast`'s byte, row and
column span. Its nid ordinal follows every code occurrence, so a comment edit moves no code
`nid`.

**Proposed.** A comment's address is the tuple

```text
(parent_nid, gap, rank, start_byte, end_byte)
```

- `parent_nid`: the occurrence it sits under. For a comment that is a grammar child (Swift's
  separator) this is the grammar parent, `class_body`.
- `gap`: the number of named `code` siblings before it under that parent. `0` is the leading
  position, before the first code sibling; `n` is after the last. Anonymous tokens have no rows
  and are not counted.
- `rank`: its index, from 0, among the non-code siblings (`comment`, `shebang`, `directive`)
  in the same gap, in source order.
- `start_byte`, `end_byte`: tree-sitter's span in the file's bytes.

**The address is factual (Q6, decided).** It records where tree-sitter attached the comment, and
LLO does not reinterpret it: no re-parenting, no "this comment belongs to that function". Which
declaration a comment documents is a separate, derived question, answered by N8.

The prose above is the contract. It is published as a view, because `gap` and `rank` are
functions of `nodes.ord` and the effective class; the shipped form uses window functions, so the
cost is one pass per parent:

```sql
CREATE VIEW v_comment_address AS
WITH sib AS (
    SELECT n.nid, n.parent_nid, n.ord, COALESCE(r.role, k.class) AS class,
           a.start_byte, a.end_byte,
           SUM(k.class = 'code') OVER (PARTITION BY n.parent_nid ORDER BY n.ord
                                       ROWS UNBOUNDED PRECEDING) AS gap
      FROM nodes n
      JOIN _ast a ON a.nid = n.nid
      JOIN kinds k ON k.kind_id = a.kind_id
      LEFT JOIN _comment_roles r ON r.nid = n.nid
)
SELECT nid, parent_nid, class, gap,
       ROW_NUMBER() OVER (PARTITION BY parent_nid, gap ORDER BY ord) - 1 AS rank,
       start_byte, end_byte
  FROM sib
 WHERE class IN ('comment', 'shebang', 'directive');
```

A predicate on `nid` cannot be pushed through the window functions, so a consumer filters the
view by a file's nid range (`nid BETWEEN (file_id << 24) AND (file_id << 24) | 0xFFFFFF`) or by
`parent_nid`. If the measured cost on a large file is not acceptable, the fallback is to store
`gap` and `rank` as parse-time columns with the same contract.

**Round trip.** With `b` the file's bytes from `source_blobs[_tree.blob_hash]`, for every
comment occurrence, `comment_hash(raw_kind, b[start_byte..end_byte])` (N4) equals the row's
`node_hash`, and when that slice is valid UTF-8 it also equals `node_content.token`. This holds
for non-UTF-8 comments only because N4 hashes bytes, not decoded text (defect 5).

**Stability under edits elsewhere.** Each component is stable under a stated class of edits,
**provided the edit leaves tree-sitter's attachment of the comment unchanged**. Tree-sitter
attaches an extra according to parser state, so an edit after a comment can re-parent it
(likely case: a trailing comment at the end of an indented Python block, when the dedented
statement after it is deleted). Under that proviso:
- The content identity is location-free: an edit that does not touch the comment's own bytes
  leaves it unchanged.
- `gap` and `rank` change only when a code sibling is added or removed before the comment under
  the same parent, or a comment is added or removed before it in the same gap. An edit inside a
  code sibling (its body, its comments) changes neither.
- `parent_nid` is a code nid, a pre-order ordinal among code occurrences. It is unchanged by any
  comment edit anywhere in the file (shipped) and changes only when a code occurrence is added or
  removed before the parent in pre-order. It is a locator and never leaves the arena.
- The byte span shifts by the length change of any earlier edit and is recomputed exactly on
  every parse. It is the round-trip coordinate, not a stable key.

Outside one arena, a comment is cited by `(storeRoot, parserId, path, start_byte, end_byte)`.
`storeRoot` and the path name the exact bytes; `parserId` is needed because "these bytes are a
comment" is a fact of the derivation, not of the bytes. A structural citation that survives
edits before the parent is later work (S6).

### N4 — A separate hash domain for comment content, and the leaf rule

**Decision.** Comment and shebang content rows use a third node tag in the merkle-ast
construction:

```text
NODE_TAG_COMMENT = 0x02
comment_hash(kind, bytes) = derive_key("leyline/ast-node/v3",
    0x02 ‖ uvarint(len(kind)) ‖ kind ‖ uvarint(len(bytes)) ‖ bytes)
```

`kind` is the raw grammar kind (comments have no κ entry, and κ must never move a comment's
address). `bytes` is the comment's exact byte slice, opener and closer included. Code leaves
hash their byte slice the same way (`ley-line-open-5b8445`), so for valid UTF-8 nothing moves and
for invalid UTF-8 two different slices no longer share an address. The content row has
`node_tag = 2`, `kind = raw_kind`, `token` = the slice when it is valid UTF-8 and NULL otherwise,
and `arity = 0`; every column is still a function of `node_hash` (ADR-0047 D-iii). The language
is not in the preimage: identical bytes under the same grammar kind in two languages are one
row. Go's `// x` and C's `// x` (both `comment`) share a row; Rust's `// x` is a `line_comment`
and does not.

**Why a code row and a comment row can never be one row.** Code leaves start their preimage with
`0x00`, internal nodes with `0x01`, comments with `0x02`, all under one keyed context. Equal
addresses mean equal preimages (barring a BLAKE3 collision), and preimages with different first
bytes are unequal. This is the separation that already keeps a leaf apart from an internal node,
and the tag-byte-inside-one-context pattern ADR-0047 §1.1 allows. `node_content.node_tag`
publishes the variant per row. The producer is not the only guard: a trigger refuses a
`node_edge` whose child row has `node_tag = 2`, so no writer can make a comment a code child, and
`AstNodeWitness` gains a `Comment` variant whose `verify` recomputes tag-2 rows.

**Why not a new `derive_key` context and a separate table.** A comment is an `_ast` occurrence
like a code node, and `_ast.content_id` has one foreign-key target, `node_content`. A separate
context in the same column would put two constructions in one `node_hash` column; a separate
table would need a second, nullable foreign key on `_ast`. One construction with a third tag
keeps one column and one join, and the known-answer vectors cover it.

**The context string is a construction name, frozen.** It stays `leyline/ast-node/v3` and is
decoupled from the lineage label `IR_SCHEMA_VERSION`, which moves to `merkle-ast-v4` (N5). Code
preimages that no reclassification touches are byte-for-byte unchanged, so those subtrees keep
their `node_hash`. Consequence: unlike the v2-to-v3 move (ADR-0047 D-i), a v3 address and a v4
address are equal exactly when their preimages are equal, so equality across the two lineages
is never a false match. What does not carry across is presence or absence: under v4 some v3
addresses are never produced (comment rows, Swift and Scala nodes holding comment children),
and a consumer must not conclude "hash X is gone, so that code was removed" across a lineage
change. The `ir_schema_version` label is what tells it so. Renaming the context for consistency
would move every address and is not done.

`node_hash` is then one construction with two kinds of value: syntax subtrees (tags `0x00`,
`0x01`) and comments (tag `0x02`). The `AGENTS.md` vocabulary row, the ADR-0047 §3 row, the
sigma v1 README's `astNode` row and `identity_docs_test.rs` are amended to say so.

### N5 — Versioning

One release carries three bumps, each on its own axis:

| Axis | From | To | Why |
| --- | --- | --- | --- |
| `IR_SCHEMA_VERSION` (`node_hash` lineage) | `merkle-ast-v3` | `merkle-ast-v4` | The preimage's meaning changes: comment rows move to tag `0x02`; nodes holding a comment child, or holding only comments, fold differently (N1 leaf rule); shebang files; non-UTF-8 leaves; and with S2 Ruby heredocs and C# directives. |
| `EXTRACTION_EPOCH` | `7` | `8` | It records which nodes become `_ast` occurrences, and that set now comes from the inventory. Forces re-derivation of every file. |
| `PROJECTION_SCHEMA_VERSION` | `projection-v13` | `projection-v14` | `kinds.class` (`TEXT NOT NULL CHECK (class IN ('code','comment','shebang','directive','trivia'))`), `_comment_roles`, `v_comment_address`, `node_tag = 2` rows, the `node_edge` trigger, `_meta.content_classes`, `_content_spans`, `v_content_class`, `_blob_kind` and `_documents` (N8). |

**Refused.** A `projection-v13` arena is refused at parse open and rebuilt cold, like every
earlier version; the refusal also gets a shape probe (a `kinds` table without `class` is
refused even when no label is stamped). An arena whose `_meta.content_classes` differs from the
binary's is refused (N2). A consumer written against v13 refuses v14.

**`class` is a closed enum per projection version.** A consumer refuses a value it does not
know. Adding a value, or reclassifying a kind, is never silent: the gate-4 pin forces the IR and
epoch bumps, and a new value also changes the CHECK, which is a projection bump.

**What else moves.** `Head.parserId` moves (it folds the IR and projection versions and the
extraction epoch). `Head.rootHash`'s construction is unchanged, but its value moves for any run
whose files gain or reorder occurrences, because it folds the bytes of `ast.capnp`, which holds
every occurrence record in ordinal order (SQL `marginalia` gains records; Swift and Scala comments
move to comment ordinals). `Controller.current_root` moves with any change to the arena's bytes.
`storeRoot` and every blob hash keep their values: neither sees the parser.

**Artifacts that move with S1.** `compatibility.json` (`ir_schema_version`, regenerated by
`task compat:gen`); `rs/ll-core/schema-spec/sigma/v1/test-vectors/constructions.json` (tag-2
and non-UTF-8 `astNode` cases and its `$comment`) and the sigma v1 README's `astNode` row;
`clients/go/leyline-schema/sigma/sigma.go` (an `AstNodeComment = 0x02` tag and its doc comment);
the `irSchemaVersion` comment in `daemon.capnp`; the `version.rs` history entries;
`rs/ll-open/cli-lib/tests/identity_docs_test.rs` and the files it pins to `projection-v13`
(`AGENTS.md`, `.claude/skills/leyline-review-kit/SKILL.md`); `version_handshake_blackbox_test.rs`
(`merkle-ast-v3`); `projection_v9_to_v12_arenas_are_refused_at_open` extended with v13;
`docs/TABLE_CONTRACT.md` (the comments section, the `_meta.extraction_epoch` and `node_content`
`node_tag` rows); a `changes/ley-line-open-1b5f3c.md` fragment with `Section: Breaking`.
`projection_schema_version` and `extraction_epoch` are in neither `compatibility.json` nor the
daemon's version response today; mache reads them from `_meta`.

### N6 — One published marker for (b), (c) and (d)

mache asked (`mache-48123a`) for an LLO-owned marker instead of name matching, covering comments,
embedded regions and AST-less data under one rule. The rule: **LLO publishes the class of every
non-code byte range a consumer can read, and a consumer never infers it.** The class is stored
where the fact that determines it lives, so that the carrier cannot disagree with its cause:

- **Comment, shebang, code:** determined by `(lang, kind, named)`, so `kinds.class`. Every
  `_ast`, `node_defs` and `node_refs` row already carries `kind_id`, host or injected.
- **Injected region:** determined by an injection query matching one host occurrence, not by any
  kind (a Go string literal may or may not be SQL), so it cannot be a `kinds` property.
- **AST-less file:** determined by the file (its extension, its bytes, its size), so it cannot be
  a `kinds` property, and it must not be a `_tree` column, because `storeRoot` folds `_tree` and
  must stay parser-free.

The last two share one table (S3, `ley-line-open-4e0852`):

```sql
CREATE TABLE _content_spans (
    path       TEXT    NOT NULL,          -- the _tree path
    start_byte INTEGER NOT NULL,
    end_byte   INTEGER NOT NULL,
    class      TEXT    NOT NULL CHECK (class IN ('injected', 'opaque')),
    lang       TEXT,                      -- injected: the guest language
    reason     TEXT CHECK (reason IN ('no_grammar', 'binary', 'oversized', 'parse_failed')),
    host_nid   INTEGER,                   -- injected: the host occurrence (a locator)
    content_id INTEGER REFERENCES node_content(content_id),  -- injected: the guest root
    PRIMARY KEY (path, start_byte, end_byte, class),
    CHECK ((class = 'opaque') = (reason IS NOT NULL)),
    CHECK ((class = 'injected') = (lang IS NOT NULL AND host_nid IS NOT NULL
                                   AND content_id IS NOT NULL))
);

CREATE VIEW v_content_class AS
SELECT s.path, c.start_byte, c.end_byte, c.class, k.lang, c.nid
  FROM v_comment_address c
  JOIN _ast a ON a.nid = c.nid
  JOIN kinds k ON k.kind_id = a.kind_id
  JOIN _source s ON s.file_id = (c.nid >> 24)
UNION ALL
SELECT path, start_byte, end_byte, class, lang, host_nid
  FROM _content_spans;
```

`v_content_class.class` takes the values `comment`, `shebang`, `directive`, `injected` and
`opaque`; code is every `_ast` row not listed. An `opaque` row spans `[0, size)`, and every
`_parse_diagnostics` row with `projected = 0` has exactly one, as does every `_tree` file no rule
selected a grammar for. Rows are written and deleted with the file's other projection rows
(`delete_file_rows`), in the same transaction, on removal, rename and scoped reparse. An opaque
row is a function of the bytes, the path and the projection's rules: the compiled grammar set
(folded into `parserId`), and the extension and file-name maps, the file-kind detector and
shebang map (N1 (d)) and `MAX_PARSE_FILE_SIZE`, which `parserId` does not fold directly; they
are pinned to `EXTRACTION_EPOCH` (gate 4), which it does. A file excluded by a parse's language
filter is not AST-less and has no row (Q5, decided).

### N7 — Consumer effects (mache, `mache-48123a`)

- **Fold by kind.** Replace `raw_kind LIKE '%comment%'` with the effective class from
  `v_comment_address` (or `kinds.class` for kind-level classes): `comment`, `shebang` and
  `directive` fold. The LIKE matches SQL `keyword_comment` and `comment_statement` (code) and
  misses `marginalia` and every shebang kind.
- **`dead_code`.** Only `class = 'code'` occurrences are candidates. A comment has no
  `node_defs` row, so today's results hold; the class makes the rule explicit.
- **Doc-comment lookup.** Read `_documents` (N8) for the languages it covers, and drop mache's
  own rule for each language as `_documents` covers it. For other languages, the TABLE_CONTRACT
  recipe still walks back from a def over siblings whose effective class is `comment` (shebangs
  and directives are excluded by class, not by a byte test). Swift member-separator comments
  become attachable.
- **Edges.** No `node_edge` reaches a comment row in any language (today a Swift separator
  comment is reachable as a code child). Rows with `node_tag = 2` are comments.
- **Unknown class values** are refused, never read as non-structural.
- **DDL pin.** mache's byte-identical DDL pin, `internal/fixturedb/schema_leyline.go`, re-pins to
  v14 (`kinds.class`, the views, the trigger); it is mache's independent check of this ABI.
- **Smell baseline.** `(rule_id, node_hash)` keys re-pin under `merkle-ast-v4`. Deltas are
  confined to comment rows, comment-only nodes, Swift, Scala, shebang files, non-UTF-8 leaves and
  (with S2) Ruby heredocs and C# directives; mache enumerates them rather than re-baselining
  blind.
- **Version.** mache accepts `projection-v14` with `_meta.extraction_epoch = 8` and
  `_meta.ir_schema_version = merkle-ast-v4`, and refuses v13.

### N8 — `_documents`: which declaration a comment documents (derived, separately versioned)

Decided by the owner as the second layer of Q6. N3's address says where a comment *is*; this
layer says what it *documents*, and it is a derivation over the occurrence rows, not part of
any identity.

```sql
CREATE TABLE _documents (
    doc_nid  INTEGER NOT NULL,   -- the comment occurrence, or the Python docstring's string occurrence
    decl_nid INTEGER NOT NULL,   -- the declaration it documents (a code occurrence)
    rule     TEXT    NOT NULL,   -- the rule that produced the row, e.g. 'go/line-run-above'
    PRIMARY KEY (doc_nid, decl_nid)
);
```

- **Only unambiguous conventions.** v1 rules:
  - Go: the run of `//` comments directly above a declaration, with no blank line inside the run
    or between it and the declaration (`go/line-run-above`).
  - Rust: `///` and `/** */` outer doc comments to the next item, skipping attributes
    (`rust/outer-doc`); `//!` and `/*! */` inner doc comments to the enclosing item or the module
    (`rust/inner-doc`).
  - JavaScript and TypeScript: a `/** */` block directly above a declaration, or above its
    `export` wrapper (`js/jsdoc-above`).
  - Python: the first statement of a module, class or function body, when it is a string
    expression (`python/docstring`); the documenting occurrence is that string, which stays code.
- **An ambiguous case gets no row, never a guess.** A comment that is directive or shebang
  class, a comment separated by a blank line, a comment above two declarations on one line,
  or a language with no rule here has no row.
- **Each row names its rule**, so a consumer can trust or ignore a rule independently.
- **Versioned on its own axis.** `_meta.documents_version` (`documents-v1` at v14). A rule fix or
  a new language bumps it and rebuilds `_documents` from `_ast`, `nodes`, `kinds`,
  `_comment_roles` and the source bytes, with no re-parse. It never moves `IR_SCHEMA_VERSION`,
  `EXTRACTION_EPOCH` or any `node_hash`: no row of `_documents` is in a preimage. Adding the
  table is part of `projection-v14`; later rule changes are not projection bumps, because the
  table's shape does not change.
- **mache** drops its own doc-comment rule for each language once `_documents` covers it (N7).

## Test plan

Each invariant has a closing test. Unless named, tests go in
`rs/ll-open/cli-lib/tests/comment_occurrences_test.rs`.

| Invariant | Test |
| --- | --- |
| A comment-only edit moves no code `node_hash` where the comment is a grammar child | `a_swift_member_separator_comment_edit_moves_no_code_hash` (replaces `a_swift_member_separator_comment_is_folded_not_extra`); `a_scala_shebang_edit_moves_no_code_hash` |
| A node holding only comments does not hash them (leaf rule) | `a_comment_only_file_edit_moves_no_code_hash`: Python `# a` against `# b`, a YAML comment-only file, a shebang-only Scala file |
| Every inventory comment and shebang kind emits an addressed occurrence | `every_compiled_comment_kind_emits_an_addressed_occurrence`: one fixture per `(lang, kind)` row, asserting a `v_comment_address` row |
| Classification ignores `is_extra` both ways | `sql_comment_statement_is_code_and_marginalia_is_a_comment`; with S2, `a_ruby_heredoc_body_edit_moves_the_hash`, `a_csharp_define_edit_moves_the_hash` |
| The address round-trips, UTF-8 or not | `a_comment_address_round_trips_to_its_bytes` over a multi-language corpus; `a_non_utf8_comment_round_trips` (two invalid-UTF-8 comments get different addresses) |
| The address matches an independent oracle | `comment_addresses_match_the_checked_in_fixture`: hand-written `(path, gap, rank, start, end)` rows for Go, Swift, Scala and SQL files, compared byte for byte with the view |
| The window view equals the definition | `v_comment_address_equals_its_correlated_definition` (differential test against the prose definition written as correlated subqueries); a timing bound on one large single-file fixture |
| The address survives edits elsewhere, and its proviso is real | `a_comment_address_survives_edits_outside_its_gap`; `a_trailing_python_comment_reparents_when_the_next_statement_is_deleted` (records tree-sitter's behaviour) |
| Code and comment rows never share | `no_content_row_is_both_a_code_child_and_a_comment`; `a_node_edge_to_a_comment_row_is_refused` (inserts one, expects the trigger); tag-2 known-answer cases in `sigma_axioms.rs` and in the Go vectors |
| The inventory stays true and versioned | `task gen:content-classes:check`; `content_classes_has_no_unclassified_rows`; `every_inventory_kind_exists_in_its_compiled_grammar` (`rs/ll-open/ts`); `content_classes_are_pinned_to_the_ir_lineage` |
| An unknown kind is code | `an_uninventoried_kind_is_folded` |
| `kinds.class` matches the binary | `every_producer_interns_the_inventory_class` (parse and `project.rs`); `an_arena_with_a_stale_content_class_stamp_is_refused` |
| Directives and shebangs are marked, outside the fold | `a_go_build_line_is_a_directive_and_moves_no_code_hash`; `a_python_coding_cookie_and_ts_ignore_are_directives`; `a_python_shebang_comment_has_the_shebang_role`; `a_csharp_region_marker_is_a_directive` |
| Content-derived file kind (60321f) | an extensionless `#!/usr/bin/env python3` file parses as Python and `#!/bin/sh` as Bash; a `.txt` holding JSON gets a JSON MIME kind and stays unparsed; a PNG and a UTF-16 text file are classified correctly; an unknown binary stays opaque; two paths with the same bytes share one `_blob_kind` row |
| `_documents` (N8) | one fixture per rule (Go run above, Rust `///` past an attribute and `//!`, JSDoc above an `export`, Python docstring), each asserting the `rule` value; `an_ambiguous_comment_documents_nothing` (blank line, two declarations on one line, a directive above a function); `a_documents_rule_change_moves_no_node_hash` |
| Versioning | `projection_schema_version_is_stamped_and_pinned` (v14); the refusal test extended to v13 and the `kinds`-without-`class` shape; `extraction_epoch` 8; `ir_schema_version` `merkle-ast-v4`; `parserId` moves; `Head.rootHash` differs for a Swift separator fixture |
| (c) Injected comments | `an_injected_sql_comment_is_outside_both_folds` |
| (d) and the marker (S3) | `every_unprojected_file_has_one_opaque_span` (binary, unknown extension, oversized, parse failure; again after a removal and a scoped reparse); `an_injection_site_has_one_injected_span`; `v_content_class_lists_comments_and_no_code` |

Gates for each slice: `task ci`, `task rs:mutants:pr`, `task compat:check`,
`task schema:go-sigma-vectors` (the Go verifier is an independent oracle: it fails on an
unknown tag), `git diff --check`. `task schema:consumer-fixture-test` compiles the Go daemon wire
API only and is not evidence for this SQL change; the independent SQL oracles are the checked-in
address fixture and mache's DDL pin.

## Scope

Decided by the owner: everything below marked v0.30.0 ships as **one** lineage bump
(`projection-v14`, `EXTRACTION_EPOCH` 8, `merkle-ast-v4`), so consumers re-pin once.

| Slice | Content | Bead | Release |
| --- | --- | --- | --- |
| S1 | N2 inventory, generator, four gates and `kinds.class` lifecycle; N1 leaf rule; N4 comment tag, byte hashing, trigger, witness variant; reclassify Swift `multiline_comment`, Scala shebang `comment`, SQL `marginalia`, the shebang kinds; `_comment_roles` (shebang and directive rules); `v_comment_address`; N5 bumps and every artifact listed there | `ley-line-open-1b5f3c`, `ley-line-open-5b8445` | v0.30.0 |
| S2 | Code-classed extras folded: Ruby `heredoc_body`, C# `preproc_*` other than the region markers (which are `directive`). They fold into whichever node tree-sitter attached them to, not into the construct that uses them; an ancestor's `node_hash` moves, a call's subtree need not | `ley-line-open-4cd459` | v0.30.0 |
| S3 | `_content_spans` (opaque and injected), `v_content_class` | `ley-line-open-4e0852` | v0.30.0 |
| S3b | Pin κ to the IR lineage | `ley-line-open-5b9949` | v0.30.0 if it fits; otherwise the next lineage bump |
| S3c | Content-derived file kind (`_blob_kind`) and shebang parser selection (N1 (d)) | `ley-line-open-60321f` | v0.30.0 |
| S3d | Go extraction: composite-literal type refs; package-level `const`/`var` defs and qualified selector argument refs. Fact rows only, no `node_hash` effect; they ride `EXTRACTION_EPOCH` 8 | `ley-line-open-4624d9`, `ley-line-open-651909` | v0.30.0 |
| S3e | `_documents` v1 (N8): Go, Rust, JSDoc, Python | to file before implementation | v0.30.0 |
| S4 | Comment occurrences for injected trees (they have no `_ast` rows today) | to file | later |
| S5 | PHP `text_interpolation` as an HTML injection; Markdown injection into doc comments | to file | later |
| S6 | A portable structural comment locator; more `_documents` languages | to file | later |

S1 estimate: the classification lookup replaces `is_comment_extra` in `fold_children` and
`fold_injected_node`; the leaf rule in both; `hash_comment`, byte hashing and the tag constant;
the trigger and witness variant; the inventory module, generator and four gates; the occurrence
role rules; the `intern_kind` signature and its callers; the `kinds` DDL column, stamp and view;
three version constants, the refusal probe and the tests that pin them; the Go sigma tag and
vectors; the documentation rows. Roughly 1,000 to 1,400 changed lines with tests. The whole
v0.30.0 bump (S1 to S3e) is several agents in separate worktrees with disjoint file scopes,
merged into one integration branch before one release.

## Decided questions (owner, 2026-11-11)

- **Q1 — DECIDED: `shebang` is its own class.** The owner expects more classes than the ones
  named here ("there is more than just this but it might be a discovery problem"). So the class
  set is discovered, not enumerated: the generated per-grammar inventory and its gate
  (no `unclassified` rows) are the mechanism, and a new class appears through a gate failure
  that a person resolves by reading the grammar (N2). A shebang in a grammar with no shebang kind
  gets the `shebang` role by the byte-0 `#!` rule.
- **Q2 — DECIDED: C# `#region` / `#endregion` are class `directive`.** They are editor directives
  with no compile meaning. Today they are dropped extras, so no `node_hash` sees them; as
  `directive` they stay out of the fold and gain a marked row. No identity changes relative to
  today.
- **Q3 — PENDING (flagged to the coordinator, not decided here).** PHP `text_interpolation`
  (inline HTML outside `<?php ?>`) is dropped today, so an inline-HTML edit moves no `node_hash`.
  The default this ADR recommends is N2's fail direction: fold it as code until S5 makes it an
  HTML injection. That default changes identity (PHP `node_hash`es move for files with inline
  HTML), so per the owner's instruction it is flagged rather than taken. The alternative with no
  identity change is to leave it out of the fold and mark it with a `_content_spans` row
  (`class = 'injected'`, `lang = 'html'`) once S5 exists, accepting the missed change meanwhile.
- **Q4 — DECIDED: comment-borne build directives are class `directive`.** `//go:build`,
  `//go:generate`, coding cookies, `@ts-ignore` and the like are out of `node_hash` and marked
  through `_comment_roles` (N1 (b), N2). A build-sensitive identity is not part of this ADR.
- **Q5 — DECIDED: no row for a file a language filter excluded.** The filter is a property of the
  parse invocation, not of the file; a filtered file is not AST-less. No identity change.
- **Q6 — DECIDED: two layers.** (1) The address is factual: parent, gap, rank and byte span as
  tree-sitter attaches the comment; LLO does not reinterpret it (N3). (2) A separate, versioned,
  derived table, `_documents`, maps a comment to the declaration it documents only for languages
  with an unambiguous convention (first: Go `//` directly above, Rust `///` and `//!`, JSDoc
  `/** */`, Python docstrings). Each row carries the rule that produced it; an ambiguous case gets
  no row. Rule fixes version the table (`documents-vN`), never the identity domain (N8). mache
  drops its own rules per language as `_documents` covers them.
- **Q7 — DECIDED: `kinds.class` is immutable after interning, and a stale
  `_meta.content_classes` stamp refuses the arena.** A reclassification changes `node_hash` across
  the arena, so the arena is re-derived in full anyway; an upsert would only hide the mismatch.
  No identity change beyond the bump that carries the reclassification.

## Non-goals

- Changing a construction other than `node_hash`. `storeRoot` and the blob hash keep their
  constructions and values; `Head.rootHash` and `current_root` keep their constructions, and their
  values move as N5 says.
- A semantic or build identity. `node_hash` remains the identity of the syntax tree.
- Parsing AST-less data. A new grammar for a data format moves its files from (d) to (a) through
  the ordinary grammar-addition path, which already moves `parserId`.
