//! Checks block fields against the input schema of the block's resource.
//!
//! serde ignores unknown keys when it turns evaluated fields into a
//! resource's typed inputs, and reports other mismatches only once the block
//! runs. The loader therefore checks every block's field expressions against
//! the schema, without evaluating them, because evaluation can run commands.
//! Literals and `let` or `param` values are checked exactly. A value that is
//! only known once upstream blocks run is checked against the type its source
//! declares, if any, so the engine checks the evaluated inputs again just
//! before the block runs.

use std::collections::HashMap;

use crate::ast::{Expr, Field, StringPart};
use crate::dag::{CONCURRENCY_FIELD, DIR_FIELD, Dag, ENGINE_FIELDS, UNCACHED_FIELD};
use crate::expr::{self, Scope, SymbolKind};
use crate::loader::LoadError;
use crate::value::{Map, StructType, Type, parse_duration_literal};

/// A block field that does not fit its resource's input schema. Each `path`
/// names the field, with nested keys and list indices, as in `flags[0]`.
#[derive(Debug, thiserror::Error)]
pub enum InputError {
    #[error("unknown field '{path}'{}; valid fields: {}", did_you_mean(.suggestion), .valid.join(", "))]
    UnknownField {
        path: String,
        suggestion: Option<String>,
        valid: Vec<String>,
    },
    #[error("missing required field '{path}'")]
    MissingField { path: String },
    #[error("field '{path}': expected {expected}, got {actual}")]
    Mismatch {
        path: String,
        expected: String,
        actual: String,
    },
    #[error("field '{path}': {message}")]
    Invalid { path: String, message: String },
}

impl InputError {
    fn path(&self) -> &str {
        match self {
            InputError::UnknownField { path, .. }
            | InputError::MissingField { path }
            | InputError::Mismatch { path, .. }
            | InputError::Invalid { path, .. } => path,
        }
    }
}

/// Check the fields of every resource block in a loaded graph.
pub(crate) fn check_dag(dag: &Dag, scope: &Scope) -> Result<(), LoadError> {
    // Sorted so the same configuration always reports the same error.
    let mut names = dag.block_names();
    names.sort();
    let nodes: Vec<_> = names
        .iter()
        .filter_map(|name| dag.get_node(name))
        .filter(|node| !crate::module::is_instance(node))
        .map(|node| (node, node.resource.schema()))
        .collect();
    let outputs = nodes
        .iter()
        .map(|(node, schema)| (node.name.clone(), schema.outputs.clone()))
        .collect();
    let checker = Checker { scope, outputs };
    for (node, schema) in &nodes {
        checker
            .check_block(&node.fields, &schema.inputs)
            .map_err(|source| LoadError::InvalidInputs {
                pos: node.pos.clone(),
                block: node.name.clone(),
                source: Box::new(source),
            })?;
    }
    Ok(())
}

/// Check a block's evaluated inputs just before it runs.
pub fn check_values(inputs: &Map, schema: &StructType) -> Result<(), InputError> {
    let fields: Vec<Field> = inputs
        .iter()
        .map(|(name, value)| Field {
            name: name.clone(),
            value: value.to_expr(),
        })
        .collect();
    let checker = Checker {
        scope: &Scope::new(),
        outputs: HashMap::new(),
    };
    checker.check_block(&fields, schema)
}

/// The type the engine expects of one of its own fields.
fn engine_field_type(name: &str) -> Type {
    match name {
        CONCURRENCY_FIELD => Type::Number,
        UNCACHED_FIELD => Type::Union(vec![Type::String, Type::List(Box::new(Type::String))]),
        DIR_FIELD => Type::String,
        // The loader resolves, and so checks, dependency lists itself.
        _ => Type::Any,
    }
}

struct Checker<'a> {
    /// Params and let bindings, which are known at load time.
    scope: &'a Scope,
    /// Output schemas of resource blocks, by graph name. Module instances
    /// are absent because their output types are not declared.
    outputs: HashMap<String, StructType>,
}

impl Checker<'_> {
    fn check_block(&self, fields: &[Field], schema: &StructType) -> Result<(), InputError> {
        let entries: Vec<_> = fields.iter().map(|field| (field.name.as_str(), &field.value)).collect();
        self.check_struct(&entries, schema, "", &ENGINE_FIELDS)
    }

    /// Check map entries against a struct type. `extra` names fields that
    /// are valid in addition to the struct's own.
    fn check_struct(
        &self,
        entries: &[(&str, &Expr)],
        st: &StructType,
        path: &str,
        extra: &[&str],
    ) -> Result<(), InputError> {
        for (name, value) in entries {
            let field_path = join(path, name);
            let expected = match st.field(name) {
                Some(field) => field.typ.clone(),
                None if extra.contains(name) => engine_field_type(name),
                None => {
                    let valid: Vec<String> = st
                        .fields
                        .iter()
                        .map(|(name, _)| name.clone())
                        .chain(extra.iter().map(|name| (*name).to_owned()))
                        .collect();
                    return Err(InputError::UnknownField {
                        suggestion: closest_name(name, &valid).map(str::to_owned),
                        path: field_path,
                        valid,
                    });
                }
            };
            self.check(value, &expected, &field_path)?;
        }
        for (name, field) in &st.fields {
            if !matches!(field.typ, Type::Optional(_)) && !entries.iter().any(|(entry, _)| entry == name) {
                return Err(InputError::MissingField { path: join(path, name) });
            }
        }
        Ok(())
    }

    fn check(&self, expr: &Expr, expected: &Type, path: &str) -> Result<(), InputError> {
        match expected {
            Type::Any => return Ok(()),
            Type::Optional(_) if matches!(expr, Expr::Null) => return Ok(()),
            Type::Optional(inner) => return self.check(expr, inner, path),
            Type::Union(variants) => return self.check_union(expr, variants, expected, path),
            _ => {}
        }
        let mismatch = || {
            Err(InputError::Mismatch {
                path: path.to_owned(),
                expected: expected.to_string(),
                actual: describe(expr),
            })
        };
        match expr {
            Expr::Null => mismatch(),
            Expr::Str(parts) => match (literal_string(parts), expected) {
                (_, Type::String | Type::Path | Type::Secret) => Ok(()),
                (Some(value), Type::Literal(literal)) if value != *literal => mismatch(),
                (Some(value), Type::Duration) => {
                    parse_duration_literal(&value)
                        .map(drop)
                        .map_err(|message| InputError::Invalid {
                            path: path.to_owned(),
                            message,
                        })
                }
                // An interpolated string's value is not known until it is evaluated.
                (_, Type::Literal(_) | Type::Duration) => Ok(()),
                _ => mismatch(),
            },
            Expr::Number(_) if matches!(expected, Type::Number) => Ok(()),
            Expr::Bool(_) if matches!(expected, Type::Bool) => Ok(()),
            Expr::Duration(_) if matches!(expected, Type::Duration) => Ok(()),
            Expr::BlockRef(_) if matches!(expected, Type::BlockRef) => Ok(()),
            Expr::List(items) => match expected {
                Type::List(inner) => items
                    .iter()
                    .enumerate()
                    .try_for_each(|(i, item)| self.check(item, inner, &format!("{path}[{i}]"))),
                _ => mismatch(),
            },
            Expr::Map(entries) => match expected {
                Type::Struct(st) => {
                    let entries: Vec<_> = entries
                        .iter()
                        .map(|entry| (entry.name.as_str(), &entry.value))
                        .collect();
                    self.check_struct(&entries, st, path, &[])
                }
                Type::Map(inner) => entries
                    .iter()
                    .try_for_each(|entry| self.check(&entry.value, inner, &join(path, &entry.name))),
                _ => mismatch(),
            },
            Expr::Ref(parts) => self.check_ref(parts, expected, path),
            Expr::MatrixRef { name, keys, fields } => match expr::eval_matrix_ref_name(name, keys, self.scope) {
                Ok(_) if fields.is_empty() => check_type(&Type::BlockRef, expected, path),
                Ok(slice) => self.check_output(&slice, fields, expected, path),
                Err(_) => Ok(()),
            },
            Expr::BlockCall { fields, .. } if fields.is_empty() => check_type(&Type::BlockRef, expected, path),
            Expr::Call(name, _) | Expr::Pipe(_, name, _) => match expr::function_return_type(name, self.scope) {
                Some(returns) => check_type(&returns, expected, path),
                None => Ok(()),
            },
            Expr::If(_, then_value, else_value) => {
                self.check(then_value, expected, path)?;
                self.check(else_value, expected, path)
            }
            Expr::BinOp(..) => check_type(&Type::Bool, expected, path),
            Expr::Add(lhs, rhs) => {
                self.check(lhs, expected, path)?;
                self.check(rhs, expected, path)
            }
            Expr::BlockCall { .. } => Ok(()),
            Expr::Number(_) | Expr::Bool(_) | Expr::Duration(_) | Expr::BlockRef(_) => mismatch(),
        }
    }

    fn check_union(&self, expr: &Expr, variants: &[Type], expected: &Type, path: &str) -> Result<(), InputError> {
        let mut nested = Vec::new();
        for variant in variants {
            match self.check(expr, variant, path) {
                Ok(()) => return Ok(()),
                // A variant that failed below `path` matched the value's
                // shape, so its error says more than the union's.
                Err(error) if error.path() != path => nested.push(error),
                Err(_) => {}
            }
        }
        if nested.len() == 1 {
            return Err(nested.remove(0));
        }
        Err(InputError::Mismatch {
            path: path.to_owned(),
            expected: expected.to_string(),
            actual: describe(expr),
        })
    }

    fn check_ref(&self, parts: &[String], expected: &Type, path: &str) -> Result<(), InputError> {
        let (root, fields) = parts.split_first().expect("references are never empty");
        if self.outputs.contains_key(root) {
            return self.check_output(root, fields, expected, path);
        }
        if !matches!(self.scope.kind(root), Some(SymbolKind::Param | SymbolKind::Let)) {
            return Ok(());
        }
        // Looking up a binding has no side effects. A param without a value
        // fails here and is reported when a block that uses it runs.
        match expr::eval(&Expr::Ref(parts.to_vec()), self.scope) {
            Ok(value) => self.check(&value.to_expr(), expected, path),
            Err(_) => Ok(()),
        }
    }

    /// Check a block output against the type its resource declares for it.
    fn check_output(&self, block: &str, fields: &[String], expected: &Type, path: &str) -> Result<(), InputError> {
        match self.output_type(block, fields) {
            Some(actual) => check_type(&actual, expected, path),
            None => Ok(()),
        }
    }

    /// The declared type of `block.fields...`. `None` when the resource does
    /// not declare it, as with exec's outputs from its `outputs` command.
    fn output_type(&self, block: &str, fields: &[String]) -> Option<Type> {
        let (first, rest) = fields.split_first()?;
        let mut typ = self.outputs.get(block)?.field(first)?.typ.clone();
        for field in rest {
            typ = match typ {
                Type::Optional(inner) => *inner,
                typ => typ,
            };
            typ = match typ {
                Type::Struct(st) => st.field(field)?.typ.clone(),
                Type::Map(inner) => *inner,
                _ => return None,
            };
        }
        Some(typ)
    }
}

/// Check a value known only by its type.
fn check_type(actual: &Type, expected: &Type, path: &str) -> Result<(), InputError> {
    if compatible(actual, expected) {
        return Ok(());
    }
    Err(InputError::Mismatch {
        path: path.to_owned(),
        expected: expected.to_string(),
        actual: actual.to_string(),
    })
}

/// Whether a value of type `actual` may fit `expected`. Value-dependent cases
/// are accepted and left to the check before the block runs.
fn compatible(actual: &Type, expected: &Type) -> bool {
    match (actual, expected) {
        (Type::Any, _) | (_, Type::Any) => true,
        (Type::Optional(actual), _) => compatible(actual, expected),
        (_, Type::Optional(expected)) => compatible(actual, expected),
        (Type::Union(variants), _) => variants.iter().any(|variant| compatible(variant, expected)),
        (_, Type::Union(variants)) => variants.iter().any(|variant| compatible(actual, variant)),
        (Type::Literal(actual), Type::Literal(expected)) => actual == expected,
        (
            Type::String | Type::Path | Type::Secret | Type::Literal(_),
            Type::String | Type::Path | Type::Secret | Type::Literal(_) | Type::Duration,
        ) => true,
        (Type::List(actual), Type::List(expected)) | (Type::Map(actual), Type::Map(expected)) => {
            compatible(actual, expected)
        }
        (Type::Struct(actual), Type::Map(expected)) => {
            actual.fields.iter().all(|(_, field)| compatible(&field.typ, expected))
        }
        (Type::Map(_), Type::Struct(_)) => true,
        (Type::Struct(actual), Type::Struct(expected)) => {
            actual.fields.iter().all(|(name, field)| {
                expected
                    .field(name)
                    .is_some_and(|expected| compatible(&field.typ, &expected.typ))
            }) && expected
                .fields
                .iter()
                .all(|(name, field)| matches!(field.typ, Type::Optional(_)) || actual.field(name).is_some())
        }
        (actual, expected) => actual == expected,
    }
}

/// The value of a string without interpolation.
fn literal_string(parts: &[StringPart]) -> Option<String> {
    parts
        .iter()
        .map(|part| match part {
            StringPart::Literal(text) => Some(text.as_str()),
            StringPart::Interpolation(_) => None,
        })
        .collect()
}

fn describe(expr: &Expr) -> String {
    match expr {
        Expr::Str(parts) => literal_string(parts).map_or_else(|| "string".to_owned(), |value| format!("{value:?}")),
        Expr::Number(_) => "number".into(),
        Expr::Bool(_) => "bool".into(),
        Expr::Duration(_) => "duration".into(),
        Expr::Null => "null".into(),
        Expr::List(_) => "list".into(),
        Expr::Map(_) => "map".into(),
        Expr::BlockRef(_) => "block".into(),
        _ => format!("'{expr}'"),
    }
}

fn join(path: &str, name: &str) -> String {
    if path.is_empty() {
        name.to_owned()
    } else {
        format!("{path}.{name}")
    }
}

/// The candidate within a third of `name`'s length in edit distance, if any.
fn closest_name<'a>(name: &str, candidates: &'a [String]) -> Option<&'a str> {
    let limit = (name.chars().count() / 3).max(1);
    candidates
        .iter()
        .map(|candidate| (edit_distance(name, candidate), candidate))
        .filter(|(distance, _)| *distance <= limit)
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, candidate)| candidate.as_str())
}

/// Optimal string alignment distance: edits are insertions, deletions,
/// substitutions and swaps of adjacent characters.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut d = vec![vec![0; b.len() + 1]; a.len() + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=a.len() {
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            d[i][j] = (d[i - 1][j] + 1).min(d[i][j - 1] + 1).min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d[i][j] = d[i][j].min(d[i - 2][j - 2] + 1);
            }
        }
    }
    d[a.len()][b.len()]
}

fn did_you_mean(suggestion: &Option<String>) -> String {
    suggestion
        .as_ref()
        .map(|name| format!(" (did you mean '{name}'?)"))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::Statement;
    use crate::value::{StructField, Value};

    fn field(typ: Type) -> StructField {
        StructField {
            typ,
            default: None,
            description: None,
        }
    }

    fn schema(fields: Vec<(&str, Type)>) -> StructType {
        StructType {
            description: None,
            fields: fields
                .into_iter()
                .map(|(name, typ)| (name.to_owned(), field(typ)))
                .collect(),
        }
    }

    fn optional(typ: Type) -> Type {
        Type::Optional(Box::new(typ))
    }

    fn healthcheck_schema() -> StructType {
        let full = schema(vec![("test", Type::String), ("interval", optional(Type::Duration))]);
        schema(vec![(
            "healthcheck",
            optional(Type::Union(vec![Type::String, Type::Struct(full)])),
        )])
    }

    fn check_fields(body: &str, schema: &StructType) -> Result<(), InputError> {
        let module = crate::parser::parse(&format!("b = x.y {{ {body} }}"), "<test>").unwrap();
        let Statement::Block(block) = &module.statements[0] else {
            panic!("expected a block");
        };
        let checker = Checker {
            scope: &Scope::new(),
            outputs: HashMap::new(),
        };
        checker.check_block(&block.fields, schema)
    }

    fn message(result: Result<(), InputError>) -> String {
        result.expect_err("expected an input error").to_string()
    }

    #[test]
    fn nested_struct_keys_are_checked_inside_a_union() {
        assert_eq!(
            message(check_fields(
                r#"healthcheck = { tset = "true" }"#,
                &healthcheck_schema()
            )),
            "unknown field 'healthcheck.tset' (did you mean 'test'?); valid fields: test, interval"
        );
        assert_eq!(
            message(check_fields(
                r#"healthcheck = { interval = 1s }"#,
                &healthcheck_schema()
            )),
            "missing required field 'healthcheck.test'"
        );
        assert_eq!(
            message(check_fields("healthcheck = 1", &healthcheck_schema())),
            "field 'healthcheck': expected string | {test = string, interval = duration?}, got number"
        );
        check_fields(r#"healthcheck = "true""#, &healthcheck_schema()).unwrap();
        check_fields(
            r#"healthcheck = { test = "true", interval = 1s }"#,
            &healthcheck_schema(),
        )
        .unwrap();
    }

    #[test]
    fn duration_strings_must_parse() {
        check_fields(
            r#"healthcheck = { test = "true", interval = "5s" }"#,
            &healthcheck_schema(),
        )
        .unwrap();
        assert!(
            message(check_fields(
                r#"healthcheck = { test = "true", interval = "soon" }"#,
                &healthcheck_schema()
            ))
            .starts_with("field 'healthcheck.interval': invalid duration 'soon'")
        );
    }

    #[test]
    fn maps_check_every_value() {
        let env = schema(vec![("env", optional(Type::Map(Box::new(Type::String))))]);
        check_fields(r#"env = { A = "1" }"#, &env).unwrap();
        assert_eq!(
            message(check_fields("env = { A = 1 }", &env)),
            "field 'env.A': expected string, got number"
        );
    }

    #[test]
    fn function_results_are_checked_by_return_type() {
        let command = schema(vec![
            ("command", Type::String),
            ("inputs", optional(Type::List(Box::new(Type::String)))),
        ]);
        check_fields(r#"command = "true" inputs = glob("*.go")"#, &command).unwrap();
        // trim returns a string or a list depending on its argument.
        check_fields(r#"command = trim(" true ") inputs = trim(["a"])"#, &command).unwrap();
        assert_eq!(
            message(check_fields(r#"command = glob("*.go")"#, &command)),
            "field 'command': expected string, got [string]"
        );
    }

    #[test]
    fn evaluated_inputs_are_checked_before_a_block_runs() {
        let flags = schema(vec![("flags", optional(Type::List(Box::new(Type::String))))]);
        let mut inputs = Map::new();
        inputs.insert("flags".into(), Value::list(vec![Value::Str("-v".into())]));
        check_values(&inputs, &flags).unwrap();

        inputs.insert("flags".into(), Value::Str("-v".into()));
        assert_eq!(
            message(check_values(&inputs, &flags)),
            "field 'flags': expected [string], got \"-v\""
        );
    }
}
