; Java extraction query — bead ley-line-open-5e21c2.
; Emission vocabulary documented in src/query_engine.rs. Behavior is
; pinned by the fixture tests in src/refs.rs (`mod java_tests`) and
; cli-lib's def_ref_extraction_fidelity_test (Java arm).
;
; First query-native language: no imperative extractor ever existed —
; this file IS the extractor. One arm stays imperative in
; refs.rs::extract_java (leak note there): qualified `Type.method`
; defs read the type name from an ANCESTOR class/interface/enum/record
; body, and patterns match downward only.
;
; Constructors are NOT matched — a constructor's token is the class
; name the class_declaration pattern already emits, and a second def
; row under the same token at a different node_id adds noise, not
; resolution power.

; ── Definitions ─────────────────────────────────────────────────────

(class_declaration
  name: (identifier) @name) @def

(interface_declaration
  name: (identifier) @name) @def

(enum_declaration
  name: (identifier) @name) @def

(record_declaration
  name: (identifier) @name) @def

; Bare method name only — the qualified `Type.method` companion is
; emitted by `java_enclosing_type` (see header). Covers class,
; interface (bodyless), enum, and record methods: all are
; method_declaration nodes.
(method_declaration
  name: (identifier) @name) @def

; ── Call-target references ──────────────────────────────────────────

; Receiver invocations dual-emit `receiver.method` + `method`:
; @qualifier is the object of ANY kind, so `this.cfg.batch()` emits
; `this.cfg.batch` + `batch`. The optional capture makes one pattern
; cover bare invocations too — no object, bare name only.
(method_invocation
  object: (_)? @qualifier
  name: (identifier) @name) @ref

; `new Point(1, 2)` is the call-site for a class — mache's dead_code
; rule needs the ref (same reasoning as Go's value-position refs).
(object_creation_expression
  type: (type_identifier) @name) @ref

; ── Imports ─────────────────────────────────────────────────────────

; `import java.util.List;` (and `import static java.lang.Math.max;` —
; same scoped_identifier shape) — the alias is the last `.` segment,
; captured explicitly because the engine's alias default splits on
; `/`. The trailing `.` anchor requires the scoped_identifier to be
; the LAST named child, which excludes `import java.util.*;` — the
; wildcard's `asterisk` node follows, and a wildcard has no
; addressable alias (same rule as Rust's `use foo::*`).
(import_declaration
  (scoped_identifier
    name: (_) @alias) @path .) @import

; `import foo;` — path and alias are the same node.
(import_declaration
  (identifier) @path @alias .) @import
