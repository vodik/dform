; dform highlights. Capture names follow nvim-treesitter's; editors map
; the ones they know.

(comment) @comment

; A doc comment (`#|` lines above an item; docs/grammar.md "Doc comments")
; is a comment to the grammar.
((comment) @comment.documentation
  (#match? @comment.documentation "^#[|]"))

; --- literals ---------------------------------------------------------------

(string) @string
(escape_sequence) @string.escape
(interpolation
  "${" @punctuation.special
  "}" @punctuation.special)
(integer) @number
[(true) (false)] @boolean
(rank) @attribute
(flag) @attribute
(any_resource) @type.builtin

; --- names ------------------------------------------------------------------

(identifier) @variable

((identifier) @variable.builtin
  (#eq? @variable.builtin "_"))

(member_expression
  field: (identifier) @property)
(block_path (identifier) @property)
(object_field key: (identifier) @property)
(named_argument name: (identifier) @property)

(dotted_name (identifier) @type)
(type name: (dotted_name (identifier) @type.builtin))
(type_alias name: (identifier) @type.definition)
(export type: (identifier) @type)

(call function: (identifier) @function.call)
(call function: (member_expression field: (identifier) @function.call))

(fact head: (call function: (identifier) @function))
(rule head: (call function: (identifier) @function))
(decl name: (dotted_name (identifier) @function))
(extern name: (dotted_name (identifier) @function))
(input_relation name: (identifier) @function)

(input name: (identifier) @constant)
(output name: (identifier) @constant)
(let name: (identifier) @constant)
(bind_arg name: (identifier) @variable.parameter)
(field_declaration name: (identifier) @variable.parameter)

(module name: (identifier) @module)
(policy name: (identifier) @module)
(use name: (identifier) @module)
(instance module: (identifier) @module)
(instance name: (identifier) @label)
(provider name: (identifier) @module)
(resource name: (identifier) @label)
(settings name: (identifier) @label)

(call
  function: (identifier) @function.builtin
  (#any-of? @function.builtin
    "count" "sum" "min" "max" "collect_set" "collect_list" "format" "ref"
    "attr" "want" "arg" "setting" "output" "input" "inet"
    "inet_subnet" "inet_host" "declassify" "cloud_attr" "cloud_exists"))

((identifier) @variable.builtin
  (#any-of? @variable.builtin "settings" "world"))

; A dot in a field-value position is a reference (proposal G, G-6): the
; value is the attribute itself, an apply-order edge, not its content read
; now. A field's value, a head or output argument, an element of a list or
; object there, and a comprehension's item are whole-value positions;
; everywhere else (a body, a clause, a builtin's argument, an index, a
; hole) a dot reads. `settings[…]`, `world.…` are always reads. Which chains
; name a resource is the resolver's business (a `let` alias or an instance
; output looks the same): this capture is the syntax's answer, and a
; language server refines it.
(field
  value: (member_expression) @variable.reference)
(field
  value: (list (member_expression) @variable.reference))
(field
  value: (object
    (object_field value: (member_expression) @variable.reference)))
(output
  value: (member_expression) @variable.reference)
(let
  value: (member_expression) @variable.reference)
(set
  value: (member_expression) @variable.reference)
(fact
  head: (call
    arguments: (arguments (member_expression) @variable.reference)))
(rule
  head: (call
    arguments: (arguments (member_expression) @variable.reference)))
(comprehension
  item: (member_expression) @variable.reference)

; --- keywords ---------------------------------------------------------------

[
  "edition" "provider" "key" "import" "input" "from" "output" "export"
  "extern" "persist" "type" "decl" "mixed" "let" "set"
  "module" "instance" "policy" "use" "resource" "settings"
] @keyword

["where" "check"] @keyword.conditional
["deny" "warn"] @keyword.exception
["not" "in" "has"] @keyword.operator

; --- operators and punctuation ----------------------------------------------

[
  "=" "+=" "==" "!=" "<" "<=" ">" ">=" "+" "-" "*" "/" "%" "|"
] @operator

["(" ")" "[" "]" "{" "}"] @punctuation.bracket
["," ":" "."] @punctuation.delimiter
