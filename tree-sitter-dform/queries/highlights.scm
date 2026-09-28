; dform highlights. Capture names follow nvim-treesitter's; editors map
; the ones they know.

(comment) @comment

; --- literals ---------------------------------------------------------------

(string) @string
(escape_sequence) @string.escape
(interpolation
  "{" @punctuation.special
  "}" @punctuation.special)
(integer) @number
[(true) (false)] @boolean
(null) @constant.builtin
(keypath) @string.special.path
(rank) @attribute
(flag) @attribute
(any_resource) @type.builtin

; --- names ------------------------------------------------------------------

(identifier) @variable

((identifier) @variable.builtin
  (#eq? @variable.builtin "_"))

(member_expression
  field: (identifier) @property)
(instance_expression
  name: (instance_name) @variable.member)
(block_path (identifier) @property)
(object_field key: (identifier) @property)
(record_field key: (identifier) @property)

(dotted_name (identifier) @type)
(type name: (dotted_name (identifier) @type.builtin))

(call function: (identifier) @function.call)
(call function: (member_expression field: (identifier) @function.call))
(record name: (identifier) @function.call)

(fact head: (call function: (identifier) @function))
(rule head: (call function: (identifier) @function))
(fact head: (record name: (identifier) @function))
(rule head: (record name: (identifier) @function))
(decl name: (dotted_name (identifier) @function))
(extern name: (dotted_name (identifier) @function))
(export name: (identifier) @function)
(input_relation name: (identifier) @function)

(value_rule name: (identifier) @constant)
(input name: (identifier) @constant)
(output name: (identifier) @constant)
(let name: (identifier) @constant)
(with name: (identifier) @constant)
(bind_arg name: (identifier) @variable.parameter)
(field_declaration name: (identifier) @variable.parameter)

(module name: (identifier) @module)
(policy name: (identifier) @module)
(scenario name: (identifier) @module)
(apply name: (identifier) @module)
(instance module: (identifier) @module)
(instance name: (identifier) @label)
(provider name: (identifier) @module)
(resource name: (identifier) @label)
(settings name: (identifier) @label)

(call
  function: (identifier) @function.builtin
  (#any-of? @function.builtin
    "count" "sum" "min" "max" "collect_set" "collect_list" "format" "ref"
    "attr" "want" "arg" "setting" "output" "input" "member" "inet"
    "inet_subnet" "inet_host" "declassify" "cloud_attr" "cloud_exists"))

((identifier) @variable.builtin
  (#any-of? @variable.builtin "settings" "world"))

; A dot in a field-value position is a reference (proposal G, G-6): the
; value is the attribute itself, an apply-order edge, not its content read
; now. A field's value, a head or output argument, an element of a list or
; object there, and a comprehension's item are whole-value positions;
; everywhere else (a body, a clause, a builtin's argument, an index, a
; hole) a dot reads. `settings.…`, `world.…` are always reads. Which chains
; name a resource is the resolver's business (a `let` alias or an instance
; output looks the same): this capture is the syntax's answer, and a
; language server refines it.
(field
  value: [(member_expression) (instance_expression)] @variable.reference)
(field
  value: (list [(member_expression) (instance_expression)] @variable.reference))
(field
  value: (object
    (object_field value: [(member_expression) (instance_expression)] @variable.reference)))
(output
  value: [(member_expression) (instance_expression)] @variable.reference)
(value_rule
  value: [(member_expression) (instance_expression)] @variable.reference)
(contribution
  value: [(member_expression) (instance_expression)] @variable.reference)
(fact
  head: (call
    arguments: (arguments [(member_expression) (instance_expression)] @variable.reference)))
(rule
  head: (call
    arguments: (arguments [(member_expression) (instance_expression)] @variable.reference)))
(comprehension
  item: [(member_expression) (instance_expression)] @variable.reference)

; --- keywords ---------------------------------------------------------------

[
  "edition" "provider" "stack" "import" "as" "input" "relation" "from"
  "output" "export" "contributes" "extern" "persist" "type" "decl" "mixed"
  "open" "let" "module" "instance" "policy" "apply" "scenario" "with"
  "resource" "settings"
] @keyword

["when" "if" "where"] @keyword.conditional
["for" "some" "ordered" "by"] @keyword.repeat
["deny" "warn" "constraint"] @keyword.exception
["not" "in" "exists" "has"] @keyword.operator

; --- operators and punctuation ----------------------------------------------

[
  "=" "+=" "==" "!=" "<" "<=" ">" ">=" "+" "-" "*" "/" "%" "|"
] @operator

["(" ")" "[" "]" "{" "}"] @punctuation.bracket
["," ":" "."] @punctuation.delimiter
