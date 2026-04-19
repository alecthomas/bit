/// <reference types="tree-sitter-cli/dsl" />
// @ts-check

module.exports = grammar({
  name: 'bit',

  extras: $ => [/[ \t\r\n]+/, $.comment],

  word: $ => $.identifier,

  externals: $ => [$._heredoc_body],

  rules: {
    source_file: $ => repeat($._statement),

    comment: $ => /#[^\n]*/,

    _statement: $ => choice(
      $.let_statement,
      $.param_statement,
      $.target_statement,
      $.output_statement,
      $.block,
    ),

    // ── Statements ──

    let_statement: $ => seq(
      'let',
      field('name', $.identifier),
      optional(seq(':', field('type', $.type))),
      '=',
      field('value', $._expression),
    ),

    param_statement: $ => seq(
      'param',
      field('name', $.identifier),
      optional(seq(':', field('type', $.type))),
      optional(seq('=', field('default', $._expression))),
    ),

    target_statement: $ => seq(
      'target',
      field('name', $.identifier),
      '=',
      '[',
      optional(seq(
        $.dotted_identifier,
        repeat(seq(',', $.dotted_identifier)),
      )),
      ']',
    ),

    output_statement: $ => seq(
      'output',
      field('name', $.identifier),
      '=',
      field('value', $._expression),
    ),

    block: $ => seq(
      optional(field('phase', $.phase_modifier)),
      optional(field('protected', $.protected_modifier)),
      field('name', $.identifier),
      optional(field('matrix_keys', $.block_matrix_keys)),
      '=',
      field('provider', $.identifier),
      optional(seq('.', field('resource', $.identifier))),
      '{',
      repeat($.field_assignment),
      '}',
    ),

    phase_modifier: _ => choice('pre', 'post'),
    protected_modifier: _ => 'protected',

    block_matrix_keys: $ => seq(
      '[',
      $.identifier,
      repeat(seq(',', $.identifier)),
      ']',
    ),

    field_assignment: $ => seq(
      field('name', $.identifier),
      '=',
      field('value', $._expression),
      optional(','),
    ),

    dotted_identifier: $ => seq(
      $.identifier,
      repeat(seq('.', $.identifier)),
    ),

    // ── Types ──

    type: $ => prec.left(seq(
      $._type_atom,
      repeat(seq('|', $._type_atom)),
    )),

    _type_atom: $ => choice($.list_type, $.map_type, $.scalar_type),

    list_type: $ => seq('[', $.type, ']'),

    map_type: $ => seq('{', 'string', '=', $.type, '}'),

    scalar_type: _ => choice('string', 'number', 'bool', 'path', 'secret'),

    // ── Expressions ──

    _expression: $ => choice(
      $.if_expression,
      $._add_expression,
    ),

    if_expression: $ => prec.right(seq(
      'if', field('condition', $._expression),
      'then', field('then', $._expression),
      'else', field('else', $._expression),
    )),

    _add_expression: $ => choice(
      $.add_expression,
      $._pipe_expression,
    ),

    add_expression: $ => prec.left(1, seq(
      field('left', $._add_expression),
      '+',
      field('right', $._pipe_expression),
    )),

    _pipe_expression: $ => choice(
      $.pipe_expression,
      $._cmp_expression,
    ),

    pipe_expression: $ => prec.left(2, seq(
      field('value', $._pipe_expression),
      '|',
      field('pipe', $.identifier),
      optional(seq(
        '(',
        optional(seq(
          $._expression,
          repeat(seq(',', $._expression)),
        )),
        ')',
      )),
    )),

    _cmp_expression: $ => choice(
      $.binary_expression,
      $._primary,
    ),

    binary_expression: $ => prec.left(3, seq(
      field('left', $._primary),
      field('op', choice('==', '!=')),
      field('right', $._primary),
    )),

    _primary: $ => choice(
      $.string,
      $.raw_string,
      $.heredoc,
      $.number,
      $.boolean,
      $.null,
      $.list,
      $.map,
      $.call,
      $.reference,
    ),

    // ── Strings ──

    string: $ => seq(
      '"',
      repeat(choice(
        $._string_content,
        $._bare_dollar,
        $.escape_sequence,
        $.interpolation,
      )),
      '"',
    ),

    _string_content: _ => token.immediate(prec(1, /[^"\\$]+/)),

    _bare_dollar: _ => token.immediate('$'),

    escape_sequence: _ => token.immediate(/\\./),

    interpolation: $ => seq(
      token.immediate('${'),
      $._expression,
      '}',
    ),

    raw_string: _ => seq("'", /[^']*/, "'"),

    heredoc: $ => seq(
      $._heredoc_start,
      $._heredoc_body,
    ),

    _heredoc_start: _ => token(seq('<<', optional('-'))),

    // ── Scalars ──

    number: _ => /\d+(\.\d+)?/,

    boolean: _ => choice('true', 'false'),

    null: _ => 'null',

    // ── Collections ──

    list: $ => seq(
      '[',
      optional(seq(
        $._expression,
        repeat(seq(',', $._expression)),
      )),
      ']',
    ),

    map: $ => seq(
      '{',
      optional(seq(
        $.map_entry,
        repeat(seq(',', $.map_entry)),
      )),
      '}',
    ),

    map_entry: $ => seq(
      field('key', choice($.identifier, $.string, $.raw_string)),
      '=',
      field('value', $._expression),
    ),

    // ── Calls & References ──

    call: $ => seq(
      field('name', $.identifier),
      '(',
      optional(seq(
        $._expression,
        repeat(seq(',', $._expression)),
      )),
      ')',
    ),

    reference: $ => seq(
      choice($.matrix_slice_ref, $.identifier),
      repeat(seq('.', $.identifier)),
    ),

    matrix_slice_ref: $ => seq(
      field('name', $.identifier),
      '[',
      $._matrix_key,
      repeat(seq(',', $._matrix_key)),
      ']',
    ),

    _matrix_key: $ => choice($.identifier, $.string, $.raw_string),

    // ── Identifier ──

    identifier: _ => /[A-Za-z_][A-Za-z0-9_-]*/,
  },
});
