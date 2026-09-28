; dform indentation (nvim-treesitter's captures). `dform fmt` is the
; reference (docs/grammar.md "Formatting"): a line is one step deeper than
; the line that holds the innermost bracket, block or body still open at
; its first token, and a line that starts with a closer sits with the line
; that opened it.

[
  (block)
  (statement_block)
  (attribute_block)
  (body_block)
  (object)
  (record)
  (record_type)
  (list)
  (comprehension)
  (arguments)
  (parenthesized)
  (index_expression)
] @indent.begin

; A body of one line continued after a `,` (a `{ }` body indents itself).
(rule body: (body)) @indent.begin
(check condition: (body)) @indent.begin
(value_rule condition: (body)) @indent.begin
(contribution condition: (body)) @indent.begin
(clause) @indent.begin

[
  "}"
  "]"
  ")"
] @indent.branch @indent.end

(comment) @indent.auto
(string) @indent.ignore
