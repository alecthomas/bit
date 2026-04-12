use std::collections::HashMap;
use std::process::Command;

use crate::ast::{BinOp, Expr, Field, StringPart};
use crate::value::{Map, Value};

#[derive(Debug, thiserror::Error)]
pub enum EvalError {
    #[error("undefined variable: {0}")]
    UndefinedVar(String),
    #[error("undefined field: {0}")]
    UndefinedField(String),
    #[error("unknown function: {0}")]
    UnknownFunc(String),
    #[error("type error: {0}")]
    Type(String),
    #[error("wrong number of arguments for {name}: expected {expected}, got {got}")]
    Arity { name: String, expected: usize, got: usize },
    #[error("exec failed: {0}")]
    Exec(String),
    #[error("glob error: {0}")]
    Glob(String),
}

/// Scope for variable lookups during expression evaluation.
#[derive(Clone)]
pub struct Scope {
    vars: HashMap<String, Value>,
}

impl Scope {
    pub fn new() -> Self {
        Self { vars: HashMap::new() }
    }

    pub fn set(&mut self, name: impl Into<String>, value: Value) {
        self.vars.insert(name.into(), value);
    }

    pub fn get(&self, name: &str) -> Option<&Value> {
        self.vars.get(name)
    }
}

impl Default for Scope {
    fn default() -> Self {
        Self::new()
    }
}

/// Controls how eval handles missing map fields.
#[derive(Clone, Copy, PartialEq)]
pub enum EvalMode {
    /// Strict: missing fields are errors.
    Strict,
    /// Lenient: missing fields produce `${ref}` placeholder strings.
    Lenient,
}

/// Evaluate an expression within a scope.
pub fn eval(expr: &Expr, scope: &Scope) -> Result<Value, EvalError> {
    eval_inner(expr, scope, EvalMode::Strict)
}

/// Evaluate an expression in lenient mode — unresolved field references
/// produce `${block.field}` placeholder strings instead of errors.
pub fn eval_lenient(expr: &Expr, scope: &Scope) -> Result<Value, EvalError> {
    eval_inner(expr, scope, EvalMode::Lenient)
}

fn eval_inner(expr: &Expr, scope: &Scope, mode: EvalMode) -> Result<Value, EvalError> {
    match expr {
        Expr::Str(parts) => eval_string(parts, scope, mode),
        Expr::Number(n) => Ok(Value::Number(n.clone())),
        Expr::Bool(b) => Ok(Value::Bool(*b)),
        Expr::Null => Ok(Value::Null),
        Expr::List(items) => {
            let values: Result<Vec<_>, _> = items.iter().map(|e| eval_inner(e, scope, mode)).collect();
            Ok(Value::List(values?))
        }
        Expr::Map(fields) => eval_map(fields, scope, mode),
        Expr::Ref(parts) => eval_ref(parts, scope, mode),
        Expr::Call(name, args) => {
            let values: Result<Vec<_>, _> = args.iter().map(|e| eval_inner(e, scope, mode)).collect();
            call_builtin(name, &values?)
        }
        Expr::Pipe(inner, name, args) => {
            let lhs = eval_inner(inner, scope, mode)?;
            let mut all_args = vec![lhs];
            for arg in args {
                all_args.push(eval_inner(arg, scope, mode)?);
            }
            call_builtin(name, &all_args)
        }
        Expr::If(cond, then_val, else_val) => {
            let cond = eval_inner(cond, scope, mode)?;
            match cond {
                Value::Bool(true) => eval_inner(then_val, scope, mode),
                Value::Bool(false) => eval_inner(else_val, scope, mode),
                _ => Err(EvalError::Type("if condition must be bool".into())),
            }
        }
        Expr::BinOp(lhs, op, rhs) => {
            let l = eval_inner(lhs, scope, mode)?;
            let r = eval_inner(rhs, scope, mode)?;
            match op {
                BinOp::Eq => Ok(Value::Bool(l == r)),
                BinOp::Ne => Ok(Value::Bool(l != r)),
            }
        }
        Expr::Add(lhs, rhs) => {
            let l = eval_inner(lhs, scope, mode)?;
            let r = eval_inner(rhs, scope, mode)?;
            match (l, r) {
                (Value::List(mut a), Value::List(b)) => {
                    a.extend(b);
                    Ok(Value::List(a))
                }
                _ => Err(EvalError::Type("+ requires two lists".into())),
            }
        }
    }
}

fn eval_string(parts: &[StringPart], scope: &Scope, mode: EvalMode) -> Result<Value, EvalError> {
    let mut result = String::new();
    for part in parts {
        match part {
            StringPart::Literal(s) => result.push_str(s),
            StringPart::Interpolation(expr) => {
                let val = eval_inner(expr, scope, mode)?;
                result.push_str(&val.to_string());
            }
        }
    }
    Ok(Value::Str(result))
}

fn eval_map(fields: &[Field], scope: &Scope, mode: EvalMode) -> Result<Value, EvalError> {
    let mut map = Map::new();
    for field in fields {
        map.insert(field.name.clone(), eval_inner(&field.value, scope, mode)?);
    }
    Ok(Value::Map(map))
}

fn eval_ref(parts: &[String], scope: &Scope, mode: EvalMode) -> Result<Value, EvalError> {
    let first = &parts[0];
    let root = scope.get(first).ok_or_else(|| EvalError::UndefinedVar(first.clone()))?;

    // Navigate nested map fields: block.field.subfield
    let mut current = root.clone();
    for part in &parts[1..] {
        match current {
            Value::Map(map) => match map.get(part).cloned() {
                Some(val) => current = val,
                None if mode == EvalMode::Lenient => {
                    return Ok(Value::Str(format!("${{{}}}", parts.join("."))));
                }
                None => {
                    return Err(EvalError::UndefinedField(parts.join(".")));
                }
            },
            _ => {
                return Err(EvalError::Type(format!(
                    "cannot access field '{part}' on non-map value"
                )));
            }
        }
    }
    Ok(current)
}

fn check_arity(name: &str, args: &[Value], expected: usize) -> Result<(), EvalError> {
    if args.len() != expected {
        return Err(EvalError::Arity {
            name: name.into(),
            expected,
            got: args.len(),
        });
    }
    Ok(())
}

fn call_builtin(name: &str, args: &[Value]) -> Result<Value, EvalError> {
    match name {
        "env" => builtin_env(args),
        "exec" => builtin_exec(args),
        "glob" => builtin_glob(args),
        "secret" => builtin_secret(args),
        "trim" => builtin_trim(args),
        "lines" => builtin_lines(args),
        "split" => builtin_split(args),
        "uniq" => builtin_uniq(args),
        _ => Err(EvalError::UnknownFunc(name.into())),
    }
}

/// `env(name)` or `env(name, default)`
fn builtin_env(args: &[Value]) -> Result<Value, EvalError> {
    if args.is_empty() || args.len() > 2 {
        return Err(EvalError::Arity {
            name: "env".into(),
            expected: 1,
            got: args.len(),
        });
    }
    let name = args[0]
        .as_str()
        .ok_or_else(|| EvalError::Type("env() name must be a string".into()))?;
    match std::env::var(name) {
        Ok(val) => Ok(Value::Str(val)),
        Err(_) => {
            if args.len() == 2 {
                Ok(args[1].clone())
            } else {
                Err(EvalError::Exec(format!("environment variable '{name}' not set")))
            }
        }
    }
}

/// `exec(cmd)` — run shell command, return stdout
fn builtin_exec(args: &[Value]) -> Result<Value, EvalError> {
    check_arity("exec", args, 1)?;
    let cmd = args[0]
        .as_str()
        .ok_or_else(|| EvalError::Type("exec() argument must be a string".into()))?;
    let output = Command::new("sh")
        .arg("-c")
        .arg(cmd)
        .output()
        .map_err(|e| EvalError::Exec(format!("failed to run '{cmd}': {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(EvalError::Exec(format!("command '{cmd}' failed: {stderr}")));
    }
    Ok(Value::Str(String::from_utf8_lossy(&output.stdout).into_owned()))
}

/// `glob(pattern)` — expand filesystem glob
fn builtin_glob(args: &[Value]) -> Result<Value, EvalError> {
    check_arity("glob", args, 1)?;
    let pattern = args[0]
        .as_str()
        .ok_or_else(|| EvalError::Type("glob() argument must be a string".into()))?;
    let paths = glob::glob(pattern).map_err(|e| EvalError::Glob(format!("invalid pattern '{pattern}': {e}")))?;
    let mut result = Vec::new();
    for entry in paths {
        match entry {
            Ok(path) => result.push(Value::Str(path.to_string_lossy().into_owned())),
            Err(e) => return Err(EvalError::Glob(e.to_string())),
        }
    }
    Ok(Value::List(result))
}

/// `secret(name)` — placeholder, TBD per spec
fn builtin_secret(args: &[Value]) -> Result<Value, EvalError> {
    check_arity("secret", args, 1)?;
    let name = args[0]
        .as_str()
        .ok_or_else(|| EvalError::Type("secret() argument must be a string".into()))?;
    // Fall back to env var for now
    std::env::var(name)
        .map(Value::Str)
        .map_err(|_| EvalError::Exec(format!("secret '{name}' not found")))
}

/// `trim(value)` — strip whitespace from string or each element of list
fn builtin_trim(args: &[Value]) -> Result<Value, EvalError> {
    check_arity("trim", args, 1)?;
    match &args[0] {
        Value::Str(s) => Ok(Value::Str(s.trim().to_owned())),
        Value::List(items) => {
            let trimmed = items
                .iter()
                .map(|v| match v {
                    Value::Str(s) => Ok(Value::Str(s.trim().to_owned())),
                    _ => Err(EvalError::Type("trim on list requires string elements".into())),
                })
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Value::List(trimmed))
        }
        _ => Err(EvalError::Type("trim() requires a string or list".into())),
    }
}

/// `lines(value)` — split string on newlines, drop empties
fn builtin_lines(args: &[Value]) -> Result<Value, EvalError> {
    check_arity("lines", args, 1)?;
    let s = args[0]
        .as_str()
        .ok_or_else(|| EvalError::Type("lines() requires a string".into()))?;
    let items = s
        .lines()
        .filter(|l| !l.is_empty())
        .map(|l| Value::Str(l.to_owned()))
        .collect();
    Ok(Value::List(items))
}

/// `split(value, separator)`
fn builtin_split(args: &[Value]) -> Result<Value, EvalError> {
    check_arity("split", args, 2)?;
    let s = args[0]
        .as_str()
        .ok_or_else(|| EvalError::Type("split() first argument must be a string".into()))?;
    let sep = args[1]
        .as_str()
        .ok_or_else(|| EvalError::Type("split() separator must be a string".into()))?;
    let items = s.split(sep).map(|p| Value::Str(p.to_owned())).collect();
    Ok(Value::List(items))
}

/// `uniq(list)` — deduplicate a list preserving order
fn builtin_uniq(args: &[Value]) -> Result<Value, EvalError> {
    check_arity("uniq", args, 1)?;
    let items = args[0]
        .as_list()
        .ok_or_else(|| EvalError::Type("uniq() requires a list".into()))?;
    let mut seen = Vec::new();
    for item in items {
        if !seen.contains(item) {
            seen.push(item.clone());
        }
    }
    Ok(Value::List(seen))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::StringPart;

    #[test]
    fn eval_int() {
        let scope = Scope::new();
        assert_eq!(
            eval(&Expr::Number(42.into()), &scope).unwrap(),
            Value::Number(42.into())
        );
    }

    #[test]
    fn eval_bool() {
        let scope = Scope::new();
        assert_eq!(eval(&Expr::Bool(true), &scope).unwrap(), Value::Bool(true));
    }

    #[test]
    fn eval_null() {
        let scope = Scope::new();
        assert_eq!(eval(&Expr::Null, &scope).unwrap(), Value::Null);
    }

    #[test]
    fn eval_plain_string() {
        let scope = Scope::new();
        let expr = Expr::Str(vec![StringPart::Literal("hello".into())]);
        assert_eq!(eval(&expr, &scope).unwrap(), Value::Str("hello".into()));
    }

    #[test]
    fn eval_interpolated_string() {
        let mut scope = Scope::new();
        scope.set("name", Value::Str("world".into()));
        let expr = Expr::Str(vec![
            StringPart::Literal("hello ".into()),
            StringPart::Interpolation(Expr::Ref(vec!["name".into()])),
        ]);
        assert_eq!(eval(&expr, &scope).unwrap(), Value::Str("hello world".into()));
    }

    #[test]
    fn eval_variable_ref() {
        let mut scope = Scope::new();
        scope.set("x", Value::Number(10.into()));
        assert_eq!(
            eval(&Expr::Ref(vec!["x".into()]), &scope).unwrap(),
            Value::Number(10.into())
        );
    }

    #[test]
    fn eval_undefined_var() {
        let scope = Scope::new();
        assert!(eval(&Expr::Ref(vec!["missing".into()]), &scope).is_err());
    }

    #[test]
    fn eval_dotted_ref() {
        let mut scope = Scope::new();
        let mut inner = Map::new();
        inner.insert("path".into(), Value::Str("/bin/server".into()));
        scope.set("server", Value::Map(inner));
        let expr = Expr::Ref(vec!["server".into(), "path".into()]);
        assert_eq!(eval(&expr, &scope).unwrap(), Value::Str("/bin/server".into()));
    }

    #[test]
    fn eval_missing_field_strict_errors() {
        let mut scope = Scope::new();
        scope.set("image", Value::Map(Map::new()));
        let expr = Expr::Ref(vec!["image".into(), "ref".into()]);
        let err = eval(&expr, &scope).unwrap_err();
        assert!(matches!(err, EvalError::UndefinedField(ref s) if s == "image.ref"));
    }

    #[test]
    fn eval_missing_field_lenient_placeholder() {
        let mut scope = Scope::new();
        scope.set("image", Value::Map(Map::new()));
        let expr = Expr::Ref(vec!["image".into(), "ref".into()]);
        assert_eq!(eval_lenient(&expr, &scope).unwrap(), Value::Str("${image.ref}".into()));
    }

    #[test]
    fn eval_missing_field_lenient_in_string() {
        let mut scope = Scope::new();
        scope.set("image", Value::Map(Map::new()));
        let expr = Expr::Str(vec![
            StringPart::Literal("docker run ".into()),
            StringPart::Interpolation(Expr::Ref(vec!["image".into(), "ref".into()])),
        ]);
        assert_eq!(
            eval_lenient(&expr, &scope).unwrap(),
            Value::Str("docker run ${image.ref}".into())
        );
    }

    #[test]
    fn eval_list() {
        let scope = Scope::new();
        let expr = Expr::List(vec![Expr::Number(1.into()), Expr::Number(2.into())]);
        assert_eq!(
            eval(&expr, &scope).unwrap(),
            Value::List(vec![Value::Number(1.into()), Value::Number(2.into())])
        );
    }

    #[test]
    fn eval_map() {
        let scope = Scope::new();
        let expr = Expr::Map(vec![Field {
            name: "a".into(),
            value: Expr::Number(1.into()),
        }]);
        let result = eval(&expr, &scope).unwrap();
        match result {
            Value::Map(m) => assert_eq!(m.get("a"), Some(&Value::Number(1.into()))),
            _ => panic!("expected Map"),
        }
    }

    #[test]
    fn eval_if_true() {
        let scope = Scope::new();
        let expr = Expr::If(
            Box::new(Expr::Bool(true)),
            Box::new(Expr::Number(1.into())),
            Box::new(Expr::Number(2.into())),
        );
        assert_eq!(eval(&expr, &scope).unwrap(), Value::Number(1.into()));
    }

    #[test]
    fn eval_if_false() {
        let scope = Scope::new();
        let expr = Expr::If(
            Box::new(Expr::Bool(false)),
            Box::new(Expr::Number(1.into())),
            Box::new(Expr::Number(2.into())),
        );
        assert_eq!(eval(&expr, &scope).unwrap(), Value::Number(2.into()));
    }

    #[test]
    fn eval_eq() {
        let scope = Scope::new();
        let expr = Expr::BinOp(
            Box::new(Expr::Number(1.into())),
            BinOp::Eq,
            Box::new(Expr::Number(1.into())),
        );
        assert_eq!(eval(&expr, &scope).unwrap(), Value::Bool(true));
    }

    #[test]
    fn eval_ne() {
        let scope = Scope::new();
        let expr = Expr::BinOp(
            Box::new(Expr::Number(1.into())),
            BinOp::Ne,
            Box::new(Expr::Number(2.into())),
        );
        assert_eq!(eval(&expr, &scope).unwrap(), Value::Bool(true));
    }

    #[test]
    fn eval_list_add() {
        let scope = Scope::new();
        let expr = Expr::Add(
            Box::new(Expr::List(vec![Expr::Number(1.into())])),
            Box::new(Expr::List(vec![Expr::Number(2.into())])),
        );
        assert_eq!(
            eval(&expr, &scope).unwrap(),
            Value::List(vec![Value::Number(1.into()), Value::Number(2.into())])
        );
    }

    #[test]
    fn eval_exec() {
        let scope = Scope::new();
        let expr = Expr::Call(
            "exec".into(),
            vec![Expr::Str(vec![StringPart::Literal("echo hello".into())])],
        );
        assert_eq!(eval(&expr, &scope).unwrap(), Value::Str("hello\n".into()));
    }

    #[test]
    fn eval_trim() {
        let scope = Scope::new();
        let expr = Expr::Pipe(
            Box::new(Expr::Str(vec![StringPart::Literal("  hello  ".into())])),
            "trim".into(),
            vec![],
        );
        assert_eq!(eval(&expr, &scope).unwrap(), Value::Str("hello".into()));
    }

    #[test]
    fn eval_lines() {
        let scope = Scope::new();
        let expr = Expr::Pipe(
            Box::new(Expr::Str(vec![StringPart::Literal("a\nb\n\nc".into())])),
            "lines".into(),
            vec![],
        );
        assert_eq!(
            eval(&expr, &scope).unwrap(),
            Value::List(vec![
                Value::Str("a".into()),
                Value::Str("b".into()),
                Value::Str("c".into()),
            ])
        );
    }

    #[test]
    fn eval_split() {
        let scope = Scope::new();
        let expr = Expr::Pipe(
            Box::new(Expr::Str(vec![StringPart::Literal("a:b:c".into())])),
            "split".into(),
            vec![Expr::Str(vec![StringPart::Literal(":".into())])],
        );
        assert_eq!(
            eval(&expr, &scope).unwrap(),
            Value::List(vec![
                Value::Str("a".into()),
                Value::Str("b".into()),
                Value::Str("c".into()),
            ])
        );
    }

    #[test]
    fn eval_uniq() {
        let scope = Scope::new();
        let expr = Expr::Pipe(
            Box::new(Expr::List(vec![
                Expr::Str(vec![StringPart::Literal("a".into())]),
                Expr::Str(vec![StringPart::Literal("b".into())]),
                Expr::Str(vec![StringPart::Literal("a".into())]),
            ])),
            "uniq".into(),
            vec![],
        );
        assert_eq!(
            eval(&expr, &scope).unwrap(),
            Value::List(vec![Value::Str("a".into()), Value::Str("b".into()),])
        );
    }

    #[test]
    fn eval_pipe_chain() {
        let scope = Scope::new();
        // "  hello  " | trim | lines (single line, so just ["hello"])
        let expr = Expr::Pipe(
            Box::new(Expr::Pipe(
                Box::new(Expr::Str(vec![StringPart::Literal("  hello  ".into())])),
                "trim".into(),
                vec![],
            )),
            "lines".into(),
            vec![],
        );
        assert_eq!(
            eval(&expr, &scope).unwrap(),
            Value::List(vec![Value::Str("hello".into())])
        );
    }

    #[test]
    fn eval_env_with_default() {
        let scope = Scope::new();
        let expr = Expr::Call(
            "env".into(),
            vec![
                Expr::Str(vec![StringPart::Literal("BIT_TEST_NONEXISTENT_VAR_12345".into())]),
                Expr::Str(vec![StringPart::Literal("fallback".into())]),
            ],
        );
        assert_eq!(eval(&expr, &scope).unwrap(), Value::Str("fallback".into()));
    }

    #[test]
    fn eval_exec_pipe_trim() {
        let scope = Scope::new();
        let expr = Expr::Pipe(
            Box::new(Expr::Call(
                "exec".into(),
                vec![Expr::Str(vec![StringPart::Literal("echo hello".into())])],
            )),
            "trim".into(),
            vec![],
        );
        assert_eq!(eval(&expr, &scope).unwrap(), Value::Str("hello".into()));
    }
}
