; Go injections — bead ley-line-open-c822a6 (EXP2 of the queries-as-data
; design, parent bead ley-line-open-e5addb).
;
; Marks embedded-language regions using tree-sitter's upstream
; injections.scm conventions: the region is captured as
; @injection.content and the target language is per-pattern data via
; (#set! injection.language "..."). The engine (ts/src/injections.rs)
; anchors at the pattern ROOT node during the content-addressing fold,
; reads injection.language from the pattern's property settings, and
; reparses the captured byte range under the target grammar via
; Parser::set_included_ranges. Behavior is pinned by
; cli-lib/tests/injection_extraction_test.rs.
;
; HEURISTIC (per-pattern data, not engine code): Go has no syntactic
; marker for "this string is SQL", so a string literal injects only
; when its content opens with a statement-shaped SQL keyword sequence.
; Bare `update`/`delete`/`insert`/`create` prefixes are NOT enough —
; "update the docs" and "delete this file" are prose. `select` + one
; whitespace char is accepted: prose starting with "select " is rare in
; string literals, and a non-SQL match emits nothing anyway (facts come
; only from FROM/JOIN/invocation shapes per queries/sql/tags.scm).
;
; LIMITATIONS (documented, out of MVP scope):
; - interpreted_string_literal with escape sequences: the grammar
;   splits the content into multiple interpreted_string_literal_content
;   segments; only the FIRST segment matches this pattern shape, so
;   escape-bearing SQL injects partially. Upstream's answer is
;   @injection.combined — follow-up data work.
; - string concatenation / fmt.Sprintf-built SQL: dynamic, no stable
;   byte range — never injects.

((raw_string_literal
   (raw_string_literal_content) @injection.content)
  (#match? @injection.content "(?i)^\\s*(create\\s+(or\\s+replace\\s+)?(temporary\\s+|temp\\s+|unlogged\\s+)?(table|view|materialized\\s+view|function|schema)|select\\s|insert\\s+into\\s|update\\s+\\S+\\s+set\\s|delete\\s+from\\s|with\\s+\\S+\\s+as\\s*\\()")
  (#set! injection.language "sql"))

((interpreted_string_literal
   (interpreted_string_literal_content) @injection.content)
  (#match? @injection.content "(?i)^\\s*(create\\s+(or\\s+replace\\s+)?(temporary\\s+|temp\\s+|unlogged\\s+)?(table|view|materialized\\s+view|function|schema)|select\\s|insert\\s+into\\s|update\\s+\\S+\\s+set\\s|delete\\s+from\\s|with\\s+\\S+\\s+as\\s*\\()")
  (#set! injection.language "sql"))
