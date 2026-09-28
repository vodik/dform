/**
 * @file The dform grammar for editors (docs/grammar.md).
 *
 * The compiler's parser is src/syntax/parser.rs; this grammar is for
 * highlighting, indentation and navigation, and tests/treesit_agreement.rs
 * holds the two to the same corpus. A newline outside brackets ends a
 * statement, a block entry or a literal of a `{ }` body: the external
 * scanner (src/scanner.c) emits it only where the grammar can take one, so
 * everywhere else it is whitespace, as in the compiler.
 *
 * One consequence: where a statement cannot end yet, a newline is
 * whitespace here but an end to the compiler, so a line broken before an
 * operator (`r.tags` then `= {}` on the next line, `x` then `in xs`) is an
 * error only in the compiler's diagnostics.
 */

/// <reference types="tree-sitter-cli/dsl" />
// @ts-check

const IDENT = /[A-Za-z_][A-Za-z0-9_]*/;

// Keywords (docs/grammar.md "Tokens"). Anywhere a plain name is expected a
// keyword is a name; `_word` says so where a keyword could otherwise start
// its own construct.
const KEYWORDS = [
  'edition', 'provider', 'stack', 'import', 'input', 'output', 'export',
  'contributes', 'module', 'instance', 'policy', 'apply', 'resource',
  'settings', 'scenario', 'extern', 'type', 'decl', 'when', 'not', 'in',
  'exists', 'persist', 'where', 'if', 'for', 'let', 'has', 'some', 'with',
  'deny', 'warn', 'constraint',
];

// Contextual words: keywords only where their construct is expected.
const CONTEXTUAL = [
  'relation', 'from', 'mixed', 'open', 'ordered', 'by', 'as',
  'required', 'computed', 'id', 'sensitive', 'nullable',
];

// Keywords that may start a chain in a term (parser.rs `term_name`).
const TERM_KEYWORDS = KEYWORDS.filter(k => ![
  'not', 'in', 'if', 'for', 'has', 'some', 'exists', 'where',
].includes(k));

const PREC = {
  add: 1,
  mul: 2,
  unary: 3,
};

/**
 * Items separated by `sep`, with any number of separators before, between
 * and after them.
 *
 * @param {RuleOrLiteral} item
 * @param {RuleOrLiteral} sep
 * @returns {SeqRule}
 */
function lines(item, sep) {
  return seq(repeat(sep), repeat(seq(item, repeat1(sep))), optional(item));
}

/**
 * Entries of a block or a `{ }` body: separated by a newline or a comma,
 * with blank lines anywhere and a trailing comma allowed.
 *
 * @param {GrammarSymbols<string>} $
 * @param {RuleOrLiteral} item
 * @returns {SeqRule}
 */
function entries($, item) {
  const sep = choice(seq(',', repeat($._newline)), repeat1($._newline));
  return seq(repeat($._newline), repeat(seq(item, sep)), optional(item));
}

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

  // Keywords with a construct of their own in a term are never a name
  // there (parser.rs `term_name`); every keyword is a name where only a
  // name can go (`_word`).
  reserved: {
    global: _ => ['not', 'in', 'if', 'for', 'has', 'some', 'exists', 'where', 'true', 'false', 'null'],
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
    source_file: $ => lines($._statement, $._newline),

    comment: _ => token(seq(choice('#', '//'), /[^\n]*/)),

    _statement: $ => choice(
      $.edition,
      $.provider,
      $.stack,
      $.import,
      $.input,
      $.input_relation,
      $.output,
      $.export,
      $.contributes,
      $.extern,
      $.type_declaration,
      $.decl,
      $.let,
      $.module,
      $.instance,
      $.policy,
      $.apply,
      $.scenario,
      $.when,
      $.for,
      $.with,
      $.resource,
      $.settings,
      $.check,
      $.contribution,
      $.value_rule,
      $.rule,
      $.fact,
    ),

    edition: $ => seq('edition', field('version', $.integer)),

    provider: $ => seq('provider', field('name', $._word), field('body', $.block)),

    stack: $ => seq(
      'stack',
      field('name', $.dotted_name),
      optional(seq('[', commaSep1(field('key', $.identifier)), ']')),
      field('body', $.block),
    ),

    import: $ => seq('import', field('path', $.string), optional(seq('as', field('alias', $._word)))),

    input: $ => seq(
      'input',
      field('name', $._word),
      ':',
      field('type', $._type),
      optional(seq('=', field('default', $._term))),
      optional($.where_clause),
    ),

    // `p/N` (facts from a .df file) or `p(col: type, ...)` (a table).
    input_relation: $ => seq(
      'input',
      'relation',
      field('name', $._word),
      choice(
        seq('/', field('arity', $.integer)),
        seq('(', commaSep1($.field_declaration), optional(','), ')'),
      ),
      'from',
      field('source', $._term),
    ),

    output: $ => seq(
      'output',
      field('name', $._word),
      choice(seq(':', field('type', $._type)), seq('=', field('value', $._term))),
    ),

    export: $ => seq('export', field('name', $._word), '/', field('arity', $.integer)),

    contributes: $ => seq('contributes', field('grant', $._chain)),

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

    attribute_block: $ => seq('{', entries($, $.attribute_declaration), '}'),

    attribute_declaration: $ => seq(
      field('path', $.block_path),
      ':',
      choice(
        field('body', $.attribute_block),
        seq(
          // A `{` here opens a nested attribute block, never a record type.
          field('type', choice($.type, $.string)),
          repeat($.flag),
          optional($.where_clause),
        ),
      ),
    ),

    flag: _ => choice('required', 'computed', 'id', 'sensitive', 'nullable'),

    decl: $ => seq('decl', choice(
      seq(field('name', $.dotted_name), '/', field('arity', $.integer), optional('mixed')),
      seq(field('name', $.dotted_name), '(', commaSep1($.field_declaration), optional(','), ')'),
      seq('type', field('type', $.dotted_name), 'open'),
    )),

    field_declaration: $ => seq(field('name', $._word), ':', field('type', $._type)),

    let: $ => seq('let', field('name', $._word), '=', field('value', $._term)),

    module: $ => seq('module', field('name', $._word), field('body', $.statement_block)),

    policy: $ => seq('policy', field('name', $._word), field('body', $.statement_block)),

    scenario: $ => seq('scenario', field('name', $._word), field('body', $.statement_block)),

    instance: $ => seq(
      'instance',
      field('module', $._word),
      field('name', $._word),
      field('body', $.block),
    ),

    apply: $ => seq('apply', field('name', $._word)),

    when: $ => seq('when', field('condition', $.body), field('body', $.statement_block)),

    for: $ => seq('for', field('condition', $.body), field('body', $.statement_block)),

    with: $ => seq('with', field('name', $._word), '=', field('value', $._term)),

    statement_block: $ => seq('{', lines($._statement, $._newline), '}'),

    resource: $ => seq(
      'resource',
      field('type', $.dotted_name),
      field('name', choice($._word, $.string)),
      optional(field('rank', $.rank)),
      field('body', $.block),
    ),

    settings: $ => seq(
      'settings',
      field('name', choice($._word, $.string)),
      optional(field('rank', $.rank)),
      field('body', $.block),
    ),

    // A resource's, settings row's, instance's, provider's or stack's
    // entries: clauses, then fields.
    block: $ => seq('{', entries($, choice($.clause, $.field)), '}'),

    clause: $ => seq(choice('for', 'if'), field('condition', $.body)),

    field: $ => seq(
      field('path', $.block_path),
      field('operator', choice('=', '+=')),
      field('value', $._term),
      optional(field('rank', $.rank)),
    ),

    // A keypath without its dot: `a.b[0]."c-d"`.
    block_path: $ => seq(
      choice($._field_word, $.string),
      repeat(choice(
        seq(token.immediate('.'), choice($._word, $.string)),
        seq(token.immediate('['), $.integer, ']'),
      )),
    ),

    check: $ => seq(
      field('kind', choice('deny', 'warn', 'constraint')),
      field('message', $.string),
      optional(field('details', $.object)),
      optional(seq('if', field('condition', $._body))),
    ),

    contribution: $ => seq(
      field('target', choice($.member_expression, $.index_expression, $.instance_expression)),
      field('operator', choice('=', '+=')),
      field('value', $._term),
      optional(field('rank', $.rank)),
      optional(seq('if', field('condition', $._body))),
    ),

    value_rule: $ => seq(
      field('name', $.identifier),
      field('operator', choice('=', '+=')),
      field('value', $._term),
      optional(field('rank', $.rank)),
      optional(seq('if', field('condition', $._body))),
    ),

    rule: $ => seq(
      field('head', choice($.call, $.record)),
      optional(field('rank', $.rank)),
      'if',
      field('body', $._body),
    ),

    fact: $ => seq(
      field('head', choice($.call, $.record)),
      optional(field('rank', $.rank)),
    ),

    where_clause: $ => seq('where', field('condition', $.body)),

    // --- bodies and literals -------------------------------------------------

    _body: $ => choice($.body, $.body_block),

    body: $ => prec.right(commaSep1($._literal)),

    body_block: $ => seq('{', entries($, $._literal), '}'),

    _literal: $ => choice(
      $.not_literal,
      $.not_block,
      $.exists_literal,
      $.has_literal,
      $.some_literal,
      $.comparison,
      $.in_literal,
      $.not_in_literal,
      $.atom_literal,
      $.truth_literal,
    ),

    not_literal: $ => seq('not', $._literal),

    not_block: $ => prec(1, seq('not', $.body_block)),

    exists_literal: $ => seq('exists', $._term),

    has_literal: $ => seq('has', $._term),

    some_literal: $ => seq(
      'some',
      field('index', $._term),
      optional(seq(',', field('element', $._term))),
      'in',
      field('collection', $._term),
    ),

    comparison: $ => seq($._term, repeat1(seq(field('operator', choice('=', '==', '!=', '<', '<=', '>', '>=')), $._term))),

    // `x in resource` is any resource; `resource` is not a name there.
    in_literal: $ => choice(
      prec(1, seq(field('element', $._term), 'in', field('collection', alias('resource', $.any_resource)))),
      seq(field('element', $._term), 'in', field('collection', $._term)),
    ),

    not_in_literal: $ => seq(field('element', $._term), 'not', 'in', field('collection', $._term)),

    atom_literal: $ => choice($.call, $.record),

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
      $.null,
      $.keypath,
      $._chain,
      $.call,
      $.record,
      $.list,
      $.comprehension,
      $.object,
      $.parenthesized,
    ),

    parenthesized: $ => seq('(', $._term, ')'),

    // `name (.seg | [terms] | /name)*`, each part glued to the last.
    _chain: $ => choice(
      $._chain_head,
      $.member_expression,
      $.index_expression,
      $.instance_expression,
    ),

    _chain_head: $ => choice(
      $.identifier,
      alias(choice(...TERM_KEYWORDS), $.identifier),
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

    // `m.i/n`: the `/` glued on both sides.
    instance_expression: $ => seq(
      field('instance', $._chain),
      field('name', alias(token.immediate(seq('/', IDENT)), $.instance_name)),
    ),

    call: $ => seq(
      field('function', $._chain),
      token.immediate('('),
      field('arguments', optional(alias($._arguments, $.arguments))),
      ')',
    ),

    _arguments: $ => seq(commaSep1($._term), optional(',')),

    record: $ => seq(
      field('name', $._chain_head),
      token.immediate('{'),
      commaSepTrailing($.record_field),
      '}',
    ),

    record_field: $ => seq(field('key', $._word), ':', field('value', $._term)),

    list: $ => seq('[', commaSepTrailing($._term), ']'),

    comprehension: $ => seq(
      '[',
      field('item', $._term),
      optional(seq('ordered', 'by', field('order', $._term))),
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
      optional(seq(token.immediate('('), commaSep1(field('argument', $._type)), ')')),
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
      alias(choice(...KEYWORDS, ...CONTEXTUAL), $.identifier),
    )),

    // A field's first segment: `for` and `if` start a clause there.
    _field_word: $ => reserved('names', choice(
      $.identifier,
      alias(choice(...KEYWORDS.filter(k => k !== 'for' && k !== 'if'), ...CONTEXTUAL), $.identifier),
    )),

    identifier: _ => IDENT,

    integer: _ => /[0-9]+/,

    true: _ => 'true',
    false: _ => 'false',
    null: _ => 'null',

    rank: _ => /@[a-z_]+/,

    // `.tags."a.b"[0]`: a keypath literal, one token.
    keypath: _ => token(seq(
      '.',
      choice(IDENT, /"([^"\\\n]|\\.)*"/),
      repeat(choice(seq('.', choice(IDENT, /"([^"\\\n]|\\.)*"/)), /\[[0-9]+\]/)),
    )),

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
      '{{',
      '}}',
    )),

    interpolation: $ => seq(token.immediate('{'), field('value', $._term), '}'),
  },
});
