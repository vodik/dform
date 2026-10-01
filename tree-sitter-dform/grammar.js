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
  'edition', 'import', 'provider', 'stack', 'type', 'decl', 'extern',
  'input', 'output', 'let', 'set', 'export', 'module', 'instance',
  'policy', 'use', 'scenario', 'resource', 'settings', 'deny', 'warn',
];

// The body words, the clause word, the reserved `if` and the literals:
// never a name in a term.
const TERM_WORDS = ['not', 'in', 'has', 'where', 'if', 'true', 'false'];

// Contextual words: plain names but where their construct is expected.
const CONTEXTUAL = [
  'from', 'mixed', 'persist', 'check',
  'required', 'computed', 'id', 'sensitive', 'nullable',
];

// Words that may start a chain in a term (parser.rs `term_name`).
const TERM_NAMES = [...STATEMENT_KEYWORDS, ...CONTEXTUAL];

const PREC = {
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
  ],

  rules: {
    source_file: $ => seq(
      repeat($._newline),
      repeat(seq($._statement, repeat1($._newline))),
      optional($._statement),
    ),

    comment: _ => token(seq('#', /[^\n]*/)),

    _statement: $ => choice(
      $.edition,
      $.import,
      $.provider,
      $.stack,
      $.input,
      $.input_relation,
      $.output,
      $.let,
      $.set,
      $.export,
      $.extern,
      $.type_declaration,
      $.type_alias,
      $.decl,
      $.module,
      $.instance,
      $.policy,
      $.use,
      $.scenario,
      $.resource,
      $.settings,
      $.check,
      $.rule,
      $.fact,
    ),

    edition: $ => seq('edition', field('version', $.integer)),

    import: $ => seq('import', field('path', $.string)),

    // A provider's or stack's block takes no clause; the compiler's
    // resolver says so, so the grammar takes one as the parser does.
    provider: $ => seq(
      'provider',
      field('name', $._word),
      field('body', $.block),
      optional($.clause),
    ),

    stack: $ => seq(
      'stack',
      field('name', $.dotted_name),
      optional(seq('[', commaSep1(field('key', $._word)), ']')),
      field('body', $.block),
      optional($.clause),
    ),

    input: $ => seq(
      'input',
      field('name', $._word),
      ':',
      field('type', $._type),
      optional(seq('=', field('default', $._term))),
      optional($.refinement),
    ),

    // `input p(cols) from SOURCE`: a relation the world gives.
    input_relation: $ => seq(
      'input',
      field('name', $._word),
      $.columns,
      'from',
      field('source', $._term),
    ),

    // `output k: T = t [where B]`: one statement.
    output: $ => seq(
      'output',
      field('name', $._word),
      optional(seq(':', field('type', $._type))),
      optional(seq('=', field('value', $._term))),
      optional(seq('where', field('condition', $._body))),
    ),

    // `let k = t [where B]`: a value.
    let: $ => seq(
      'let',
      field('name', $._word),
      '=',
      field('value', $._term),
      optional(seq('where', field('condition', $._body))),
    ),

    // `set r.p = t [@rank] [where B]`: a contribution.
    set: $ => seq(
      'set',
      field('target', $._chain),
      field('operator', choice('=', '+=')),
      field('value', $._term),
      optional(field('rank', $.rank)),
      optional(seq('where', field('condition', $._body))),
    ),

    // `export type NAME`: a module's alias, for its importers (a relation
    // is not exported, R-5).
    export: $ => seq('export', 'type', field('type', $._word)),

    extern: $ => seq(
      'extern',
      field('name', $.dotted_name),
      '(',
      commaSep1($.bind_arg),
      ')',
      optional('persist'),
    ),

    bind_arg: $ => seq(
      field('mode', choice('+', '-')),
      field('name', $._word),
      optional(seq(':', field('type', $._type))),
    ),

    type_declaration: $ => seq('type', field('name', $.dotted_name), field('body', $.attribute_block)),

    // `type NAME = TYPE`: a transparent alias.
    type_alias: $ => seq('type', field('name', $.identifier), '=', field('type', $._type)),

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

    module: $ => seq('module', field('name', $._word), field('body', $.statement_block)),

    policy: $ => seq('policy', field('name', $._word), field('body', $.statement_block)),

    scenario: $ => seq('scenario', field('name', $._word), field('body', $.statement_block)),

    instance: $ => seq(
      'instance',
      field('module', $._word),
      field('name', $._word),
      field('body', $.block),
      optional($.clause),
    ),

    use: $ => seq('use', field('name', $._word)),

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

    settings: $ => seq(
      'settings',
      field('name', choice($._word, $.string)),
      optional(field('rank', $.rank)),
      field('body', $.block),
      optional($.clause),
    ),

    // A resource's, settings row's, instance's, provider's or stack's
    // entries, separated by a newline or a comma. Its clause follows it.
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

    field: $ => seq(
      field('path', $.block_path),
      field('operator', choice('=', '+=')),
      field('value', $._term),
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

    truth_literal: $ => $._chain,

    // --- terms ---------------------------------------------------------------

    _term: $ => choice(
      $.binary_expression,
      $.unary_expression,
      $._primary,
    ),

    binary_expression: $ => choice(
      prec.left(PREC.add, seq(field('left', $._term), field('operator', choice('+', '-')), field('right', $._term))),
      prec.left(PREC.mul, seq(field('left', $._term), field('operator', choice('*', '/', '%')), field('right', $._term))),
    ),

    unary_expression: $ => prec(PREC.unary, seq('-', $._term)),

    _primary: $ => choice(
      $.integer,
      $.string,
      $.true,
      $.false,
      $._chain,
      $.call,
      $.list,
      $.comprehension,
      $.object,
      $.parenthesized,
    ),

    parenthesized: $ => seq('(', $._term, ')'),

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
      commaSep1(field('index', $._term)),
      ']',
    ),

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
      field('condition', $.body),
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

    // `name(.name)*` with no spaces: a type, a stack, an extern.
    dotted_name: $ => seq(
      $._word,
      repeat(seq(token.immediate('.'), alias(token.immediate(IDENT), $.identifier))),
    ),

    _word: $ => reserved('names', choice(
      $.identifier,
      alias(choice(...STATEMENT_KEYWORDS, ...TERM_WORDS, ...CONTEXTUAL), $.identifier),
    )),

    identifier: _ => IDENT,

    integer: _ => /[0-9]+/,

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

    escape_sequence: _ => token.immediate(choice(
      /\\["\\nt]/,
      /\\u\{[0-9A-Fa-f]+\}/,
      '$${',
    )),

    // `${e}`: a hole (H-13).
    interpolation: $ => seq(token.immediate('${'), field('value', $._term), '}'),
  },
});
