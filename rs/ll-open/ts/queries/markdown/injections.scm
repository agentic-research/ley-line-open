; Markdown injections — bead ley-line-open-ea1e42 (found by mache-eb2bf3).
;
; tree-sitter-md ships TWO grammars: the block grammar (what
; TsLanguage::Markdown parses with) leaves a paragraph's content as one
; opaque `inline` node — code spans, links, and emphasis do not exist
; as nodes at all. This is not a preprocessor problem: the crate's own
; INLINE_LANGUAGE parses exactly those ranges, handling the cases
; hand-rolled backtick scanning gets wrong (escaped backticks,
; double-backtick spans containing a backtick, fenced vs prose).
;
; Every `inline` node is an injection site for the inline grammar —
; unconditional, unlike Go→SQL's content heuristic, because the block
; grammar already guarantees the range is prose-level inline content
; (fenced/indented code lands in fenced_code_block/indented_code_block,
; never in `inline`).
;
; The injected tree emits node_refs via queries-side extraction
; (extract_markdown_inline in src/refs.rs): a backtick code span citing
; a symbol IS a reference — that is the join surface mache's
; drift_doc_dead_symbol_reference reads.

((inline) @injection.content
  (#set! injection.language "markdown-inline"))
