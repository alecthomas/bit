; Scopes
(source_file) @local.scope
(target) @local.scope

; Definitions
(let_statement name: (identifier) @local.definition.var)
(param_statement name: (identifier) @local.definition.parameter)
(group_statement name: (identifier) @local.definition.function)
(output_statement name: (identifier) @local.definition.var)
(target name: (identifier) @local.definition.var)
(target_matrix_keys (identifier) @local.definition.var)

; References
(reference (identifier) @local.reference)
(matrix_slice_ref name: (identifier) @local.reference)
