; Comments
(comment) @comment

; Keywords
[
  "import"
  "as"
  "let"
  "param"
  "group"
  "output"
] @keyword

[
  "if"
  "then"
  "else"
] @keyword

(phase_modifier) @keyword
(protected_modifier) @keyword
(explicit_modifier) @keyword
(large_modifier) @keyword

; Types
(scalar_type) @type.builtin

; Literals
(string) @string
(raw_string) @string
(heredoc_body) @string
(escape_sequence) @string.escape
(number) @number
(duration) @number
(boolean) @boolean
(null) @constant.builtin

; Interpolation markers
(interpolation
  "#{" @punctuation.special
  "}" @punctuation.special)

; Statement names
(import_statement
  alias: (identifier) @type)

(let_statement
  name: (identifier) @variable)

(param_statement
  name: (identifier) @variable)

(parameter
  name: (identifier) @variable)

(group_statement
  name: (identifier) @function)

(output_statement
  name: (identifier) @variable)

; Targets: name = provider.resource
(target
  name: (identifier) @variable
  provider: (identifier) @type
  resource: (identifier) @function)

; Field assignments
(field_assignment
  name: (identifier) @property)

(named_argument
  name: (identifier) @property)

(map_entry
  key: (identifier) @property)

(map_entry
  key: (string) @property)

; Function calls
(target_call
  name: (identifier) @variable)

(call
  name: (identifier) @function)

(call
  name: (provider_function
    (identifier) @type
    (identifier) @function))

; Pipes
(pipe_expression
  pipe: (identifier) @function)

; References
(reference
  (identifier) @variable)

(matrix_slice_ref
  name: (identifier) @variable)

(matrix_key_binding
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
