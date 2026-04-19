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
] @keyword

(phase_modifier) @keyword
(protected_modifier) @keyword

; Types
(scalar_type) @type.builtin

; Literals
(string) @string
(raw_string) @string
(heredoc) @string
(escape_sequence) @string.escape
(number) @number
(duration) @number
(boolean) @boolean
(null) @constant.builtin

; Interpolation markers
(interpolation
  "${" @punctuation.special
  "}" @punctuation.special)

; Statement names
(let_statement
  name: (identifier) @variable)

(param_statement
  name: (identifier) @variable)

(target_statement
  name: (identifier) @function)

(output_statement
  name: (identifier) @variable)

; Blocks: name = provider.resource
(block
  name: (identifier) @variable
  provider: (identifier) @type
  resource: (identifier) @function)

; Field assignments
(field_assignment
  name: (identifier) @property)

(map_entry
  key: (identifier) @property)

(map_entry
  key: (string) @property)

; Function calls
(call
  name: (identifier) @function)

; Pipes
(pipe_expression
  pipe: (identifier) @function)

; References
(reference
  (identifier) @variable)

(matrix_slice_ref
  name: (identifier) @variable)

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
