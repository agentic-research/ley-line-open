; HCL/Terraform typed address references — bead ley-line-open-55c1cc.
; Emission vocabulary documented in src/query_engine.rs. Behavior is pinned
; across the serialized SQLite boundary by cli-lib's hcl_address_refs_test.
;
; The query selects structure; refs.rs::extract_hcl adds the `env:` / `mod:`
; scheme because the generic query vocabulary has no static token-prefix field.
; Both patterns anchor @ref on the block so node_id/source_id/container
; attribution follows the same construct-level contract as call references.

; `variable "NAME" { ... }` -> env:NAME. A Terraform variable block has one
; label; resource/data blocks are excluded by the block-type predicate.
((block
  (identifier) @_type
  (string_lit) @name) @ref
 (#eq? @_type "variable"))

; `module "NAME" { source = "LOCATOR" }` -> mod:LOCATOR. `source` may occur
; anywhere in the direct body and other attributes/comments may intervene.
; The literal_value/string_lit shape deliberately excludes dynamic sources.
((block
  (identifier) @_type
  (body
    (attribute
      (identifier) @_key
      (expression
        (literal_value
          (string_lit) @name))))) @ref
 (#eq? @_type "module")
 (#eq? @_key "source"))
