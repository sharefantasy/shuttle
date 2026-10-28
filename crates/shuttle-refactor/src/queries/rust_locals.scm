; Hand-written Rust locals (tree-sitter-rust ships no locals.scm).
; Structural subset sufficient for function-scoped refactors (docs §6.1).

; Scopes
[
  (source_file)
  (function_item)
  (closure_expression)
  (block)
  (if_expression)
  (for_expression)
  (while_expression)
  (loop_expression)
  (match_arm)
] @local.scope

; Definitions
(function_item
  name: (identifier) @local.definition)

(parameter
  pattern: (identifier) @local.definition)

(let_declaration
  pattern: (identifier) @local.definition)

(const_item
  name: (identifier) @local.definition)

(static_item
  name: (identifier) @local.definition)

; References
(identifier) @local.reference

; Type references (P0.1): type positions that name a named type.
; Params `x: Foo`, return `-> Foo`, let bindings `let x: Foo`, and struct/enum
; construction all surface the type name as a `type_identifier`.
(parameter type: (type_identifier) @local.type-ref)
(function_item return_type: (type_identifier) @local.type-ref)
(let_declaration type: (type_identifier) @local.type-ref)
(reference_type (type_identifier) @local.type-ref)
(generic_type type: (type_identifier) @local.type-ref)
(scoped_type_identifier name: (type_identifier) @local.type-ref)
(call_expression function: (identifier) @local.type-ref)
(field_declaration type: (type_identifier) @local.type-ref)
