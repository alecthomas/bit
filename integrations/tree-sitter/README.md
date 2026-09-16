# tree-sitter-bit

Tree-sitter grammar for the [bit](https://github.com/alecthomas/bit) build language.

## Usage

```sh
tree-sitter generate
tree-sitter test
tree-sitter parse path/to/BUILD.bit
tree-sitter highlight path/to/BUILD.bit
```

## Files

- `grammar.js` — grammar definition
- `src/scanner.c` — external scanner for heredoc bodies (dynamic terminator)
- `queries/` — syntax highlighting, injections, locals
- `test/corpus/` — parser tests

## Coverage

Covers all language constructs in `SPEC.md`:

- Statements: `let`, `param`, `target`, `output`, blocks
- Block modifiers: `pre`, `post`, `protected`, matrix `name[key]`
- Types: scalar, list, map, union
- Expressions: literals, strings (with `${}` interpolation and escapes),
  raw strings, heredocs (`<<EOF` / `<<-EOF`), lists, maps, function
  calls, pipes, `if/then/else`, comparison, list concatenation
- References: dotted, matrix slices with identifier or string keys
- Comments: `#` to end of line (attached as nodes, skipped as extras)
