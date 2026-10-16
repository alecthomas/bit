//! The import graph of `.proto` files, read with a minimal tokenizer rather
//! than a full parser: only `import` statements matter here.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use crate::provider::BoxError;

/// Files reachable from a set of sources through their imports.
#[derive(Debug)]
pub(super) struct ImportGraph {
    /// Each reachable file with the files its imports resolve to. Imports
    /// that resolve under no root, such as the well-known types protoc
    /// bundles, are omitted.
    imports: BTreeMap<PathBuf, Vec<PathBuf>>,
}

impl ImportGraph {
    /// Follow imports from `sources`, resolving each against `roots` in
    /// order, as protoc does with its `-I` paths. Paths must be normalized
    /// the same way as `roots` so a file reached twice is one node.
    pub(super) fn build(sources: &[PathBuf], roots: &[PathBuf]) -> Result<Self, BoxError> {
        let mut imports = BTreeMap::new();
        let mut pending = sources.to_vec();
        while let Some(file) = pending.pop() {
            if imports.contains_key(&file) {
                continue;
            }
            let text = std::fs::read_to_string(&file).map_err(|e| format!("failed to read {}: {e}", file.display()))?;
            let resolved: Vec<PathBuf> = parse_imports(&text)
                .iter()
                .filter_map(|import| resolve_import(import, roots))
                .collect();
            pending.extend(resolved.iter().filter(|path| !imports.contains_key(*path)).cloned());
            imports.insert(file, resolved);
        }
        Ok(Self { imports })
    }

    /// Every reachable file, sources included.
    pub(super) fn files(&self) -> Vec<PathBuf> {
        self.imports.keys().cloned().collect()
    }

    /// The `sources` that are, or transitively import, a file in `changed`.
    pub(super) fn affected(&self, sources: &[PathBuf], changed: &BTreeSet<PathBuf>) -> Vec<PathBuf> {
        let mut importers: BTreeMap<&Path, Vec<&Path>> = BTreeMap::new();
        for (file, imports) in &self.imports {
            for import in imports {
                importers.entry(import).or_default().push(file);
            }
        }
        let mut reached = BTreeSet::new();
        let mut pending: Vec<&Path> = changed.iter().map(PathBuf::as_path).collect();
        while let Some(file) = pending.pop() {
            if reached.insert(file) {
                pending.extend(importers.get(file).into_iter().flatten());
            }
        }
        sources
            .iter()
            .filter(|source| reached.contains(source.as_path()))
            .cloned()
            .collect()
    }
}

fn resolve_import(import: &str, roots: &[PathBuf]) -> Option<PathBuf> {
    roots
        .iter()
        .map(|root| super::normalize(&root.join(import)))
        .find(|path| path.is_file())
}

/// The paths named by `import`, `import public`, and `import weak`
/// statements in `source`, in order.
fn parse_imports(source: &str) -> Vec<String> {
    let mut tokens = Tokens { rest: source }.peekable();
    let mut imports = Vec::new();
    while let Some(token) = tokens.next() {
        if token != Token::Ident("import") {
            continue;
        }
        if matches!(tokens.peek(), Some(Token::Ident("public" | "weak"))) {
            tokens.next();
        }
        // Adjacent string literals concatenate, as in C.
        let mut path = None;
        while let Some(Token::Str(part)) = tokens.next_if(|token| matches!(token, Token::Str(_))) {
            path.get_or_insert_with(String::new).push_str(&part);
        }
        // Anything else, such as a field named `import`, is not a statement.
        if let Some(path) = path
            && tokens.peek() == Some(&Token::Other(';'))
        {
            imports.push(path);
        }
    }
    imports
}

#[derive(Debug, PartialEq)]
enum Token<'a> {
    Ident(&'a str),
    Str(String),
    Other(char),
}

/// Identifiers, string literals, and single punctuation characters, with
/// whitespace and comments skipped.
struct Tokens<'a> {
    rest: &'a str,
}

impl<'a> Iterator for Tokens<'a> {
    type Item = Token<'a>;

    fn next(&mut self) -> Option<Token<'a>> {
        loop {
            self.rest = self.rest.trim_start();
            if let Some(comment) = self.rest.strip_prefix("//") {
                self.rest = comment.split_once('\n').map_or("", |(_, rest)| rest);
            } else if let Some(comment) = self.rest.strip_prefix("/*") {
                self.rest = comment.split_once("*/").map_or("", |(_, rest)| rest);
            } else {
                break;
            }
        }

        let mut chars = self.rest.char_indices();
        let (_, first) = chars.next()?;
        if first == '"' || first == '\'' {
            let mut value = String::new();
            let mut escaped = false;
            for (index, c) in chars {
                if escaped {
                    value.push(c);
                    escaped = false;
                } else if c == '\\' {
                    escaped = true;
                } else if c == first || c == '\n' {
                    self.rest = &self.rest[index + c.len_utf8()..];
                    return Some(Token::Str(value));
                } else {
                    value.push(c);
                }
            }
            self.rest = "";
            return Some(Token::Str(value));
        }
        if first.is_ascii_alphabetic() || first == '_' {
            let end = self
                .rest
                .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .unwrap_or(self.rest.len());
            let (ident, rest) = self.rest.split_at(end);
            self.rest = rest;
            return Some(Token::Ident(ident));
        }
        self.rest = &self.rest[first.len_utf8()..];
        Some(Token::Other(first))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_imports_reads_every_import_form() {
        let source = r#"
            syntax = "proto3";
            import "a.proto";
            import public 'b.proto';
            import weak "c" ".proto";
            package acme.v1;
        "#;
        assert_eq!(parse_imports(source), ["a.proto", "b.proto", "c.proto"]);
    }

    #[test]
    fn parse_imports_ignores_comments_strings_and_fields() {
        let source = r#"
            // import "line.proto";
            /* import "block.proto"; */
            option (doc) = "import \"string.proto\";";
            message M { string import = 1; }
            import "real.proto"; // trailing
        "#;
        assert_eq!(parse_imports(source), ["real.proto"]);
    }

    fn write(path: &Path, text: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    #[test]
    fn graph_follows_imports_through_roots_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(
            &root.join("proto/a.proto"),
            r#"import "b.proto"; import "google/protobuf/any.proto";"#,
        );
        write(&root.join("proto/b.proto"), r#"import "c.proto";"#);
        write(&root.join("proto/c.proto"), "");
        write(&root.join("vendor/c.proto"), "");
        write(&root.join("proto/unrelated.proto"), "");
        let roots = [root.join("proto"), root.join("vendor")];
        let graph = ImportGraph::build(&[root.join("proto/a.proto")], &roots).unwrap();
        assert_eq!(
            graph.files(),
            [
                root.join("proto/a.proto"),
                root.join("proto/b.proto"),
                root.join("proto/c.proto")
            ]
        );
    }

    #[test]
    fn affected_includes_transitive_importers() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("proto");
        write(&root.join("a.proto"), r#"import "b.proto";"#);
        write(&root.join("b.proto"), r#"import "c.proto";"#);
        write(&root.join("c.proto"), "");
        write(&root.join("d.proto"), "");
        let sources: Vec<PathBuf> = ["a", "b", "c", "d"]
            .iter()
            .map(|name| root.join(format!("{name}.proto")))
            .collect();
        let graph = ImportGraph::build(&sources, std::slice::from_ref(&root)).unwrap();

        let changed = BTreeSet::from([root.join("c.proto")]);
        assert_eq!(graph.affected(&sources, &changed), &sources[..3]);
        let changed = BTreeSet::from([root.join("d.proto")]);
        assert_eq!(graph.affected(&sources, &changed), &sources[3..]);
    }
}
