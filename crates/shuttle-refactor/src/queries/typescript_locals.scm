; Hand-written TypeScript locals (upstream locals.scm only covers params).
; Structural subset sufficient for function-scoped refactors (docs §6.1).

; Scopes
[
  (program)
  (function_declaration)
  (function_expression)
  (arrow_function)
  (method_definition)
  (statement_block)
  (for_statement)
  (for_in_statement)
  (if_statement)
] @local.scope

; Definitions
(function_declaration
  name: (identifier) @local.definition)

(required_parameter (identifier) @local.definition)
(optional_parameter (identifier) @local.definition)

(variable_declarator
  name: (identifier) @local.definition)

; References
(identifier) @local.reference

; Type references (P0.1): TS type annotations name types via type_annotation
; wrapping a type_identifier. Params, returns, var decls, and `new Foo()`.
(type_annotation (type_identifier) @local.type-ref)
(type_annotation (generic_type name: (type_identifier) @local.type-ref))
(new_expression constructor: (identifier) @local.type-ref)
(public_field_definition type: (type_annotation (type_identifier) @local.type-ref))
(property_signature type: (type_annotation (type_identifier) @local.type-ref))
