; Bash extraction query — bead ley-line-open-780821 (partial algebra
; assessed on parent bead ley-line-open-e5addb).
; Emission vocabulary documented in src/query_engine.rs. Behavior is
; pinned by the fixture tests in src/refs.rs (`mod bash_tests`) and
; cli-lib's def_ref_extraction_fidelity_test (bash arm).
;
; Query-native language: this file IS the extractor — extract_bash is
; a pure engine delegate. Grammar: tree-sitter-bash 0.25. Text
; predicates (#any-of? / #not-any-of?) are evaluated natively by the
; Rust binding's QueryCursor::matches — per-pattern query data, no
; engine support needed.
;
; PARTIAL ALGEBRA BY DESIGN. Shell has no complete def/ref algebra
; (upstream ships no tags.scm — structural); the well-posed subset is
; function definitions, statically-named command invocations, and
; static `source` paths. NOT emitted, with reasons:
; - variable_assignment defs + $VAR expansion refs: dynamic scoping
;   plus export/env crossing process and file boundaries makes the
;   def↔ref join unsound, and expansion refs are the noisiest
;   emission shell has.
; - expansion-carrying source paths (`source "$HOME/x.sh"`): the path
;   is dynamic — not join-usable.
; - commands invoked through expansions (`"$CMD" --flag`): no stable
;   token (the command_name holds no word).
; - alias definitions: `alias ll='ls -l'` is a plain command node
;   whose defined name lives inside a string argument — text parsing,
;   not structure.

; ── Definitions ─────────────────────────────────────────────────────

; Covers both spellings — `f() {}` and `function f {}` are the same
; function_definition node kind with a `word` name field.
(function_definition
  name: (word) @name) @def

; ── Call-target references ──────────────────────────────────────────

; Statically-named command invocations. Joins to shell-function defs;
; external binaries (grep, make) emit unresolved refs — same class as
; printf in C. `source`/`.` are excluded: those command nodes emit as
; Imports below — one node, one fact.
(command
  name: (command_name
    (word) @name)
  (#not-any-of? @name "source" ".")) @ref

; ── Imports ─────────────────────────────────────────────────────────

; `source ./lib.sh` / `. /etc/vars.sh` with a static word path. The
; adjacency anchor pins @path to the FIRST argument — source's path
; is argument 1; later arguments become the sourced script's $1… The
; alias defaults to the path's last `/` segment (engine rule).
(command
  name: (command_name
    (word) @_cmd)
  .
  argument: (word) @path
  (#any-of? @_cmd "source" ".")) @import

; Static double-quoted path: `source "config/local.sh"`. The
; sole-named-child anchors require string_content to be the string's
; ONLY named child, which structurally excludes any expansion-carrying
; string (`"$HOME/x.sh"` has a simple_expansion sibling). @path is the
; string_content itself, so the quotes never enter the token.
(command
  name: (command_name
    (word) @_cmd)
  .
  argument: (string
    .
    (string_content) @path
    .)
  (#any-of? @_cmd "source" ".")) @import
