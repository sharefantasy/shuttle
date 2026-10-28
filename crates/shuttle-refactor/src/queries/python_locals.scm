; Hand-written Python locals (tree-sitter-python ships no locals.scm).
; Structural subset sufficient for function-scoped refactors (docs §6.1).

; Scopes
[
  (module)
  (function_definition)
  (lambda)
] @local.scope

; Definitions
(function_definition
  name: (identifier) @local.definition)

(parameters (identifier) @local.definition)
(parameters (typed_parameter (identifier) @local.definition))
(parameters (default_parameter name: (identifier) @local.definition))

(assignment
  left: (identifier) @local.definition)

(for_statement
  left: (identifier) @local.definition)

; References
(identifier) @local.reference

; Type references (P0.1): type positions that name a type/class.
; Python type annotations use `type: (identifier)` on params, and return
; annotations via `return_type`. Constructor calls surface as `call.function`.
(typed_parameter type: (type (identifier) @local.type-ref))
(function_definition return_type: (type (identifier) @local.type-ref))
(call function: (identifier) @local.type-ref)
(class_definition body: (block (expression_statement (assignment right: (call function: (identifier) @local.type-ref)))))
