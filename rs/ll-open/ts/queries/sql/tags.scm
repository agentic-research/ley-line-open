; SQL extraction query — bead ley-line-open-780821 (partial algebra
; assessed on parent bead ley-line-open-e5addb).
; Emission vocabulary documented in src/query_engine.rs. Behavior is
; pinned by the fixture tests in src/refs.rs (`mod sql_tests`) and
; cli-lib's def_ref_extraction_fidelity_test (SQL arm).
;
; Query-native language: this file IS the extractor — extract_sql is a
; pure engine delegate. Grammar: DerekStride/tree-sitter-sql (crate
; tree-sitter-sequel 0.3).
;
; PARTIAL ALGEBRA BY DESIGN. SQL has no complete def/ref algebra
; (upstream ships no tags.scm — structural); the well-posed subset is
; DDL names as defs and relation/invocation positions as their
; use-sites. NOT emitted, with reasons:
; - create_index / create_trigger NAMES: no use-site exists in the
;   language (only DROP) — a def token that can never join is a
;   permanent dead_code false positive.
; - column defs/refs: bare column tokens collide across tables
;   (`users.name` vs `orders.name` both emit `name`); resolving them
;   needs schema knowledge the token algebra cannot express.
; - DROP/ALTER targets and index/FK ON-table wiring: lifecycle and
;   schema furniture, not use — counting them as refs would mask
;   "created, altered, indexed, never queried" dead tables.
; - CTE names: query-scoped; their use-sites ride the same relation
;   shape as table refs, so a CTE shadowing a table name errs toward
;   false-negative (table marked used) — accepted, no def emitted.
; - CREATE PROCEDURE: tree-sitter-sequel 0.3 does not parse it at all
;   (ERROR nodes) — grammar limitation, not a choice.
; - imports: SQL has no in-language import construct (\i / \include
;   are psql metacommands outside the grammar).

; ── Definitions ─────────────────────────────────────────────────────

; object_reference carries an optional schema field; @qualifier
; dual-emits `analytics.events` + `events` (engine default sep ".").
(create_table
  (object_reference
    schema: (identifier)? @qualifier
    name: (identifier) @name)) @def

(create_view
  (object_reference
    schema: (identifier)? @qualifier
    name: (identifier) @name)) @def

(create_materialized_view
  (object_reference
    schema: (identifier)? @qualifier
    name: (identifier) @name)) @def

(create_function
  (object_reference
    schema: (identifier)? @qualifier
    name: (identifier) @name)) @def

; The schema name is a bare identifier immediately after the SCHEMA
; keyword (no object_reference wrapper); the adjacency anchor keeps
; trailing identifiers (AUTHORIZATION owner) out.
(create_schema
  (keyword_schema)
  .
  (identifier) @name) @def

; ── Use-site references ─────────────────────────────────────────────

; FROM / JOIN / UPDATE targets. select's FROM wraps its
; object_reference in a relation; matching the wrapper keeps the def
; positions (create_*'s own object_reference) out.
(relation
  (object_reference
    schema: (identifier)? @qualifier
    name: (identifier) @name)) @ref

; INSERT INTO target — object_reference is a direct child of insert.
(insert
  (object_reference
    schema: (identifier)? @qualifier
    name: (identifier) @name)) @ref

; DELETE FROM target — delete's from holds the object_reference
; directly (no relation wrapper), so this pattern matches nothing in
; a select, where the object_reference sits one level deeper.
(from
  (object_reference
    schema: (identifier)? @qualifier
    name: (identifier) @name)) @ref

; Function call sites (`SELECT add_one(2)`) — the join partners of
; create_function defs. Builtins (count, lower) emit as unresolved
; refs, same class as printf in C.
(invocation
  (object_reference
    schema: (identifier)? @qualifier
    name: (identifier) @name)) @ref

; Trigger body edges. The trigger's own NAME does not emit (header),
; but its edges are real usage: writes to the ON table invoke the
; EXECUTE FUNCTION target, so a function used only by a trigger is
; not dead. Adjacency anchors pin each object_reference to its
; keyword, keeping the trigger-name object_reference (first child,
; before keyword_on) out.
(create_trigger
  (keyword_on)
  .
  (object_reference
    schema: (identifier)? @qualifier
    name: (identifier) @name)) @ref

(create_trigger
  (keyword_execute)
  (keyword_function)
  .
  (object_reference
    schema: (identifier)? @qualifier
    name: (identifier) @name)) @ref
