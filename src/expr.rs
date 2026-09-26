use std::collections::HashMap;
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::ast::{BinOp, Expr, MapEntry, StringPart};
use crate::provider::{FuncSignature, ProviderRegistry};
use crate::schema::SchemaType;
use crate::value::{Map, StructField, StructType, Type, Value, validate_type};

#[derive(Debug, thiserror::Error)]
pub enum EvalError {
    #[error("undefined variable: {0}")]
    UndefinedVar(String),
    #[error("undefined field: {0}")]
    UndefinedField(String),
    #[error("unknown function: {0}")]
    UnknownFunc(String),
    #[error("parameterized block call was not materialized: {0}")]
    UnmaterializedBlockCall(String),
    #[error("provider function '{name}' failed: {message}")]
    ProviderFunction { name: String, message: String },
    #[error("type error: {0}")]
    Type(String),
    #[error("wrong number of arguments for {name}: expected {expected}, got {got}")]
    Arity { name: String, expected: usize, got: usize },
    #[error("exec failed: {0}")]
    Exec(String),
    #[error("glob error: {0}")]
    Glob(String),
}

/// What kind of symbol a name represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    Param,
    Let,
    Block,
}

impl SymbolKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SymbolKind::Param => "param",
            SymbolKind::Let => "variable",
            SymbolKind::Block => "block",
        }
    }
}

impl std::fmt::Display for SymbolKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Scope for variable lookups during expression evaluation.
///
/// Each name carries a [`SymbolKind`] so the loader can detect collisions
/// between params, let bindings, and blocks that share a flat namespace.
#[derive(Clone)]
pub struct Scope {
    vars: HashMap<String, (SymbolKind, Value)>,
    providers: Option<ProviderRegistry>,
}

impl Scope {
    pub fn new() -> Self {
        Self {
            vars: HashMap::new(),
            providers: None,
        }
    }

    pub fn with_providers(providers: ProviderRegistry) -> Self {
        Self {
            vars: HashMap::new(),
            providers: Some(providers),
        }
    }

    /// Define a new symbol. Returns `Err(existing_kind)` if the name is
    /// already taken by a different kind of symbol.
    pub fn define(&mut self, name: impl Into<String>, kind: SymbolKind, value: Value) -> Result<(), SymbolKind> {
        let name = name.into();
        if let Some((existing_kind, _)) = self.vars.get(&name) {
            return Err(*existing_kind);
        }
        self.vars.insert(name, (kind, value));
        Ok(())
    }

    /// Overwrite an existing symbol's value without conflict checking.
    /// Used by the engine to fill block placeholders with real outputs.
    pub fn set(&mut self, name: impl Into<String>, value: Value) {
        let name = name.into();
        let kind = self.vars.get(&name).map(|(k, _)| *k).unwrap_or(SymbolKind::Block);
        self.vars.insert(name, (kind, value));
    }

    pub fn get(&self, name: &str) -> Option<&Value> {
        self.vars.get(name).map(|(_, v)| v)
    }

    pub fn kind(&self, name: &str) -> Option<SymbolKind> {
        self.vars.get(name).map(|(k, _)| *k)
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
    /// Lenient: missing fields produce `#{ref}` placeholder strings.
    Lenient,
}

/// Evaluate an expression within a scope.
pub fn eval(expr: &Expr, scope: &Scope) -> Result<Value, EvalError> {
    eval_inner(expr, scope, EvalMode::Strict)
}

/// Evaluate an expression in lenient mode — unresolved field references
/// produce `#{block.field}` placeholder strings instead of errors.
pub fn eval_lenient(expr: &Expr, scope: &Scope) -> Result<Value, EvalError> {
    eval_inner(expr, scope, EvalMode::Lenient)
}

fn eval_inner(expr: &Expr, scope: &Scope, mode: EvalMode) -> Result<Value, EvalError> {
    match expr {
        Expr::Str(parts) => eval_string(parts, scope, mode),
        Expr::Number(n) => Ok(Value::Number(n.clone())),
        Expr::Bool(b) => Ok(Value::Bool(*b)),
        Expr::Duration(d) => Ok(Value::Duration(*d)),
        Expr::Null => Ok(Value::Null),
        Expr::List(items) => {
            let values: Result<Vec<_>, _> = items.iter().map(|e| eval_inner(e, scope, mode)).collect();
            Ok(Value::list(values?))
        }
        Expr::Map(fields) => eval_map(fields, scope, mode),
        Expr::Ref(parts) => eval_ref(parts, scope, mode),
        Expr::BlockRef(name) => Ok(Value::BlockRef(name.clone())),
        Expr::MatrixRef { name, keys, fields } => {
            let root = eval_matrix_ref_name_inner(name, keys, scope, mode)?;
            let mut parts = Vec::with_capacity(fields.len() + 1);
            parts.push(root);
            parts.extend(fields.iter().cloned());
            eval_ref(&parts, scope, mode)
        }
        Expr::BlockCall { name, .. } => Err(EvalError::UnmaterializedBlockCall(name.clone())),
        Expr::Call(name, args) => {
            let values: Result<Vec<_>, _> = args.iter().map(|e| eval_inner(e, scope, mode)).collect();
            let values = values?;
            if mode == EvalMode::Lenient && values.iter().any(has_placeholder) {
                return Ok(values.into_iter().find(has_placeholder).unwrap());
            }
            call_function(name, &values, scope)
        }
        Expr::Pipe(inner, name, args) => {
            let lhs = eval_inner(inner, scope, mode)?;
            let mut all_args = vec![lhs];
            for arg in args {
                all_args.push(eval_inner(arg, scope, mode)?);
            }
            if mode == EvalMode::Lenient && all_args.iter().any(has_placeholder) {
                return Ok(all_args.into_iter().find(has_placeholder).unwrap());
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
                (Value::Number(a), Value::Number(b)) => Ok(Value::Number(a + b)),
                (Value::Str(a), Value::Str(b)) => Ok(Value::Str(a + &b)),
                (Value::List(typ, mut a), Value::List(_, b)) => {
                    a.extend(b);
                    Ok(Value::List(typ, a))
                }
                (l, r) => Err(EvalError::Type(format!(
                    "+ requires matching types (two numbers, strings, or lists), got {l} and {r}",
                ))),
            }
        }
    }
}

pub(crate) fn eval_matrix_ref_name(name: &str, keys: &[Expr], scope: &Scope) -> Result<String, EvalError> {
    eval_matrix_ref_name_inner(name, keys, scope, EvalMode::Strict)
}

fn eval_matrix_ref_name_inner(name: &str, keys: &[Expr], scope: &Scope, mode: EvalMode) -> Result<String, EvalError> {
    let values: Result<Vec<_>, _> = keys.iter().map(|key| eval_inner(key, scope, mode)).collect();
    let values = values?;
    let value_refs: Vec<_> = values.iter().collect();
    Ok(crate::matrix::matrix_key(name, &value_refs))
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

fn eval_map(fields: &[MapEntry], scope: &Scope, mode: EvalMode) -> Result<Value, EvalError> {
    let mut map = Map::new();
    let mut types = Vec::with_capacity(fields.len());
    for field in fields {
        let value = eval_inner(&field.value, scope, mode)?;
        let typ = match &field.typ {
            Some(typ) if matches!(value, Value::Null) => Type::Optional(Box::new(typ.clone())),
            Some(typ) => {
                validate_type(&value, typ)
                    .map_err(|message| EvalError::Type(format!("map field '{}': {message}", field.name)))?;
                typ.clone()
            }
            None if matches!(value, Value::Null) => Type::Optional(Box::new(Type::String)),
            None => value.value_type(),
        };
        types.push((
            field.name.clone(),
            StructField {
                typ,
                default: None,
                description: None,
            },
        ));
        map.insert(field.name.clone(), value);
    }
    Ok(Value::Struct(
        StructType {
            description: None,
            fields: types,
        },
        map,
    ))
}

fn eval_ref(parts: &[String], scope: &Scope, mode: EvalMode) -> Result<Value, EvalError> {
    let first = &parts[0];
    let root = scope.get(first).ok_or_else(|| EvalError::UndefinedVar(first.clone()))?;

    // Navigate nested map fields: block.field.subfield
    let mut current = root.clone();
    for part in &parts[1..] {
        match current {
            Value::Map(_, map) | Value::Struct(_, map) => match map.get(part).cloned() {
                Some(val) => current = val,
                None if mode == EvalMode::Lenient => {
                    return Ok(Value::Str(format!("#{{{}}}", parts.join("."))));
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

fn has_placeholder(v: &Value) -> bool {
    matches!(v, Value::Str(s) if s.contains("#{"))
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum StringOrList {
    String(String),
    List(Vec<String>),
}

impl SchemaType for StringOrList {
    fn schema_type() -> Type {
        Type::Union(vec![Type::String, Type::List(Box::new(Type::String))])
    }
}

#[derive(Serialize)]
#[serde(untagged)]
enum EnvironmentValue {
    String(String),
    Fallback(serde_json::Value),
}

impl SchemaType for EnvironmentValue {
    fn schema_type() -> Type {
        Type::Union(vec![Type::String, Type::Any])
    }
}

impl SchemaType for serde_json::Value {
    fn schema_type() -> Type {
        Type::Any
    }
}

/// Definition of a built-in function/pipe.
struct BuiltinDef {
    func: fn(&[Value]) -> Result<Value, crate::provider::BoxError>,
    // Parser inference keeps its existing, narrower type for polymorphic calls.
    return_type: Type,
    signature: FuncSignature,
}

impl BuiltinDef {
    fn new(
        func: fn(&[Value]) -> Result<Value, crate::provider::BoxError>,
        return_type: Type,
        signature: FuncSignature,
    ) -> Self {
        Self {
            func,
            return_type,
            signature,
        }
    }
}

/// Static registry of all built-in functions and pipes.
fn builtins() -> &'static HashMap<String, BuiltinDef> {
    use std::sync::OnceLock;
    static BUILTINS: OnceLock<HashMap<String, BuiltinDef>> = OnceLock::new();
    BUILTINS.get_or_init(|| {
        [
            BuiltinDef::new(__bit_call_env, Type::String, __bit_signature_env()),
            BuiltinDef::new(__bit_call_exec, Type::String, __bit_signature_exec()),
            BuiltinDef::new(
                __bit_call_glob,
                Type::List(Box::new(Type::String)),
                __bit_signature_glob(),
            ),
            BuiltinDef::new(__bit_call_secret, Type::String, __bit_signature_secret()),
            BuiltinDef::new(__bit_call_trim, Type::String, __bit_signature_trim()),
            BuiltinDef::new(
                __bit_call_lines,
                Type::List(Box::new(Type::String)),
                __bit_signature_lines(),
            ),
            BuiltinDef::new(
                __bit_call_split,
                Type::List(Box::new(Type::String)),
                __bit_signature_split(),
            ),
            BuiltinDef::new(
                __bit_call_uniq,
                Type::List(Box::new(Type::String)),
                __bit_signature_uniq(),
            ),
            BuiltinDef::new(__bit_call_basename, Type::String, __bit_signature_basename()),
            BuiltinDef::new(__bit_call_dirname, Type::String, __bit_signature_dirname()),
            BuiltinDef::new(__bit_call_prefix, Type::String, __bit_signature_prefix()),
            BuiltinDef::new(__bit_call_suffix, Type::String, __bit_signature_suffix()),
            BuiltinDef::new(__bit_call_sha256, Type::String, __bit_signature_sha256()),
        ]
        .into_iter()
        .map(|def| (def.signature.name.clone(), def))
        .collect()
    })
}

pub fn builtin_functions() -> Vec<FuncSignature> {
    let mut functions: Vec<_> = builtins().values().map(|def| def.signature.clone()).collect();
    functions.sort_by(|a, b| a.name.cmp(&b.name));
    functions
}

/// Look up the return type of a built-in function or pipe.
pub fn builtin_return_type(name: &str) -> Option<Type> {
    builtins().get(name).map(|b| b.return_type.clone())
}

fn call_builtin(name: &str, args: &[Value]) -> Result<Value, EvalError> {
    let def = builtins()
        .get(name)
        .ok_or_else(|| EvalError::UnknownFunc(name.into()))?;
    let required = def
        .signature
        .params
        .iter()
        .take_while(|(_, field)| !matches!(field.typ, Type::Optional(_)))
        .count();
    if args.len() < required || args.len() > def.signature.params.len() {
        return Err(EvalError::Arity {
            name: name.into(),
            expected: required,
            got: args.len(),
        });
    }
    (def.func)(args).map_err(|error| match error.downcast::<EvalError>() {
        Ok(error) => *error,
        Err(error) => EvalError::Type(error.to_string()),
    })
}

fn call_function(name: &str, args: &[Value], scope: &Scope) -> Result<Value, EvalError> {
    let Some((provider, function)) = name.split_once('.') else {
        return call_builtin(name, args);
    };
    let providers = scope
        .providers
        .as_ref()
        .ok_or_else(|| EvalError::UnknownFunc(name.to_owned()))?;
    providers
        .call_function(provider, function, args)
        .map_err(|source| EvalError::ProviderFunction {
            name: name.to_owned(),
            message: source.to_string(),
        })
}

/// Read an environment variable, with an optional fallback.
#[bit_derive::provider_function]
fn env(name: String, default: Option<serde_json::Value>) -> Result<EnvironmentValue, EvalError> {
    match std::env::var(&name) {
        Ok(value) => Ok(EnvironmentValue::String(value)),
        Err(_) => default
            .map(EnvironmentValue::Fallback)
            .ok_or_else(|| EvalError::Exec(format!("environment variable '{name}' not set"))),
    }
}

/// Run a shell command and return stdout.
#[bit_derive::provider_function]
fn exec(command: String) -> Result<String, EvalError> {
    let output = Command::new("sh")
        .arg("-c")
        .arg(&command)
        .output()
        .map_err(|e| EvalError::Exec(format!("failed to run '{command}': {e}")))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(EvalError::Exec(format!("command '{command}' failed: {stderr}")));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Expand a filesystem glob.
#[bit_derive::provider_function]
fn glob(pattern: String) -> Result<Vec<String>, EvalError> {
    let paths = glob::glob(&pattern).map_err(|e| EvalError::Glob(format!("invalid pattern '{pattern}': {e}")))?;
    let mut result = Vec::new();
    for entry in paths {
        match entry {
            Ok(path) => result.push(path.to_string_lossy().into_owned()),
            Err(e) => return Err(EvalError::Glob(e.to_string())),
        }
    }
    Ok(result)
}

/// Read a secret by name.
#[bit_derive::provider_function]
fn secret(name: String) -> Result<String, EvalError> {
    // Fall back to env var for now
    std::env::var(&name).map_err(|_| EvalError::Exec(format!("secret '{name}' not found")))
}

/// Trim whitespace from a string or each string in a list.
#[bit_derive::provider_function]
fn trim(value: StringOrList) -> Result<StringOrList, EvalError> {
    Ok(match value {
        StringOrList::String(value) => StringOrList::String(value.trim().to_owned()),
        StringOrList::List(values) => {
            StringOrList::List(values.into_iter().map(|value| value.trim().to_owned()).collect())
        }
    })
}

/// Split a string into nonempty lines.
#[bit_derive::provider_function]
fn lines(value: String) -> Result<Vec<String>, EvalError> {
    Ok(value.lines().filter(|l| !l.is_empty()).map(str::to_owned).collect())
}

/// Split a string by a separator.
#[bit_derive::provider_function]
fn split(value: String, separator: String) -> Result<Vec<String>, EvalError> {
    Ok(value.split(&separator).map(str::to_owned).collect())
}

/// Deduplicate a list while preserving order.
#[bit_derive::provider_function]
fn uniq(list: Vec<serde_json::Value>) -> Result<Vec<serde_json::Value>, EvalError> {
    let mut seen = Vec::new();
    for item in list {
        if !seen.contains(&item) {
            seen.push(item);
        }
    }
    Ok(seen)
}

/// Extract file names from a path or list of paths.
#[bit_derive::provider_function]
fn basename(path: StringOrList) -> Result<StringOrList, EvalError> {
    let basename = |path: String| {
        std::path::Path::new(&path)
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    Ok(match path {
        StringOrList::String(path) => StringOrList::String(basename(path)),
        StringOrList::List(paths) => StringOrList::List(paths.into_iter().map(basename).collect()),
    })
}

/// Extract directories from a path or list of paths.
#[bit_derive::provider_function]
fn dirname(path: StringOrList) -> Result<StringOrList, EvalError> {
    let dirname = |path: String| {
        std::path::Path::new(&path)
            .parent()
            .map(|dir| dir.to_string_lossy().into_owned())
            .unwrap_or_default()
    };
    Ok(match path {
        StringOrList::String(path) => StringOrList::String(dirname(path)),
        StringOrList::List(paths) => StringOrList::List(paths.into_iter().map(dirname).collect()),
    })
}

/// Prepend text to a string or each string in a list.
#[bit_derive::provider_function]
fn prefix(value: StringOrList, text: String) -> Result<StringOrList, EvalError> {
    Ok(match value {
        StringOrList::String(value) => StringOrList::String(format!("{text}{value}")),
        StringOrList::List(values) => {
            StringOrList::List(values.into_iter().map(|value| format!("{text}{value}")).collect())
        }
    })
}

/// Append text to a string or each string in a list.
#[bit_derive::provider_function]
fn suffix(value: StringOrList, text: String) -> Result<StringOrList, EvalError> {
    Ok(match value {
        StringOrList::String(value) => StringOrList::String(format!("{value}{text}")),
        StringOrList::List(values) => {
            StringOrList::List(values.into_iter().map(|value| format!("{value}{text}")).collect())
        }
    })
}

/// Hash a string with SHA-256.
#[bit_derive::provider_function]
fn sha256(value: String) -> Result<String, EvalError> {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(value.as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    Ok(hex)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::StringPart;
    use crate::provider::{BoxError, DynResource, FuncSignature, Provider};

    struct FunctionProvider;

    impl Provider for FunctionProvider {
        fn name(&self) -> &str {
            "example"
        }

        fn resources(&self) -> Vec<Box<dyn DynResource>> {
            vec![]
        }

        fn functions(&self) -> Vec<FuncSignature> {
            vec![]
        }

        fn call_function(&self, name: &str, args: &[Value]) -> Result<Value, BoxError> {
            if name != "echo" || args.len() != 1 {
                return Err("unexpected function call".into());
            }
            Ok(args[0].clone())
        }
    }

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
    fn eval_provider_function() {
        let mut providers = ProviderRegistry::new();
        providers.register(Box::new(FunctionProvider));
        let scope = Scope::with_providers(providers);
        let expression = Expr::Call(
            "example.echo".into(),
            vec![Expr::Str(vec![StringPart::Literal("value".into())])],
        );

        assert_eq!(eval(&expression, &scope).unwrap(), Value::Str("value".into()));
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
        scope.set("server", Value::strct(inner));
        let expr = Expr::Ref(vec!["server".into(), "path".into()]);
        assert_eq!(eval(&expr, &scope).unwrap(), Value::Str("/bin/server".into()));
    }

    #[test]
    fn eval_matrix_ref_key_reference() {
        let mut scope = Scope::new();
        scope.set("selected_arch", Value::Str("amd64".into()));
        let mut outputs = Map::new();
        outputs.insert("path".into(), Value::Str("/bin/server".into()));
        scope.set(r#"build["amd64"]"#, Value::strct(outputs));
        let expression = Expr::MatrixRef {
            name: "build".into(),
            keys: vec![Expr::Ref(vec!["selected_arch".into()])],
            fields: vec!["path".into()],
        };

        assert_eq!(eval(&expression, &scope).unwrap(), Value::Str("/bin/server".into()));
    }

    #[test]
    fn eval_matrix_ref_preserves_block_ref_key() {
        let mut scope = Scope::new();
        scope.set("dependency", Value::BlockRef(r#"crate["core"]"#.into()));
        scope.set(r#"consumer[crate["core"]]"#, Value::strct(Map::new()));
        let expression = Expr::MatrixRef {
            name: "consumer".into(),
            keys: vec![Expr::Ref(vec!["dependency".into()])],
            fields: vec![],
        };

        assert_eq!(eval(&expression, &scope).unwrap(), Value::strct(Map::new()));
    }

    #[test]
    fn eval_missing_field_strict_errors() {
        let mut scope = Scope::new();
        scope.set("image", Value::strct(Map::new()));
        let expr = Expr::Ref(vec!["image".into(), "ref".into()]);
        let err = eval(&expr, &scope).unwrap_err();
        assert!(matches!(err, EvalError::UndefinedField(ref s) if s == "image.ref"));
    }

    #[test]
    fn eval_missing_field_lenient_placeholder() {
        let mut scope = Scope::new();
        scope.set("image", Value::strct(Map::new()));
        let expr = Expr::Ref(vec!["image".into(), "ref".into()]);
        assert_eq!(eval_lenient(&expr, &scope).unwrap(), Value::Str("#{image.ref}".into()));
    }

    #[test]
    fn eval_missing_field_lenient_in_string() {
        let mut scope = Scope::new();
        scope.set("image", Value::strct(Map::new()));
        let expr = Expr::Str(vec![
            StringPart::Literal("docker run ".into()),
            StringPart::Interpolation(Expr::Ref(vec!["image".into(), "ref".into()])),
        ]);
        assert_eq!(
            eval_lenient(&expr, &scope).unwrap(),
            Value::Str("docker run #{image.ref}".into())
        );
    }

    #[test]
    fn eval_call_with_placeholder_lenient_propagates() {
        let mut scope = Scope::new();
        scope.set("debug", Value::strct(Map::new()));
        let expr = Expr::Call(
            "exec".into(),
            vec![Expr::Str(vec![
                StringPart::Interpolation(Expr::Ref(vec!["debug".into(), "path".into()])),
                StringPart::Literal(" --schema".into()),
            ])],
        );
        let result = eval_lenient(&expr, &scope).unwrap();
        assert_eq!(result, Value::Str("#{debug.path} --schema".into()));
    }

    #[test]
    fn eval_pipe_with_placeholder_lenient_propagates() {
        let mut scope = Scope::new();
        scope.set("debug", Value::strct(Map::new()));
        let expr = Expr::Pipe(
            Box::new(Expr::Call(
                "exec".into(),
                vec![Expr::Str(vec![
                    StringPart::Interpolation(Expr::Ref(vec!["debug".into(), "path".into()])),
                    StringPart::Literal(" --schema".into()),
                ])],
            )),
            "sha256".into(),
            vec![],
        );
        let result = eval_lenient(&expr, &scope).unwrap();
        assert_eq!(result, Value::Str("#{debug.path} --schema".into()));
    }

    #[test]
    fn eval_list() {
        let scope = Scope::new();
        let expr = Expr::List(vec![Expr::Number(1.into()), Expr::Number(2.into())]);
        assert_eq!(
            eval(&expr, &scope).unwrap(),
            Value::list(vec![Value::Number(1.into()), Value::Number(2.into())])
        );
    }

    #[test]
    fn eval_map() {
        let scope = Scope::new();
        let expr = Expr::Map(vec![
            MapEntry {
                name: "a".into(),
                typ: None,
                value: Expr::Number(1.into()),
            },
            MapEntry {
                name: "b".into(),
                typ: None,
                value: Expr::Number(2.into()),
            },
        ]);
        let result = eval(&expr, &scope).unwrap();
        match result {
            Value::Struct(st, m) => {
                assert_eq!(m.get("a"), Some(&Value::Number(1.into())));
                assert_eq!(m.get("b"), Some(&Value::Number(2.into())));
                assert_eq!(st.field("a").unwrap().typ, Type::Number);
            }
            _ => panic!("expected Struct"),
        }
    }

    #[test]
    fn eval_map_accepts_mixed_value_types() {
        let expr = Expr::Map(vec![
            MapEntry {
                name: "name".into(),
                typ: Some(Type::String),
                value: Expr::Str(vec![StringPart::Literal("Bob".into())]),
            },
            MapEntry {
                name: "age".into(),
                typ: Some(Type::Number),
                value: Expr::Null,
            },
        ]);
        let value = eval(&expr, &Scope::new()).unwrap();
        let Value::Struct(st, map) = &value else {
            panic!("expected Struct");
        };
        assert_eq!(map.get("age"), Some(&Value::Null));
        assert_eq!(st.field("name").unwrap().typ, Type::String);
        assert_eq!(st.field("age").unwrap().typ, Type::Optional(Box::new(Type::Number)));
        assert!(validate_type(&value, &value.value_type()).is_ok());
        assert_eq!(eval(&value.to_expr(), &Scope::new()).unwrap(), value);
    }

    #[test]
    fn eval_map_rejects_invalid_annotated_value() {
        let expr = Expr::Map(vec![MapEntry {
            name: "more".into(),
            typ: Some(Type::List(Box::new(Type::Number))),
            value: Expr::List(vec![Expr::Number(3.into()), Expr::Bool(true)]),
        }]);
        let error = eval(&expr, &Scope::new()).unwrap_err();
        assert!(
            matches!(error, EvalError::Type(message) if message == "map field 'more': [1]: expected number, got bool")
        );
    }

    #[test]
    fn eval_map_accepts_unannotated_mixed_values() {
        let expr = Expr::Map(vec![
            MapEntry {
                name: "count".into(),
                typ: None,
                value: Expr::Number(2.into()),
            },
            MapEntry {
                name: "enabled".into(),
                typ: None,
                value: Expr::Bool(true),
            },
            MapEntry {
                name: "unset".into(),
                typ: None,
                value: Expr::Null,
            },
        ]);
        let value = eval(&expr, &Scope::new()).unwrap();
        assert!(validate_type(&value, &value.value_type()).is_ok());
        let Value::Struct(st, _) = value else {
            panic!("expected Struct")
        };
        assert_eq!(st.field("count").unwrap().typ, Type::Number);
        assert_eq!(st.field("enabled").unwrap().typ, Type::Bool);
        assert_eq!(st.field("unset").unwrap().typ, Type::Optional(Box::new(Type::String)));
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
    fn eval_number_add() {
        let scope = Scope::new();
        let expr = Expr::Add(Box::new(Expr::Number(1.into())), Box::new(Expr::Number(2.into())));
        assert_eq!(eval(&expr, &scope).unwrap(), Value::Number(3.into()));
    }

    #[test]
    fn eval_string_add() {
        let scope = Scope::new();
        let expr = Expr::Add(
            Box::new(Expr::Str(vec![StringPart::Literal("hello ".into())])),
            Box::new(Expr::Str(vec![StringPart::Literal("world".into())])),
        );
        assert_eq!(eval(&expr, &scope).unwrap(), Value::Str("hello world".into()));
    }

    #[test]
    fn eval_add_type_mismatch() {
        let scope = Scope::new();
        let expr = Expr::Add(
            Box::new(Expr::Number(1.into())),
            Box::new(Expr::Str(vec![StringPart::Literal("x".into())])),
        );
        assert!(eval(&expr, &scope).is_err());
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
            Value::list(vec![Value::Number(1.into()), Value::Number(2.into())])
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
            Value::list(vec![
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
            Value::list(vec![
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
            Value::list(vec![Value::Str("a".into()), Value::Str("b".into()),])
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
            Value::list(vec![Value::Str("hello".into())])
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
    fn builtin_dynamic_values_roundtrip() {
        let missing = Value::Str("BIT_TEST_NONEXISTENT_VAR_12345".into());
        assert_eq!(
            call_builtin("env", &[missing.clone(), Value::Number(42.into())]).unwrap(),
            Value::Number(42.into())
        );
        assert_eq!(call_builtin("env", &[missing, Value::Null]).unwrap(), Value::Null);
        let precise = Value::Number("12345678901234567890.123456789".parse().unwrap());
        assert_eq!(
            call_builtin(
                "env",
                &[Value::Str("BIT_TEST_NONEXISTENT_VAR_12345".into()), precise.clone()]
            )
            .unwrap(),
            precise
        );
        let reference = Value::BlockRef("build[core]".into());
        assert_eq!(
            call_builtin(
                "env",
                &[Value::Str("BIT_TEST_NONEXISTENT_VAR_12345".into()), reference.clone()]
            )
            .unwrap(),
            reference.clone()
        );
        assert_eq!(
            call_builtin(
                "uniq",
                &[Value::list(vec![
                    Value::Number(1.into()),
                    Value::Number(1.into()),
                    Value::Number(2.into())
                ])]
            )
            .unwrap(),
            Value::list(vec![Value::Number(1.into()), Value::Number(2.into())])
        );
        assert_eq!(
            call_builtin(
                "uniq",
                &[Value::List(Type::BlockRef, vec![reference.clone(), reference.clone()])]
            )
            .unwrap(),
            Value::List(Type::BlockRef, vec![reference])
        );
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

    #[test]
    fn eval_basename_string() {
        let scope = Scope::new();
        let expr = Expr::Pipe(
            Box::new(Expr::Str(vec![StringPart::Literal("/usr/bin/test".into())])),
            "basename".into(),
            vec![],
        );
        assert_eq!(eval(&expr, &scope).unwrap(), Value::Str("test".into()));
    }

    #[test]
    fn eval_basename_list() {
        let scope = Scope::new();
        let expr = Expr::Pipe(
            Box::new(Expr::List(vec![
                Expr::Str(vec![StringPart::Literal("/a/b.txt".into())]),
                Expr::Str(vec![StringPart::Literal("/c/d.go".into())]),
            ])),
            "basename".into(),
            vec![],
        );
        assert_eq!(
            eval(&expr, &scope).unwrap(),
            Value::list(vec![Value::Str("b.txt".into()), Value::Str("d.go".into()),])
        );
    }

    #[test]
    fn eval_dirname_string() {
        let scope = Scope::new();
        let expr = Expr::Pipe(
            Box::new(Expr::Str(vec![StringPart::Literal("/usr/bin/test".into())])),
            "dirname".into(),
            vec![],
        );
        assert_eq!(eval(&expr, &scope).unwrap(), Value::Str("/usr/bin".into()));
    }

    #[test]
    fn eval_dirname_list() {
        let scope = Scope::new();
        let expr = Expr::Pipe(
            Box::new(Expr::List(vec![
                Expr::Str(vec![StringPart::Literal("/a/b.txt".into())]),
                Expr::Str(vec![StringPart::Literal("/c/d.go".into())]),
            ])),
            "dirname".into(),
            vec![],
        );
        assert_eq!(
            eval(&expr, &scope).unwrap(),
            Value::list(vec![Value::Str("/a".into()), Value::Str("/c".into()),])
        );
    }

    #[test]
    fn eval_prefix_string() {
        let scope = Scope::new();
        let expr = Expr::Pipe(
            Box::new(Expr::Str(vec![StringPart::Literal("amd64".into())])),
            "prefix".into(),
            vec![Expr::Str(vec![StringPart::Literal("linux/".into())])],
        );
        assert_eq!(eval(&expr, &scope).unwrap(), Value::Str("linux/amd64".into()));
    }

    #[test]
    fn eval_prefix_list() {
        let scope = Scope::new();
        let expr = Expr::Pipe(
            Box::new(Expr::List(vec![
                Expr::Str(vec![StringPart::Literal("amd64".into())]),
                Expr::Str(vec![StringPart::Literal("arm64".into())]),
            ])),
            "prefix".into(),
            vec![Expr::Str(vec![StringPart::Literal("linux/".into())])],
        );
        assert_eq!(
            eval(&expr, &scope).unwrap(),
            Value::list(vec![Value::Str("linux/amd64".into()), Value::Str("linux/arm64".into()),])
        );
    }

    #[test]
    fn eval_suffix_string() {
        let scope = Scope::new();
        let expr = Expr::Pipe(
            Box::new(Expr::Str(vec![StringPart::Literal("app".into())])),
            "suffix".into(),
            vec![Expr::Str(vec![StringPart::Literal(".exe".into())])],
        );
        assert_eq!(eval(&expr, &scope).unwrap(), Value::Str("app.exe".into()));
    }

    #[test]
    fn eval_suffix_list() {
        let scope = Scope::new();
        let expr = Expr::Pipe(
            Box::new(Expr::List(vec![
                Expr::Str(vec![StringPart::Literal("app".into())]),
                Expr::Str(vec![StringPart::Literal("lib".into())]),
            ])),
            "suffix".into(),
            vec![Expr::Str(vec![StringPart::Literal(".so".into())])],
        );
        assert_eq!(
            eval(&expr, &scope).unwrap(),
            Value::list(vec![Value::Str("app.so".into()), Value::Str("lib.so".into()),])
        );
    }

    #[test]
    fn eval_sha256() {
        let scope = Scope::new();
        let expr = Expr::Pipe(
            Box::new(Expr::Str(vec![StringPart::Literal("hello".into())])),
            "sha256".into(),
            vec![],
        );
        assert_eq!(
            eval(&expr, &scope).unwrap(),
            Value::Str("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824".into())
        );
    }
}
