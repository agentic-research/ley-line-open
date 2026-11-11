# ADR-0048 — Non-structural content: comments, embedded regions and AST-less data

**Status:** Accepted (owner, 2026-11-11), not yet implemented. The owner's decisions on the open
questions are recorded under "Decided questions". Nothing in the Thesis or the "Decisions"
sections is shipped: the "Context" section describes what ships, and every later section
separates **Shipped** from **Proposed** where both appear. Implementation ships in v0.30.0 as one
lineage bump (`projection-v14`, `EXTRACTION_EPOCH` 8, `merkle-ast-v4`; see "Scope").
**Bead:** `ley-line-open-1b5f3c` (first implementation slice). Filed from this ADR and its
review: `ley-line-open-4cd459` (code-kind extras), `ley-line-open-4e0852` (the content-class
marker), `ley-line-open-5b8445` (non-UTF-8 leaves), `ley-line-open-5b9949` (κ not pinned),
`ley-line-open-8bcfa7` (bytes in hidden tokens), `ley-line-open-81c06c` (`_documents`).
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
6. **Bytes held in hidden tokens are in no preimage** (`ley-line-open-8bcfa7`). The fold hashes
   an internal node over its visible children only. tree-sitter-md 0.5.3 builds `inline`,
   `code_fence_content` and `html_block` from `_line`, whose `_word` token is hidden, while the
   punctuation tokens beside it are visible. So `Calls foo.` and `Calls bar.` both hash as
   `hash_internal("inline", [leaf(".")])`: two different paragraphs, one `node_hash`. That
   violates ADR-0047 law (a) for Markdown files today.

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

**Hidden-token kinds hash their span and their children (closes defect 6).** `child_count()`
counts visible children only, so the leaf rule alone does not reach bytes held in hidden tokens.
The inventory (N2) gets a second attribute per code kind, its hash shape, published as
`kinds.hash_shape` (`TEXT NOT NULL CHECK (hash_shape IN ('fold','span'))`): `fold` (the default,
as today) or `span`. A `span` node is hashed under a fourth tag over both its full byte span and
its folded children:

```text
NODE_TAG_SPAN = 0x03
span_hash(kind, bytes, children) = derive_key("leyline/ast-node/v3",
    0x03 ‖ uvarint(len(kind)) ‖ kind ‖ uvarint(len(bytes)) ‖ bytes
         ‖ uvarint(child_count) ‖ child_hash[0..n])
```

Every byte inside the node is in its preimage, and so are its children. **Its visible children
keep their `node_edge` rows and stay edge-reachable**: because the child hashes are in the
preimage, the edges are a function of the parent's `node_hash`, as for any internal node
(ADR-0047 D-iii). A leaf over the span alone was rejected for this reason: its edges would not be
a function of its address, and two occurrences with equal bytes but different parses would share
one `node_content` row with conflicting edges. The `node_content` row has `node_tag = 3`, `token`
= the span when it is valid UTF-8 (else NULL), and `arity` = the child count. The generator
proposes `span` for a kind whose grammar rule reaches a hidden terminal that is not trivia, and
gate 2 fails until a person confirms each one. The set is per `(lang, kind, named)`, and the
generator's output is authoritative. Known members at the locked tree-sitter-md 0.5.3 are as
follows. In `markdown` (block): `inline`, `code_fence_content`, `html_block`, `info_string`,
`language`, `pipe_table_cell`, `link_label`, `link_title` and `link_destination`. In
`markdown-inline` (the guest grammar): its `inline` root and every inline kind that holds `_word`
text beside visible punctuation, among them the link and emphasis kinds. Byte sensitivity has its
own closing test,
`every_non_trivia_byte_is_in_some_node_hash_preimage`, over every compiled grammar's fixtures,
with pairs that differ only in a fence info string, a table cell, a link destination, and an
inline-guest paragraph.

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

**(c) Embedded regions.** Regions nest: PHP holds inline HTML, which holds `<script>` JavaScript
and `<style>` CSS, which can hold a template string of SQL. The regions of one file form a tree
(N9). At every level the host's bytes stay (a) of the host: the string literal, the HTML
`raw_text` or the PHP inline HTML (Q3) is code of the host, so editing the region moves the host
`node_hash`. The region's parse is (a) of the guest language under its own root, the same
`node_hash` the same text would have as a standalone file
(`inj_own_ca_root_dedups_with_standalone_sql`). The guest tree is classified by the guest
language's inventory (N2), so a comment inside injected SQL is a guest comment: out of the guest
fold, never in the host's. Addressed by its region row (N9).

**A guest root does not fold into its host's `node_hash`; the host commits to the region's
bytes instead.** A JavaScript edit inside HTML inside PHP moves the PHP file's `node_hash`, at
every depth, because each host folds the region's bytes as one of its own code leaves. Folding
the guest *root* as well would add no sensitivity to any byte, and it would make the host's
identity a function of the guest grammar, the guest's κ map and the injection query. Then every
new injection, every injection-query fix and every guest grammar bump would move host
`node_hash`es and need a lineage bump; a malformed guest would change its host's identity; and a
region the depth limit refuses would hash differently from one it admits. The shipped rule
("host structural identity is independent of the injected grammar's version", `fold_injected`)
already takes this side. The invariant it rests on, stated as a test (N9): every non-trivia byte
of a region lies inside its host's preimage. For a code host that is a code leaf or a
`span` node of the host's fold; for a comment host (a doc comment holding Markdown, a later
S5 case) it is the comment's own tag-`0x02` preimage (N4). Editing a comment-hosted region
therefore moves the comment's content row and no code `node_hash`, as any comment edit does. A
host grammar that would drop a region's bytes must classify them first: as code, the way Q3
does for PHP's `text_interpolation` (today a dropped extra), or as `span` for hidden tokens
(Markdown `inline`, today defect 6).

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
  `text_interpolation` (Q3, decided).

**Occurrence roles.** A `comment` occurrence is refined by a per-language rule over its bytes,
stored in `_comment_roles(nid PRIMARY KEY, role CHECK (role IN ('shebang','directive')),
rule TEXT NOT NULL)`, where `rule` names the rule that matched. Initial rules: any language, a
`comment` at byte 0 that starts with `#!` is `shebang`; Go, exactly `go/ast`'s `isDirective`
(`//line `, `//extern `, `//export `, or `//` followed by `[a-z0-9]+:[a-z0-9]`, which covers
`//go:build`, `//go:embed`, `//go:noinline` and `//nolint:errcheck`), as rule `go/is-directive`,
plus the legacy build constraint `// +build` as rule `go/plus-build`; Python a coding cookie (PEP 263's regular expression, on line 1 or 2) and
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

**Per-row class reads.** A consumer that needs only an occurrence's class, not its `gap` or
`rank`, may read it without the view:

```sql
SELECT a.nid, COALESCE(r.role, k.class) AS class
  FROM _ast a
  JOIN kinds k ON k.kind_id = a.kind_id
  LEFT JOIN _comment_roles r ON r.nid = a.nid;
```

This is part of the contract: for every non-code occurrence it gives the same class as
`v_comment_address.class`, and for every other `_ast` row it gives `code` or no non-code value.
`per_row_class_equals_v_comment_address_class` pins the equivalence on a multi-language corpus.

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

**Decision.** The content row of every non-code occurrence uses a third node tag in the
merkle-ast construction: every `comment`, `shebang` and `directive` occurrence, whether the class
comes from `kinds.class` (for example the C# region markers, kind-level `directive`) or from an
occurrence role:

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
addresses are never produced (comment rows, and every code shape in N5's moving-shape table),
and a consumer must not conclude "hash X is gone, so that code was removed" across a lineage
change. The `ir_schema_version` label is what tells it so. Renaming the context for consistency
would move every address and is not done.

`node_hash` is then one construction with two kinds of value: syntax subtrees (tags `0x00`,
`0x01`, and `0x03` for `span` nodes) and non-code occurrences (tag `0x02`). The `AGENTS.md` vocabulary row, the ADR-0047 §3 row, the
sigma v1 README's `astNode` row and `identity_docs_test.rs` are amended to say so.

### N5 — Versioning

One release carries three bumps, each on its own axis:

| Axis | From | To | Why |
| --- | --- | --- | --- |
| `IR_SCHEMA_VERSION` (`node_hash` lineage) | `merkle-ast-v3` | `merkle-ast-v4` | The preimage's meaning changes: non-code rows move to tag `0x02`, `span` nodes use tag `0x03`, and the code shapes in the moving-shape table below fold differently. |
| `EXTRACTION_EPOCH` | `7` | `8` | It records which nodes become `_ast` occurrences, and that set now comes from the inventory. Forces re-derivation of every file. |
| `PROJECTION_SCHEMA_VERSION` | `projection-v13` | `projection-v14` | `kinds.class` (`TEXT NOT NULL CHECK (class IN ('code','comment','shebang','directive','trivia'))`), `_comment_roles`, `v_comment_address`, `node_tag = 2` rows, the `node_edge` trigger, `_meta.content_classes`, `_content_spans`, `_regions` and `_region_ranges` (N9), `node_defs.region_id` and `node_refs.region_id`, `kinds.hash_shape`, `v_content_class`, `_blob_kind`, `_documents` (N8), and `_meta.derived_from` format `v3` (N8). |

**Refused.** A `projection-v13` arena is refused at parse open and rebuilt cold, like every
earlier version; the refusal also gets a shape probe (a `kinds` table without `class` is
refused even when no label is stamped). An arena whose `_meta.content_classes` differs from the
binary's is refused (N2).

**Consumer obligation.** A consumer must refuse an arena whose projection version it was not
written for; LLO cannot enforce this on the reader's side. A consumer written against v13 must
refuse v14, and a v14 consumer must refuse v13. The recommended check is both of:
`SELECT value FROM _meta WHERE key = 'projection_schema_version'` equals the version the
consumer was written for, and the shape probe
`SELECT 1 FROM pragma_table_info('kinds') WHERE name = 'class'` returns a row (v14) or no row
(v13). The label catches a stamped arena and the probe catches an unstamped one.

**The moving-shape table: code `node_hash`es that move from v3 to v4.** This table is a
contract. mache carries its `(rule, node_hash)` smell baseline across the bump with it, and N4,
N5 and N7 cite it rather than restate it. Each row is a predicate on v4 rows. A code occurrence's
`node_hash` moves from v3 to v4 if and only if the occurrence, or one of its descendants,
satisfies a row:

| # | Predicate on a v4 row (the occurrence or a descendant) | Derived from | Bead |
| --- | --- | --- | --- |
| 1 | a `node_content` row with `node_tag = 1` and `arity = 0`: an internal node over an empty child list, because every child is non-code or a dropped extra; this includes a comment-only file's root | the N1 leaf rule | 1b5f3c |
| 2 | a non-code occurrence (effective class `comment`, `shebang` or `directive`) whose inventory row has `is_extra = false` | inventory: `is_extra = false` and class not `code` (Swift `multiline_comment` as a separator, Scala's shebang `comment`, every shebang kind) | 1b5f3c |
| 3 | a code leaf row (`node_tag = 0`) with `token IS NULL`; a non-code row with a NULL token is in no code preimage and moves nothing | non-UTF-8 bytes (byte hashing) | 5b8445 |
| 4 | a code occurrence, or a code token, whose inventory row has `is_extra = true` and class `code` | inventory: `is_extra = true` and class `code` (Ruby `heredoc_body`, C# `preproc_*` except the region markers, PHP `text_interpolation`) | 4cd459 |
| 5 | a row with `node_tag = 3` | inventory: `hash_shape = 'span'` (the Markdown kinds listed in N1) | 8bcfa7 |
| 6 | κ: none. `ley-line-open-5b9949` ships in v0.30.0 as a pin test only, with the κ map unchanged, so no row moves for κ; the release notes say "no κ change" | — | 5b9949 |

**The rows are derived, not enumerated.** Rows 2, 4 and 5 are computed from the inventory: the
`(lang, kind, named)` rows where v3's treatment (`is_extra` decides: folded if false, dropped or
a comment occurrence if true) differs from v4's (class and hash shape decide). The generator
writes that derived set to `rs/ll-open/ts/src/content_classes_moving.json`, and
`moving_shape_table_equals_inventory_derivation` asserts that it equals the kinds the table
names. So a new inventory row that changes treatment changes the derived set and fails the test
until the table is updated. Rows 1 and 3 are structural and not tied to any kind; their
predicates are on v4 rows directly.

**Not in the table, by design.** Nested regions (a guest root never folds into its host, §(c)),
so no host moves for injection reasons. Non-code rows themselves (they are tag-`0x02` rows, not
code). SQL `marginalia` and the C# region markers: they were dropped extras and become non-code
occurrences, so no code preimage changes. Go extraction (4624d9, 651909), which changes fact rows
only. Files the file-kind detector (60321f) newly projects gain `node_hash`es, and a file it newly
classes as binary loses them; neither moves an existing value.

**The golden test, `v3_to_v4_moving_shapes_are_exactly_the_published_set`.** The corpus has every
derived kind once, plus every grammar's existing fixtures. The v0.29.0 binary hashes it into a
checked-in golden file. A v3 code occurrence is matched to a v4 code occurrence by
`(path, start_byte, end_byte, raw_kind)`; comment ordinals differ between the two lineages, so
nids are never used to match. Then two assertions hold. Every matched pair whose `node_hash`
differs has the v4 occurrence, or a descendant, satisfying a row. And every matched pair where
that holds does differ. An occurrence that exists in only one lineage, such as a Swift separator
comment that was code at v3 and is non-code at v4, must itself satisfy row 2 or row 4.

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

Each gets its own table (S3, `ley-line-open-4e0852`), because their shapes differ: a region
has a parent, a depth and a guest root, and an opaque file has a reason. Injected regions are the
region tree `_regions` (N9). AST-less files are `_content_spans`:

```sql
CREATE TABLE _content_spans (
    path       TEXT    NOT NULL PRIMARY KEY,   -- the _tree path
    start_byte INTEGER NOT NULL,               -- always 0
    end_byte   INTEGER NOT NULL,               -- the file's size
    class      TEXT    NOT NULL CHECK (class = 'opaque'),
    reason     TEXT    NOT NULL CHECK (reason IN ('no_grammar', 'binary', 'oversized', 'parse_failed'))
);

CREATE VIEW v_content_class AS
SELECT s.path, c.start_byte, c.end_byte, c.class, k.lang,
       c.nid AS nid, NULL AS host_nid, NULL AS region_id
  FROM v_comment_address c
  JOIN _ast a ON a.nid = c.nid
  JOIN kinds k ON k.kind_id = a.kind_id
  JOIN _source s ON s.file_id = (c.nid >> 24)
UNION ALL
SELECT path, start_byte, end_byte, 'injected', lang, NULL, host_nid, region_id
  FROM _regions
UNION ALL
SELECT path, start_byte, end_byte, class, NULL, NULL, NULL, NULL
  FROM _content_spans;
```

`v_content_class.class` takes the values `comment`, `shebang`, `directive`, `injected` and
`opaque`; code is every `_ast` row not listed. An `injected` range is not non-code: its bytes are
code of the host (§(c)), and it is listed so that a consumer can find the guest, never so that it
folds the range out. `nid` is the row's own occurrence (comments,
shebangs, directives), and `host_nid` is the occurrence hosting a region (NULL for a region
deeper than 1 whose host has no occurrence). A region row gives its hull; its ranges are in
`_region_ranges`. A region's bytes are listed twice on purpose: they are code of the host
(§(c)) and the input of the guest. An `opaque` row spans `[0, size)`, and every
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
- **Edges.** No `node_edge` reaches a non-code row in any language (today a Swift separator
  comment is reachable as a code child). Rows with `node_tag = 2` are non-code. The visible
  children of a `span` node (`node_tag = 3`, `kinds.hash_shape = 'span'`) **are**
  edge-reachable: each has a `node_edge` row from its parent, as under any internal node (N1).
  So a field derivation over `node_edge` finds Markdown `block_continuation` under `inline`.
- **Injected regions are host code.** The bytes of an `injected` range are code of their host
  (§(c)). They are never folded out the way comment, shebang or directive bytes are; a consumer
  that folds non-code must not fold a region's span.
- **Unknown class values** are refused, never read as non-structural.
- **DDL pin.** mache's byte-identical DDL pin, `internal/fixturedb/schema_leyline.go`, re-pins to
  v14 (`kinds.class`, the views, the trigger); it is mache's independent check of this ABI.
- **Smell baseline.** `(rule_id, node_hash)` keys re-pin under `merkle-ast-v4`. The deltas are
  exactly N5's moving-shape table, rows 1 to 5; mache enumerates them by its predicates rather
  than re-baselining blind.
- **PHP inline HTML.** It becomes code (Q3; moving-shape row 4): an inline-HTML-only edit now
  moves the PHP file's `node_hash`, where at v13 it moved none.
- **Version.** mache accepts `projection-v14` with `_meta.extraction_epoch = 8` and
  `_meta.ir_schema_version = merkle-ast-v4`, and refuses v13.

### N8 — `_documents`: which declaration a comment documents (derived, separately versioned)

Decided by the owner as the second layer of Q6. N3's address says where a comment *is*; this
layer says what it *documents*, and it is a derivation over the occurrence rows, not part of
any identity.

```sql
CREATE TABLE _documents (
    doc_nid  INTEGER NOT NULL,   -- one comment occurrence of the run, or the Python docstring's string occurrence
    decl_nid INTEGER NOT NULL,   -- the declaration it documents (see "Which node is decl_nid")
    rule     TEXT    NOT NULL,   -- the rule that produced the row, e.g. 'go/line-run-above'
    PRIMARY KEY (doc_nid, decl_nid)
);
CREATE INDEX idx_documents_decl ON _documents(decl_nid);
```

There is **one row per comment occurrence in a documenting run**: a three-line Go doc comment
gives three rows with the same `decl_nid`, and `nodes.ord` orders them. A Python docstring gives
one row.

- **Only unambiguous conventions.** v1 rules, with the declarations each covers:
  - **Go** (`go/line-run-above`): the comment group directly above a declaration, with no blank
    line inside it or between it and the declaration, as `go/ast` builds a `Doc` group. `//` and
    `/* */` comments both count. A directive (`go/ast`'s `isDirective`, the same predicate as the
    `go/is-directive` role) inside or at the end of the group gets no `_documents` row and does
    **not** break the group: `// doc` + `//go:embed x` + `var` documents the `var` with
    `// doc`, and a `//go:noinline` under a doc comment leaves the doc attached. Covers
    `function_declaration`, `method_declaration`, and, at top level and inside groups,
    `type_spec`, `const_spec` and `var_spec`. A group `type (…)`, `const (…)` or `var (…)`: a
    comment above the keyword documents the group's declaration node (`type_declaration`,
    `const_declaration`, `var_declaration`), and a comment above a spec inside the parentheses
    documents that spec. Below the top level: struct `field_declaration` and interface method
    specs, as `go/ast` attaches `Doc` to fields.
  - **Rust** (`rust/outer-doc`): `///` and `/** */` to the next item, skipping attribute items.
    Covers every item at module level and inside `impl` and `trait` blocks (`function_item`,
    `function_signature_item`, `struct_item`, `enum_item`, `union_item`, `trait_item`,
    `type_item`, `const_item`, `static_item`, `mod_item`, `macro_definition`, `impl_item`),
    struct fields (`field_declaration`) and enum variants (`enum_variant`).
    (`rust/inner-doc`): `//!` and `/*! */` document the item that encloses them: the `mod_item`
    whose body holds them, or, at the top of a file, the file's root occurrence
    (`source_file`, ordinal 0).
  - **JavaScript and TypeScript** (`js/jsdoc-above`): a `/** */` block directly above a
    declaration, above its `export` wrapper, or above its decorators. Covers
    `function_declaration`, `generator_function_declaration`, `class_declaration`,
    `method_definition`, field definitions, `variable_declarator` (for
    `export const f = () => …` and `const x = …`), and TypeScript `interface_declaration`,
    `type_alias_declaration` and `enum_declaration`.
  - **Python** (`python/docstring`): the first statement of a module, class or function body
    (methods included), when it is a string expression; the documenting occurrence is that
    string, which stays code. Attribute docstrings (a string after an assignment) are a tool
    convention, not a language one, and get no row.
- **Which node is `decl_nid`.** Always the inner declaration, the node that carries the
  declaration's `node_defs` row, never a wrapper: for `export function f` the
  `function_declaration`; for `export default function f` the `function_declaration`; for
  `export const f = () => …` the `variable_declarator`; for a decorated class or method the
  `class_declaration` or `method_definition`. This is the node mache scopes on. The one
  exception is a declaration with no `node_defs` row of its own: a Go group's declaration node,
  the `source_file` root for a file-level `//!`, and a module's root for a module docstring.
- **An ambiguous case gets no row, never a guess.** A directive or shebang occurrence, a comment
  separated by a blank line, a comment above two declarations on one line, or a language with no
  rule here has no row.
- **Each row names its rule**, so a consumer can trust or ignore a rule independently.
- **Versioned on its own axis.** `_meta.documents_version` (`documents-v1` at v14). A rule fix or
  a new language bumps it and rebuilds `_documents` from `_ast`, `nodes`, `kinds`,
  `_comment_roles` and the source bytes, with no re-parse. It never moves `IR_SCHEMA_VERSION`,
  `EXTRACTION_EPOCH` or any `node_hash`: no row of `_documents` is in a preimage. Adding the
  table is part of `projection-v14`; later rule changes are not projection bumps, because the
  table's shape does not change.
- **Reaching consumers' reuse keys.** `documents_version` goes into `_meta.derived_from`, which
  becomes `v3 <storeRoot hex> <parserId hex> <projection_schema_version> <documents_version>`. The
  leading `v3` is the format discriminator, and a `v2` value is refused, as `v1` is today.
  `parserId` does not fold it: `parserId` stays the derivation of every projection row except
  `_documents`, whose derivation is the pair `(parserId, documents_version)`. This keeps a
  documents-rule fix from moving `Head.parserId`, and with it every restriction-cache key that
  binds `parserId`. A consumer that reuses `_documents` results keys on `derived_from` (or on that
  pair), never on `parserId` alone.
- **Scoped reparse.** A scoped reparse deletes a file's `_documents` rows with its other
  projection rows (`delete_file_rows`) and re-derives them in the same transaction. Every rule is
  confined to one file, so no other file's rows change.
- **mache** drops its own doc-comment rule for each language once `_documents` covers it (N7).

### N9 — Nested embedded regions: the region tree

**Shipped: one level, two hosts.** Injections are probed only in the host fold
(`fold_children` calls `injection_engine(language).sites(child, …)`); `fold_injected_node`, which
folds a guest tree, never probes, so a guest region is never a host. The compiled queries are Go
→ SQL (statement-shaped string literals) and Markdown block → Markdown inline. HTML, PHP,
JavaScript and CSS have no `injections.scm`: an HTML `<script>` body is a `raw_text` code leaf of
the HTML tree and is not reparsed. A guest region has no table of its own today; its named nodes
appear only through `node_defs`/`node_refs` rows (`kind_id` and span), and its root
`node_content` row has no occurrence pointing at it. An unloadable grammar, a rejected range or a
failed reparse yields zero facts and no error.

**Proposed: regions form a tree, recorded in `_regions`.** A region is a byte range of a file
that some host's injection query handed to a guest grammar. Its parent is the region whose guest
tree contains the matching host node, or the file itself:

```sql
CREATE TABLE _regions (
    region_id        INTEGER PRIMARY KEY,                 -- arena-local locator, never cited outside
    path             TEXT    NOT NULL,                    -- the _tree path
    file_id          INTEGER NOT NULL,                    -- the host file (_source.file_id; nid >> 24)
    parent_region_id INTEGER REFERENCES _regions(region_id),  -- NULL: the file is the parent
    depth            INTEGER NOT NULL CHECK (depth >= 1), -- 1: directly inside the file
    lang             TEXT    NOT NULL,                    -- the guest language
    start_byte       INTEGER NOT NULL,                    -- in the file: the hull of the region's ranges
    end_byte         INTEGER NOT NULL,
    rel_start        INTEGER NOT NULL,                    -- relative to the parent region's start_byte
    rel_end          INTEGER NOT NULL,                    --   (equal to start/end_byte at depth 1)
    host_nid         INTEGER,                             -- the host node's nid, when it has one
    content_id       INTEGER REFERENCES node_content(content_id),  -- the guest root; NULL unless parsed
    status           TEXT    NOT NULL CHECK (status IN
                       ('parsed', 'parsed_with_errors', 'opaque',
                        'refused_depth', 'refused_count', 'refused_repeat')),
    message          TEXT,                                -- set for every status but 'parsed'
    UNIQUE (path, start_byte, end_byte, lang, depth),
    CHECK ((status IN ('parsed', 'parsed_with_errors')) = (content_id IS NOT NULL)),
    CHECK ((parent_region_id IS NULL) = (depth = 1))
);

-- One row per byte range the guest parser was given, in order. A single-range region has one
-- row; a combined injection (upstream `injection.combined`: PHP `text` -> HTML, a JavaScript
-- template string with substitutions -> SQL) has one per range.
CREATE TABLE _region_ranges (
    region_id  INTEGER NOT NULL REFERENCES _regions(region_id),
    ord        INTEGER NOT NULL,
    start_byte INTEGER NOT NULL,                          -- in the file
    end_byte   INTEGER NOT NULL,
    PRIMARY KEY (region_id, ord)
);

CREATE INDEX idx_regions_host ON _regions(host_nid);
CREATE INDEX idx_regions_file ON _regions(file_id);
```

A separate table, not more columns on `_content_spans`: a region needs a self-referencing parent
key, a depth, two coordinate frames, a guest root and possibly several ranges, and an opaque file
has none of them. Keeping each table's columns all meaningful lets the CHECKs say exactly what
each row is. `_region_ranges` ships in v14, although the shipped engine only makes single-range
sites, so that combined injections later need no projection change. Rows are written and deleted
with the file's other projection rows (`delete_file_rows`).

- **Identity.** The region tree carries no new construction. A region's guest root is a
  `node_hash` (the code variants, of the guest language), which never folds into its host's
  (§(c)). The host commits to the region's bytes; the guest root commits to their parse. Adding,
  fixing or nesting an injection changes `_regions` rows and the guest roots, never a host
  `node_hash`, so it moves `injection_epoch` (already a `parserId` input), not
  `IR_SCHEMA_VERSION`.
- **Bounds.** `MAX_REGION_DEPTH = 8`, `MAX_REGIONS_PER_FILE = 4096`, and an injected-ordinal
  budget per file that keeps host and comment ordinals inside the 24-bit nid space. All three
  become inputs of `current_injection_epoch`; today it folds only the queries and grammar
  fingerprints, so this is code S3 writes. A site past the depth limit gets a row with status
  `refused_depth` and a message naming the limit (`region depth 9 exceeds MAX_REGION_DEPTH 8`),
  and is not parsed. A site past the region count or the ordinal budget gets `refused_count` in
  the same way. A site whose ranges and language equal an ancestor region's (a guest injecting
  itself) gets `refused_repeat`. Every refused region stays inside its host's preimage as bytes,
  so a refusal changes no `node_hash`.
- **Degradation.** A guest grammar that cannot load, a range the parser rejects, or a reparse that
  returns no tree gives a row with status `opaque` and a message; a guest tree with `ERROR` or
  `MISSING` nodes gives `parsed_with_errors` with the first error's position in the message. No
  region outcome may fail the file or the run. Today one can: `ordinal_space_needed` counts every
  injected node, and overflowing the 24-bit space aborts `parse_into_conn` for the whole run. The
  ordinal budget above closes that path, and the closing test pins it: a fixture whose injected
  nodes exceed the budget still parses, with host `node_hash`es equal to the
  `LLO_DISABLE_INJECTIONS=1` run.
- **Facts reach their region directly.** `node_defs` and `node_refs` gain a nullable
  `region_id` (NULL for a host fact). An injected fact names its region without a span join on
  `_region_ranges`, and `idx_regions_host` finds a host occurrence's regions. `_regions.file_id`
  joins to `_source.file_id` and to every occurrence through `nid >> 24`; `path` joins to
  `_tree.path`. (`_content_spans` keeps only `path`, because an opaque file has no `_source` row
  and no `file_id`.)
- **Comments.** A guest region's comments are classified by the guest language (N2). Rows for
  guest comment occurrences are S4.
- **Recursion.** `fold_injected_node` probes the guest language's injection engine as
  `fold_children` does, passing the region's id and depth down. A region's `host_nid` is set when
  the host node has an occurrence nid (a depth-1 site always does); deeper hosts are located by
  `parent_region_id` and span.
- **Nesting consistency** is a predicate, checked by tests over every `_regions` row: every range
  of a child lies inside its parent's ranges (or the file); `rel_start = start_byte −
  parent.start_byte`; `depth = parent.depth + 1`; sibling regions' ranges do not overlap; every
  range lies inside its host's preimage (the §(c) invariant).

### N10 — The region tree and the cellular sheaf: a partial fit

The owner asked whether the cellular sheaves (`rs/ll-open/sheaf`, ADR-0030) fit nested regions.
The answer is **partial**: the region tree is a declared decomposition with well-defined
restriction maps, and the existing region-hash scheme can label its parsed regions. But the sheaf
it carries is one where gluing cannot fail, so the defect measures nothing about nesting.

**What fits.**
- **A poset and a 1-dimensional cell complex.** Regions ordered by containment form a rooted
  tree: 0-cells are the file and its regions, 1-cells are host → guest containment edges. A tree
  has no 2-cells, so H¹ is zero and only H⁰ (agreement along edges) is meaningful.
- **A decomposition LLO declares, in ADR-0032's sense only loosely.** The region tree divides the
  file's bytes into parts that LLO declares; the region rows are the parts, and the descriptor is
  the injection queries plus the bounds. It is a nesting, not a partition: a host keeps the bytes
  outside its children. ADR-0032 D2 asks for the descriptor to be folded into an address. Here it
  is reached only through `parserId` → `injection_epoch`, and the bounds join that epoch only
  once S3 adds them (N9). No address is built over the region tree itself.
- **Restriction maps.** host → guest is well-defined on bytes: restrict the host region's bytes
  to the child's span, which is exactly the guest parser's input (`set_included_ranges`).
- **Region-granular invalidation, by reuse.** A parsed region can carry the existing
  `leyline/sheaf-region/v1` stalk hash with one member, its guest root:
  `regionHash(R) = A({RowSet, "leyline/sheaf-region/v1", params = parserId, canon_version 1},
  {(node_hash(guest root of R), 0, 0)})`, and edges keep `boundaryHash = regionHash(host) ⊕
  regionHash(guest)`. This needs no new construction. The daemon's sheaf topology can then
  invalidate only the regions whose guest root moved after an edit. Today a whole file's facts
  move together. The stalk is location-free: the guest root equals a standalone file's root
  (`inj_own_ca_root_dedups_with_standalone_sql`), so two regions with the same text, in any file,
  have equal stalks.
- **The file's own stalk.** The depth-0 cell, the file, has the stalk
  `regionHash(parserId, {node_hash(file root)})`. A refused or `opaque` region has no guest root
  and no stalk of its own. Its bytes are inside its host's preimage (§(c)), so its host's stalk
  (the parent region's, or the file's) moves when they change. That holds only for stalks LLO
  defines. For stalks a client supplies over the wire (`op_sheaf_set_topology`, as mache does
  with community regions), coverage is **advisory, not enforced**. A client stalk is an opaque
  hash: LLO cannot see its members, so it cannot check that one of them covers the enclosing
  leaf. The sheaf cascade is advisory anyway, with `node_hash` as the correctness floor
  (ADR-0030), so an uncovered region costs only invalidation precision. A client that wants
  refused or opaque regions covered includes a member whose fold contains the enclosing leaf,
  such as the file root.

**Where it stops.**
- **Two different objects.** Over bytes, the restriction maps are real, but gluing is vacuous:
  the file already contains every region. Over the hash stalks, there is no restriction map at
  all, because no declared function takes a host's hash to a guest's. δ⁰ over the XOR boundaries
  is then a change detector, not a consistency measure.
- **Gluing cannot fail.** The guest's input *is* the restriction of the host's bytes, by
  construction, so the byte-level sections always agree. "The guest parse disagrees with the
  host's span" is not a gluing failure: a guest that fails to parse is a local fact about one
  stalk (status `opaque` or `parsed_with_errors`). A bad span is a containment-predicate failure
  (N9). Neither is a δ⁰ defect.
- **The defect reduces to equality.** These stalks are hashes, so δ⁰ over them is a hash-equality
  check. ADR-0030 measured the same reduction on live stalks (the δ⁰ cascade is a hash-gated
  breadth-first search) and keeps the sheaf advisory, with `node_hash` as the correctness floor.
  Nesting inconsistency is caught by the N9 predicates and tests, not by the health metric.
- **No global section in the identity sense.** The section that glues every region is the file's
  bytes, already named by the blob hash. The `node_hash` fold is not a global section of this
  sheaf, because guest roots do not fold into hosts (§(c)).

**Identity domains stay separate.** A sheaf region hash is not a `node_hash`. It is a
`leyline/sheaf-region/v1` fold whose member is a `node_hash` and whose params bind `parserId`,
and the only mapping between the two is that formula. Region stalks are daemon topology state,
not projection rows: they add no column to `_regions` and no projection ABI. Wiring them into the
daemon's sheaf topology is slice S7, after v0.30.0. The daemon numbers topology regions as `u32`
(`install_region_labels`), and that state outlives an arena generation, so S7 must not use
`_regions.region_id` as the topology number: `delete_file_rows` reassigns it on every reparse.

## Test plan

Each invariant has a closing test. Unless named, tests go in
`rs/ll-open/cli-lib/tests/comment_occurrences_test.rs`.

| Invariant | Test |
| --- | --- |
| A comment-only edit moves no code `node_hash` where the comment is a grammar child | `a_swift_member_separator_comment_edit_moves_no_code_hash` (replaces `a_swift_member_separator_comment_is_folded_not_extra`); `a_scala_shebang_edit_moves_no_code_hash` |
| A node holding only comments does not hash them (leaf rule) | `a_comment_only_file_edit_moves_no_code_hash`: Python `# a` against `# b`, a YAML comment-only file, a shebang-only Scala file |
| Every inventory comment and shebang kind emits an addressed occurrence | `every_compiled_comment_kind_emits_an_addressed_occurrence`: one fixture per `(lang, kind)` row, asserting a `v_comment_address` row |
| Classification ignores `is_extra` both ways | `sql_comment_statement_is_code_and_marginalia_is_a_comment`; with S2, `a_ruby_heredoc_body_edit_moves_the_hash`, `a_csharp_define_edit_moves_the_hash` |
| PHP inline HTML is code (Q3) | `a_php_inline_html_only_edit_moves_the_file_hash`: two PHP files that differ only in the HTML outside `<?php ?>` have different root `node_hash`es |
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
| Moving shapes are exactly the published set (N5) | `v3_to_v4_moving_shapes_are_exactly_the_published_set` against a golden file of v0.29.0 hashes |
| Per-row class equals the view (N3) | `per_row_class_equals_v_comment_address_class` |
| Go directives inside a doc group (N8) | `a_go_directive_in_a_doc_group_neither_documents_nor_breaks_it` (`// doc` + `//go:embed x` + `var`; `//go:noinline` under a doc comment; `//nolint:errcheck`); `go_is_directive_matches_go_ast` (a table of `go/ast` `isDirective` cases); Go grouped specs, struct fields and interface methods; a Go `/* */` doc comment |
| `decl_nid` is the inner declaration (N8) | `export function`, `export default function`, `export const f = () =>` and a decorated class each map to the node holding the `node_defs` row; Rust `//!` at file top maps to the `source_file` root; Rust items inside `impl` |
| `documents_version` reaches reuse keys (N8) | `derived_from_v3_carries_documents_version`; a `v2` `derived_from` is refused; a documents-rule bump moves `derived_from` and not `parserId` |
| `_documents` (N8) | one fixture per rule (Go run above, Rust `///` past an attribute and `//!`, JSDoc above an `export`, Python docstring), each asserting the `rule` value; `an_ambiguous_comment_documents_nothing` (blank line, two declarations on one line, a directive above a function); `a_documents_rule_change_moves_no_node_hash` |
| Versioning | `projection_schema_version_is_stamped_and_pinned` (v14); the refusal test extended to v13 and the `kinds`-without-`class` shape; `extraction_epoch` 8; `ir_schema_version` `merkle-ast-v4`; `parserId` moves; `Head.rootHash` differs for a Swift separator fixture |
| (c) Injected comments | `an_injected_sql_comment_is_outside_both_folds` |
| (d) and the marker (S3) | `every_unprojected_file_has_one_opaque_span` (binary, unknown extension, oversized, parse failure; again after a removal and a scoped reparse); `an_injection_site_has_one_region_row`; `v_content_class_lists_comments_and_no_code` |
| Regions: the host commits to the bytes, not the guest root (N9) | `every_region_byte_is_in_a_host_code_leaf` (every non-trivia byte of each region lies in a code leaf of its host's fold); `an_injection_query_change_moves_no_host_hash` (disable injections with `LLO_DISABLE_INJECTIONS=1`: host `node_hash`es equal, `_regions` empty) |
| Nested regions (N9, when nested queries land) | a PHP → HTML → `<script>` JS fixture: one `_regions` row per level with the right `parent_region_id`, `depth`, file and relative spans; a JS-only edit moves the PHP file's `node_hash` and the JS guest root |
| Every non-trivia byte is in a preimage (N1, defect 6) | `every_non_trivia_byte_is_in_some_node_hash_preimage` over every grammar's fixtures; `two_markdown_paragraphs_differing_in_one_word_have_different_hashes` (`Calls foo.` against `Calls bar.`, and two fenced code blocks) |
| `span` children stay edge-reachable (N1) | `span_node_children_have_node_edges`: every `_ast` child of a Markdown `inline` (`block_continuation` included) has a `node_edge` row from its parent; `kinds.hash_shape` equals the inventory |
| Moving shapes are derived (N5) | `moving_shape_table_equals_inventory_derivation` |
| Injected facts name their region (N9) | every `node_defs`/`node_refs` row with a non-NULL `region_id` lies inside that region's `_region_ranges`, and every injected fact has one |
| A comment-hosted region commits through the comment (S5) | a doc comment holding a Markdown region: editing the region moves the comment's content row and no code `node_hash` |
| Combined regions (S5) | a PHP file with `<?php ?>` inside `<script>`: one HTML region with several `_region_ranges` rows, and the `<script>` → JavaScript region forms |
| No region fails the run (N9) | a fixture whose injected nodes exceed the ordinal budget still parses, with `refused_count` rows and host `node_hash`es equal to the `LLO_DISABLE_INJECTIONS=1` run; a guest injecting its own span gets `refused_repeat` |
| Region stalks (S7, N10) | an edit inside a `refused_depth` region moves its host's stalk; two files with the same injected text have equal region stalks |
| Region bounds and degradation | a chain nine deep gives a `refused_depth` row naming the limit at depth 9 and moves no `node_hash`; a guest with a syntax error gives `parsed_with_errors`; an unloadable guest gives `opaque`; the file parses in every case |

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
| S1 | N2 inventory, generator, four gates and `kinds.class` lifecycle; N1 leaf rule; N4 comment tag, byte hashing, trigger, witness variant; reclassify Swift `multiline_comment`, Scala shebang `comment`, SQL `marginalia`, the shebang kinds; `_comment_roles` (shebang and directive rules); `v_comment_address`; N5 bumps and every artifact listed there | `ley-line-open-1b5f3c`, `ley-line-open-5b8445`, `ley-line-open-8bcfa7` (`span` shape, defect 6) | v0.30.0 |
| S2 | Code-classed extras folded: Ruby `heredoc_body`, C# `preproc_*` other than the region markers (which are `directive`), PHP `text_interpolation` (Q3). They fold into whichever node tree-sitter attached them to, not into the construct that uses them; an ancestor's `node_hash` moves, a call's subtree need not | `ley-line-open-4cd459` | v0.30.0 |
| S3 | `_content_spans` (opaque), `_regions` and `_region_ranges` with recursion, bounds (including the ordinal budget) and degradation (N9), populated by the shipped Go → SQL and Markdown injections; the bounds added to `current_injection_epoch`; `v_content_class` | `ley-line-open-4e0852` | v0.30.0 |
| S3b | Pin κ to the IR lineage | `ley-line-open-5b9949` | v0.30.0 if it fits; otherwise the next lineage bump |
| S3c | Content-derived file kind (`_blob_kind`) and shebang parser selection (N1 (d)) | `ley-line-open-60321f` | v0.30.0 |
| S3d | Go extraction: composite-literal type refs; package-level `const`/`var` defs and qualified selector argument refs. Fact rows only, no `node_hash` effect; they ride `EXTRACTION_EPOCH` 8 | `ley-line-open-4624d9`, `ley-line-open-651909` | v0.30.0 |
| S3e | `_documents` v1 (N8): Go, Rust, JSDoc, Python | to file before implementation | v0.30.0 |
| S4 | Comment occurrences for injected trees (they have no `_ast` rows today) | to file | later |
| S5 | Nested injections: PHP `text` → HTML (combined), HTML `<script>` → JavaScript and `<style>` → CSS, and onward (JavaScript template strings holding SQL, combined; heredocs whose language comes from an `@injection.language` capture); comment-hosted regions (Markdown in doc comments, PHPDoc). Code and data: each host needs an `injection_engine` arm and its parts in `current_injection_epoch`, and the engine needs `injection.combined` and capture-named languages. On the S3 schema each moves `injection_epoch` and adds `_regions` and `_region_ranges` rows, with no projection bump and no `merkle-ast` bump. A guest that needs a new grammar moves `parserId` through the grammar digest, and `EXTRACTION_EPOCH` too if it adds an extension or file-name map | to file | later |
| S6 | A portable structural comment locator; more `_documents` languages | to file | later |
| S7 | Region stalks (`leyline/sheaf-region/v1` over the guest root) in the daemon's sheaf topology, for region-granular invalidation (N10); daemon state, no projection ABI | to file | later |

S1 estimate: the classification lookup replaces `is_comment_extra` in `fold_children` and
`fold_injected_node`; the leaf rule in both; `hash_comment`, byte hashing and the tag constant;
the trigger and witness variant; the inventory module, generator and four gates; the occurrence
role rules; the `intern_kind` signature and its callers; the `kinds` DDL column, stamp and view;
three version constants, the refusal probe and the tests that pin them; the Go sigma tag and
vectors; the documentation rows. Roughly 1,000 to 1,400 changed lines with tests. The whole
v0.30.0 bump (S1 to S3e) is several agents in separate worktrees with disjoint file scopes,
merged into one integration branch before one release.

**Nested regions: why the mechanism ships in v0.30.0 and the nested injections later.** Two
options were weighed.
- **Option A (recommended):** ship `_regions`, `_region_ranges`, the recursive probe, the bounds
  and the degradation rules in v0.30.0, populated by today's two injections. That adds roughly
  450 to 650 lines with tests to S3. Each later nested language (S5) is then engine code plus an
  `injections.scm` and fixtures. It moves `injection_epoch`, and `EXTRACTION_EPOCH` only if it
  adds a file map. It needs no projection bump and no consumer DDL re-pin.
- **Option B:** add the region tables with S5. That is the same code later, plus a second
  projection bump (`projection-v15`) and a second mache DDL re-pin. A design that has not met a
  combined injection risks a third bump.

Neither option needs a second `merkle-ast` bump, because of the §(c) rule: a guest root never
folds into its host, so no injection change moves a host `node_hash`. Folding guest roots into
hosts would make every S5 language a `merkle-ast` bump, which is a further reason against it.

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
- **Q3 — DECIDED: PHP inline HTML (`text_interpolation`) is folded into `node_hash` as code,
  in the v0.30.0 bump (S2).** Inline HTML is the program's output, so editing it changes what the
  program does. Today it is a dropped extra and an inline-HTML edit moves no `node_hash`; under
  v4 it moves the PHP file's `node_hash` (N5, N7). A later S5 can also mark the region as an
  injected HTML span with its own guest root without another identity change, because the host
  keeps folding the bytes, as a Go host keeps folding an injected SQL string.
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
