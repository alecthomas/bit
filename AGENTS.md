# AGENTS.md

## Editor integrations must track the bit parser

The tree-sitter grammar at `integrations/tree-sitter/` and the Zed extension at `integrations/zed/` are independent implementations / consumers of the bit language. They must stay in sync with the canonical parser at `src/parser.rs`.

When you change the bit grammar — adding/removing/renaming statements, keywords, expression forms, or any user-visible syntax — you must also:

1. Update `integrations/tree-sitter/grammar.js` to match.
2. Update `integrations/tree-sitter/queries/highlights.scm` for any new keywords or named nodes that need a capture.
3. Add or update test cases under `integrations/tree-sitter/test/corpus/`.
4. Regenerate the parser: `cd integrations/tree-sitter && tree-sitter generate`. This rewrites `src/grammar.json`, `src/node-types.json`, and `src/parser.c` — commit them.
5. Verify: `tree-sitter test` must report 100% success.
6. Mirror the highlight changes in `integrations/zed/languages/bit/highlights.scm` (Zed uses its own captures; don't just copy the tree-sitter file). Add the same keywords and any new node captures.

The Zed extension's `grammars.bit.rev` pin in `integrations/zed/extension.toml` is auto-synced to the latest tree-sitter-touching commit by the `sync-zed-rev` target in `BUILD.bit` (runs as part of the default `bit` target). Don't edit it manually.

A grammar change without these updates breaks editor highlighting and structural editing for every bit user.

## README must track the implementation

`README.md` is user-facing documentation and must stay in sync with the code. When you change behaviour, CLI flags, syntax, or semantics, update the README in the same change.

The `## Providers` section is the exception: it is auto-generated from the live provider schemas by the `sync-readme` target in `BUILD.bit` (which shells out to `bit --schema --json | jq …`). Run `bit sync-readme` (or just `bit`, since `sync-readme` is in the default target) after touching any provider or module schema; don't hand-edit that section.
