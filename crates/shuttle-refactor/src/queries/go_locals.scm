; Hand-written Go locals (tree-sitter-go ships no locals.scm).
; Structural subset sufficient for function-scoped refactors (docs §6.1).

; Scopes
[
  (source_file)
  (function_declaration)
  (method_declaration)
  (func_literal)
  (block)
  (for_statement)
  (if_statement)
  (expression_switch_statement)
  (type_switch_statement)
] @local.scope

; Definitions
(function_declaration
  name: (identifier) @local.definition)

(parameter_declaration
  name: (identifier) @local.definition)

(short_var_declaration
  left: (expression_list (identifier) @local.definition))

(var_spec
  name: (identifier) @local.definition)

(const_spec
  name: (identifier) @local.definition)

(range_clause
  left: (expression_list (identifier) @local.definition))

; References
(identifier) @local.reference

; Type references (P0.1): Go param/field/var types are named via type_identifier.
; Composite literals `Foo{...}` name the type in `type: (type_identifier)`.
(parameter_declaration type: (type_identifier) @local.type-ref)
(var_spec type: (type_identifier) @local.type-ref)
(pointer_type (type_identifier) @local.type-ref)
(composite_literal type: (type_identifier) @local.type-ref)
(qualified_type name: (type_identifier) @local.type-ref)
(field_declaration type: (type_identifier) @local.type-ref)
