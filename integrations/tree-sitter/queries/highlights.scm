; Comments
(comment) @comment

; Keywords
[
  "let"
  "param"
  "target"
  "output"
] @keyword

[
  "if"
  "then"
  "else"
] @keyword.conditional

(phase_modifier) @keyword.modifier
(protected_modifier) @keyword.modifier

; Types
(scalar_type) @type.builtin

; Literals
(string) @string
(raw_string) @string
(heredoc) @string
(escape_sequence) @string.escape
(number) @number
(boolean) @boolean
(null) @constant.builtin

; Interpolation
(interpolation
  "${" @punctuation.special
  "}" @punctuation.special)

; Statements
(let_statement
  name: (identifier) @variable)

(param_statement
  name: (identifier) @variable.parameter)

(target_statement
  name: (identifier) @function)

(output_statement
  name: (identifier) @variable)

; Blocks
(block
  name: (identifier) @variable
  provider: (identifier) @namespace
  resource: (identifier) @type)

; Field assignments
(field_assignment
  name: (identifier) @property)

(map_entry
  key: (identifier) @property)

(map_entry
  key: (string) @property)

; Function calls
(call
  name: (identifier) @function.call)

; Pipes
(pipe_expression
  pipe: (identifier) @function.call)

; References
(reference
  (identifier) @variable)

(matrix_slice_ref
  name: (identifier) @variable)

; Dotted identifiers in targets
(dotted_identifier
  (identifier) @variable)

; Punctuation
[
  "{"
  "}"
  "["
  "]"
  "("
  ")"
] @punctuation.bracket

[
  "."
  ","
  "="
  ":"
] @punctuation.delimiter

[
  "|"
  "+"
  "=="
  "!="
] @operator
