; dform scopes. A variable's scope is its rule (docs/grammar.md "Names"):
; a rule, a fact, a value rule, a contribution, a check or a block, and
; every occurrence in it is the same variable. Declared names (inputs,
; value rules, `let` aliases, resources, modules) are definitions of the
; enclosing statement list.

[
  (source_file)
  (statement_block)
  (rule)
  (fact)
  (value_rule)
  (contribution)
  (check)
  (resource)
  (settings)
  (instance)
] @local.scope

(input name: (identifier) @local.definition.var)
(output name: (identifier) @local.definition.var)
(let name: (identifier) @local.definition.var)
(value_rule name: (identifier) @local.definition.var)
(resource name: (identifier) @local.definition.var)
(module name: (identifier) @local.definition.namespace)
(instance name: (identifier) @local.definition.namespace)
(decl name: (dotted_name) @local.definition.function)
(extern name: (dotted_name) @local.definition.function)

(identifier) @local.reference
