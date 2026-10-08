; dform scopes. A variable's scope is its rule (docs/grammar.md "Names"):
; a rule, a fact, a `let`, a `set`, an output, a check or a block, and
; every occurrence in it is the same variable. Declared names (inputs,
; `let`s, outputs, resources (a component's too), components, the names a `use`
; binds) are definitions of the enclosing
; statement list.

[
  (source_file)
  (statement_block)
  (rule)
  (fact)
  (let)
  (set)
  (output)
  (check)
  (resource)
] @local.scope

(input name: (identifier) @local.definition.var)
(output name: (identifier) @local.definition.var)
(let name: (identifier) @local.definition.var)
(parameter name: (identifier) @local.definition.parameter)
(resource name: (identifier) @local.definition.var)
(component name: (identifier) @local.definition.namespace)
(use name: (identifier) @local.definition.namespace)
(decl name: (dotted_name) @local.definition.function)
(extern name: (dotted_name) @local.definition.function)

(identifier) @local.reference
