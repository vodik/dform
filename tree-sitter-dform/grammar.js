/**
 * @file The dform grammar for editors (docs/grammar.md).
 *
 * The compiler's parser is crates/dform-core/src/syntax/parser.rs; this
 * grammar is for highlighting, indentation and navigation, and
 * tests/treesit_agreement.rs holds the two to the same corpus. A
 * statement's first token decides what it is (proposal H, H-2). A newline
 * outside `( )`, `[ ]` and an object's braces ends a statement, a block
 * entry or a literal of a `{ }` body, and nothing continues a line: the
 * external scanner (src/scanner.c) emits a newline only where the grammar
 * can take one, so everywhere else it is whitespace, as in the compiler.
 *
 * One consequence: where a statement cannot end yet, a newline is
 * whitespace here but an end to the compiler, so a line broken after an
 * operator, a comma or `where`, or before a block's `{`, is an error only in
 * the compiler's diagnostics.
 */

/// <reference types="tree-sitter-cli/dsl" />
// @ts-check

const IDENT = /[A-Za-z_][A-Za-z0-9_]*/;

// The statement keywords: a statement's first token (docs/grammar.md
// "Tokens"). Anywhere a plain name is expected a keyword is a name.
const STATEMENT_KEYWORDS = [
  'key', 'type', 'decl', 'extern', 'input',
  'output', 'let', 'set', 'component', 'instance', 'use', 'resource',
  'deny', 'warn',
];

// The body words, the clause word, the reserved `if` and the literals:
// never a name in a term.
const TERM_WORDS = ['not', 'in', 'has', 'where', 'if', 'true', 'false'];

// Contextual words: plain names but where their construct is expected.
const CONTEXTUAL = [
  'from', 'mixed', 'check', 'as',
  'required', 'computed', 'id', 'sensitive', 'nullable',
];

// Words that may start a chain in a term (parser.rs `term_name`).
const TERM_NAMES = [...STATEMENT_KEYWORDS, ...CONTEXTUAL];

const PREC = {
  range: 0,
  add: 1,
  mul: 2,
  unary: 3,
};

/**
 * @param {RuleOrLiteral} rule
 * @returns {SeqRule}
 */
function commaSep1(rule) {
  return seq(rule, repeat(seq(',', rule)));
}

/**
 * Comma-separated, a trailing comma allowed.
 *
 * @param {RuleOrLiteral} rule
 * @returns {ChoiceRule}
 */
function commaSepTrailing(rule) {
  return optional(seq(commaSep1(rule), optional(',')));
}

export default grammar({
  name: 'dform',

  word: $ => $.identifier,

  // The body words have a construct of their own in a term and are never a
  // name there (parser.rs `term_name`); every keyword is a name where only
  // a name can go (`_word`).
  reserved: {
    global: _ => TERM_WORDS,
    names: _ => [],
  },

  externals: $ => [
    $._newline,
    $.string_content,
    $._error_sentinel,
  ],

  extras: $ => [/\s/, $.comment],

  supertypes: $ => [$._statement, $._literal, $._term],

  conflicts: $ => [
    [$._chain_head, $._word],
    [$._primary, $.member_expression],
    [$._primary, $.index_expression],
    [$._primary, $._call_access],
  ],

  rules: {
    source_file: $ => seq(
      repeat($._newline),
      repeat(seq($._statement, repeat1($._newline))),
      optional($._statement),
    ),

    comment: _ => token(seq('#', /[^\n]*/)),

    _statement: $ => choice(
      $.input,
      $.input_relation,
      $.output,
      $.let,
      $.set,
      $.extern,
      $.type_declaration,
      $.type_alias,
      $.decl,
      $.component,
      $.instance,
      $.use,
      $.resource,
      $.check,
      $.rule,
      $.fact,
    ),

    // `input k: T`, or `key k: T`: an input the target gives (R-29); or
    // `input k { f: T = d .. }`, an object input by its fields (R-54).
    input: $ => choice(
      seq(
        choice('input', 'key'),
        field('name', $._word),
        ':',
        field('type', $._type),
        optional(seq('=', field('default', $._term))),
        optional($.refinement),
        // `input k: T where B`: a dependent input (R-104).
        optional($.clause),
      ),
      seq('input', field('name', $._word), field('body', $.input_fields)),
    ),

    // An object input's fields: `name: T [= d] [check B]`, a nested
    // object `name: { .. }`.
    input_fields: $ => seq(
      '{',
      repeat($._newline),
      repeat(seq(alias($.input_field, $.attribute_declaration), $._separator)),
      optional(alias($.input_field, $.attribute_declaration)),
      '}',
    ),

    input_field: $ => seq(
      field('path', $.block_path),
      ':',
      choice(
        field('body', $.input_fields),
        seq(
          field('type', choice($.type, $.string)),
          optional(seq('=', field('default', $._term))),
          optional($.refinement),
        ),
      ),
    ),

    // `input p from SOURCE [where B]`: rows of the relation `p`, its
    // columns its `decl`'s; `input p` alone, in a module, a relation its
    // user gives (R-55).
    input_relation: $ => seq(
      'input',
      field('name', $._word),
      optional(seq(
        'from',
        field('source', $._term),
        optional(field('selector', $.selector)),
        optional(seq('where', field('condition', $._body))),
      )),
    ),

    // `output k: T = t [where B]`: one statement; `output p` marks the
    // relation `p` as the copy's; `output k { f = t .. }` an object (R-55).
    output: $ => choice(
      seq(
        'output',
        field('name', $._word),
        optional(seq(':', field('type', $._type))),
        optional(seq('=', field('value', $._term))),
        optional(seq('where', field('condition', $._body))),
      ),
      seq(
        'output',
        field('name', $._word),
        field('body', $.output_fields),
        optional(seq('where', field('condition', $._body))),
      ),
    ),

    // An object output's fields: `name [: T] = t`, a nested `name: { .. }`.
    output_fields: $ => seq(
      '{',
      repeat($._newline),
      repeat(seq(alias($.output_field, $.attribute_declaration), $._separator)),
      optional(alias($.output_field, $.attribute_declaration)),
      '}',
    ),

    output_field: $ => seq(
      field('path', $.block_path),
      choice(
        seq('=', field('value', $._term)),
        seq(':', field('body', $.output_fields)),
        seq(':', field('type', choice($.type, $.string)), '=', field('value', $._term)),
      ),
    ),

    // `let k [: T] = t [where B]`: a value, typed or not (R-74).
    let: $ => seq(
      'let',
      field('name', $._word),
      optional(seq(':', field('type', $._type))),
      '=',
      field('value', $._term),
      optional(seq('where', field('condition', $._body))),
    ),

    // `set r.p = t [@rank] [where B]`: a contribution; `set { r.p = t .. }`
    // several under one clause; `set from DOC` a document's leaves to the
    // inputs at their paths (R-38).
    set: $ => seq(
      'set',
      choice(
        seq(
          field('target', $._chain),
          field('operator', choice('=', '+=')),
          field('value', $._term),
        ),
        field('body', $.set_block),
        seq('from', field('source', $._term), optional(field('selector', $.selector))),
      ),
      optional(field('rank', $.rank)),
      optional(seq('where', field('condition', $._body))),
    ),

    set_block: $ => seq(
      '{',
      repeat($._newline),
      optional(choice(
        seq(repeat(seq($.set_entry, $._separator)), $.set_entry),
        repeat1(seq($.set_entry, $._separator)),
      )),
      '}',
    ),

    set_entry: $ => seq(
      field('target', $._chain),
      field('operator', choice('=', '+=')),
      field('value', $._term),
      optional(field('rank', $.rank)),
    ),

    // `.name` and `[*]` steps into a document after `from` (R-39).
    selector: $ => repeat1(choice(
      seq(token.immediate('.'), choice($._word, $.string)),
      seq(token.immediate('['), '*', ']'),
    )),

    extern: $ => seq(
      'extern',
      field('name', $.dotted_name),
      '(',
      commaSep1($.bind_arg),
      ')',
    ),

    bind_arg: $ => seq(
      field('mode', choice('+', '-')),
      field('name', $._word),
      optional(seq(':', field('type', $._type))),
    ),

    type_declaration: $ => seq('type', field('name', $.dotted_name), field('body', $.attribute_block)),

    // `type NAME = TYPE`: a transparent alias.
    // `type T = TYPE`, or `type T = component { .. }`: a component
    // signature, the inputs and outputs a component that has it declares
    // (R-104).
    type_alias: $ => seq(
      'type',
      field('name', $.identifier),
      '=',
      choice(field('type', $._type), field('signature', $.signature)),
    ),

    signature: $ => seq('component', field('body', $.statement_block)),

    attribute_block: $ => seq(
      '{',
      repeat($._newline),
      repeat(seq($.attribute_declaration, $._separator)),
      optional($.attribute_declaration),
      '}',
    ),

    attribute_declaration: $ => seq(
      field('path', $.block_path),
      ':',
      choice(
        field('body', $.attribute_block),
        seq(
          // A `{` here opens a nested attribute block, never a record type.
          field('type', choice($.type, $.string)),
          repeat($.flag),
          optional($.refinement),
        ),
      ),
    ),

    flag: _ => choice('required', 'computed', 'id', 'sensitive', 'nullable'),

    // `decl p(a, b: T) [mixed]`: a relation by its columns.
    decl: $ => seq(
      'decl',
      field('name', $.dotted_name),
      $.columns,
      optional('mixed'),
    ),

    columns: $ => seq('(', commaSep1($.field_declaration), optional(','), ')'),

    field_declaration: $ => seq(
      field('name', $._word),
      optional(seq(':', field('type', $._type))),
    ),

    // `component NAME { .. }`: a component declared as an item (R-65).
    component: $ => seq(
      'component',
      field('name', $._word),
      optional(seq(':', field('signature', $._type))),
      field('body', $.statement_block),
    ),

    // `instance PATH NAME [{ .. }] [where B]`: a named copy of a
    // component.
    instance: $ => seq(
      'instance',
      field('component', $.dotted_name),
      field('name', $._name),
      optional(field('body', alias($.copy_block, $.block))),
      optional($.clause),
    ),

    // `use PATH [as NAME] [{ .. }] [where B]`: a module by its path from
    // the project root, a component stamped once (its inputs the block),
    // or a stack's deployments.
    use: $ => seq(
      'use',
      field('path', $.dotted_name),
      optional(seq('as', field('name', $._word))),
      optional(field('body', alias($.copy_block, $.block))),
      optional($.clause),
    ),

    // A `use` or `instance` block: its inputs, and the rows of the
    // relations its module takes, `p(t, ..) [where B]` or `p from TERM
    // [where B]` (R-55).
    copy_block: $ => seq(
      '{',
      repeat($._newline),
      optional(choice(
        seq(repeat(seq($._copy_entry, $._separator)), $._copy_entry),
        repeat1(seq($._copy_entry, $._separator)),
      )),
      '}',
    ),

    _copy_entry: $ => choice($.field, $.rule, $.fact, alias($.rows_from, $.input_relation)),

    rows_from: $ => seq(
      field('name', $._word),
      'from',
      field('source', $._term),
      optional(field('selector', $.selector)),
      optional(seq('where', field('condition', $._body))),
    ),

    statement_block: $ => seq(
      '{',
      repeat($._newline),
      repeat(seq($._statement, repeat1($._newline))),
      optional($._statement),
      '}',
    ),

    resource: $ => seq(
      'resource',
      field('type', $.dotted_name),
      field('name', choice($._word, $.string)),
      optional(field('rank', $.rank)),
      field('body', $.block),
      optional($.clause),
    ),

    // A resource's entries,
    // separated by a newline or a comma. Its clause follows it.
    block: $ => seq(
      '{',
      repeat($._newline),
      optional($._fields),
      '}',
    ),

    _fields: $ => choice(
      seq(repeat(seq($.field, $._separator)), $.field),
      repeat1(seq($.field, $._separator)),
    ),

    _separator: $ => choice(seq(',', repeat($._newline)), repeat1($._newline)),

    // `where B` after a block: the block is the head (R-1).
    clause: $ => seq('where', field('condition', $._body)),

    // `path = t`, or the path alone: `zone` is `zone = zone` (R-33).
    field: $ => seq(
      field('path', $.block_path),
      optional(seq(
        field('operator', choice('=', '+=')),
        field('value', $._term),
      )),
      optional(field('rank', $.rank)),
    ),

    // An attribute path: `a.b[0]."c-d"`.
    block_path: $ => seq(
      choice($._word, $.string),
      repeat(choice(
        seq(token.immediate('.'), choice($._word, $.string)),
        seq(token.immediate('['), $.integer, ']'),
      )),
    ),

    check: $ => seq(
      field('kind', choice('deny', 'warn')),
      field('message', $.string),
      optional(field('details', $.object)),
      optional(seq('where', field('condition', $._body))),
    ),

    rule: $ => seq(
      field('head', $.call),
      optional(field('rank', $.rank)),
      'where',
      field('body', $._body),
    ),

    fact: $ => seq(
      field('head', $.call),
      optional(field('rank', $.rank)),
    ),

    // `check B` after an input's or an attribute's type: a refinement.
    refinement: $ => seq('check', field('condition', $.body)),

    // --- bodies and literals -------------------------------------------------

    _body: $ => choice($.body, $.body_block),

    body: $ => prec.right(commaSep1($._literal)),

    // A comprehension's literals: a comma after the last too (R-52).
    comprehension_body: $ => seq(commaSep1($._literal), optional(',')),

    body_block: $ => seq(
      '{',
      repeat($._newline),
      repeat(seq($._literal, $._separator)),
      optional($._literal),
      '}',
    ),

    _literal: $ => choice(
      $.not_literal,
      $.not_block,
      $._positive_literal,
    ),

    _positive_literal: $ => choice(
      $.has_literal,
      $.comparison,
      $.in_literal,
      $.not_in_literal,
      $.atom_literal,
      $.truth_literal,
    ),

    not_literal: $ => seq('not', $._positive_literal),

    not_block: $ => prec(1, seq('not', $.body_block)),

    has_literal: $ => seq('has', $._term),

    comparison: $ => seq($._term, repeat1(seq(field('operator', choice('=', '==', '!=', '<', '<=', '>', '>=')), $._term))),

    // `x in resource` is any resource; `resource` is not a name there.
    in_literal: $ => choice(
      prec(1, seq(field('element', $._term), 'in', field('collection', alias('resource', $.any_resource)))),
      seq(field('element', $._term), 'in', field('collection', $._term)),
    ),

    not_in_literal: $ => seq(field('element', $._term), 'not', 'in', field('collection', $._term)),

    atom_literal: $ => $.call,

    truth_literal: $ => choice(
      $._chain,
      alias($._call_member, $.member_expression),
      alias($._call_index, $.index_expression),
    ),

    // --- terms ---------------------------------------------------------------

    _term: $ => choice(
      $.range,
      $.binary_expression,
      $.unary_expression,
      $._primary,
    ),

    // `lo..hi`, `lo..=hi` (R-56): enumerated by `in`; the compiler refuses
    // one anywhere else.
    range: $ => prec.left(PREC.range, seq(
      field('low', $._term),
      field('operator', choice('..', '..=')),
      field('high', $._term),
    )),

    binary_expression: $ => choice(
      prec.left(PREC.add, seq(field('left', $._term), field('operator', choice('+', '-')), field('right', $._term))),
      prec.left(PREC.mul, seq(field('left', $._term), field('operator', choice('*', '/', '%')), field('right', $._term))),
    ),

    unary_expression: $ => prec(PREC.unary, seq('-', $._term)),

    _primary: $ => choice(
      $.integer,
      $.quantity,
      $.string,
      $.true,
      $.false,
      $._chain,
      $.call,
      alias($._call_member, $.member_expression),
      alias($._call_index, $.index_expression),
      $.list,
      $.comprehension,
      $.object,
      $.parenthesized,
      $.tuple,
    ),

    parenthesized: $ => seq('(', $._term, ')'),

    // `(a, b, ..)`: a tuple pattern (R-58), after `in`, on the left of `=`
    // and as a relation's argument; the compiler refuses one as a value.
    tuple: $ => seq('(', $._term, repeat1(seq(',', $._term)), optional(','), ')'),

    // `name (.seg | [terms])*`: `.` is static, `[ ]` a key (H 5.1).
    _chain: $ => choice(
      $._chain_head,
      $.member_expression,
      $.index_expression,
    ),

    _chain_head: $ => choice(
      $.identifier,
      alias(choice(...TERM_NAMES), $.identifier),
    ),

    member_expression: $ => seq(
      field('object', $._chain),
      token.immediate('.'),
      field('field', choice($._word, $.string)),
    ),

    index_expression: $ => seq(
      field('object', $._chain),
      token.immediate('['),
      commaSep1(field('index', choice($._term, alias($._keyed, $.named_argument)))),
      ']',
    ),

    // A `.seg` or `[t]` after a call reads its result (R-71):
    // `oci.parse(image).digest`, `str.split(s, ":")[0]`; after `from` it is
    // the document's selector instead. Never a target or a function.
    _call_access: $ => choice(
      $.call,
      alias($._call_member, $.member_expression),
      alias($._call_index, $.index_expression),
    ),

    _call_member: $ => prec.dynamic(-1, seq(
      field('object', $._call_access),
      token.immediate('.'),
      field('field', choice($._word, $.string)),
    )),

    _call_index: $ => prec.dynamic(-1, seq(
      field('object', $._call_access),
      token.immediate('['),
      commaSep1(field('index', $._term)),
      ']',
    )),

    // `[k=v]`: a stack's deployment by its keys (R-65).
    _keyed: $ => seq(field('name', $._word), '=', field('value', $._term)),

    call: $ => seq(
      field('function', $._chain),
      '(',
      field('arguments', optional(alias($._arguments, $.arguments))),
      ')',
    ),

    _arguments: $ => seq(commaSep1(choice($._term, $.named_argument)), optional(',')),

    // `p(a: x)`: an argument by its column's name.
    named_argument: $ => seq(field('name', $._word), ':', field('value', $._term)),

    list: $ => seq('[', commaSepTrailing($._term), ']'),

    comprehension: $ => seq(
      '[',
      field('item', $._term),
      '|',
      field('condition', alias($.comprehension_body, $.body)),
      ']',
    ),

    object: $ => seq('{', commaSepTrailing($.object_field), '}'),

    object_field: $ => seq(
      field('key', choice($._word, $.string)),
      optional(seq(':', field('value', $._term))),
    ),

    // --- types ---------------------------------------------------------------

    _type: $ => choice($.type, $.record_type, $.string),

    type: $ => seq(
      field('name', $.dotted_name),
      optional(seq('(', commaSep1(field('argument', $._type)), ')')),
    ),

    record_type: $ => seq('{', commaSepTrailing($.field_declaration), '}'),

    // --- tokens --------------------------------------------------------------

    // `name(.name)*` with no spaces: a type, an extern.
    dotted_name: $ => seq(
      $._word,
      repeat(seq(token.immediate('.'), alias(token.immediate(IDENT), $.identifier))),
    ),

    _word: $ => reserved('names', choice(
      $.identifier,
      alias(choice(...STATEMENT_KEYWORDS, ...TERM_WORDS, ...CONTEXTUAL), $.identifier),
    )),

    // A name that is not a body word: where a clause may follow it.
    _name: $ => choice(
      $.identifier,
      alias(choice(...STATEMENT_KEYWORDS, ...CONTEXTUAL), $.identifier),
    ),

    identifier: _ => IDENT,

    integer: _ => /[0-9]+/,

    // A number with a unit adjacent, or with a fraction (R-66, R-62):
    // `1Gi`, `500m`, `1h30m`, `1.5Gi`; `0.5` is a float (R-75), which a
    // quantity's position reads as one.
    quantity: _ => token(choice(
      /[0-9]+(\.[0-9]+)?[A-Za-z][A-Za-z0-9]*/,
      /[0-9]+\.[0-9]+/,
    )),

    true: _ => 'true',
    false: _ => 'false',

    rank: _ => /@[a-z_]+/,

    string: $ => seq(
      '"',
      repeat(choice(
        $.string_content,
        $.escape_sequence,
        $.interpolation,
      )),
      token.immediate('"'),
    ),

    // `\` at a line end joins the line with the next (R-61).
    escape_sequence: _ => token.immediate(choice(
      /\\["\\nt]/,
      /\\\r?\n/,
      /\\u\{[0-9A-Fa-f]+\}/,
      '$${',
    )),

    // `${e}`: a hole (H-13).
    interpolation: $ => seq(token.immediate('${'), field('value', $._term), '}'),
  },
});
