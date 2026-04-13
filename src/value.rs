use std::collections::HashMap;
use std::fmt;

use bigdecimal::BigDecimal;
use serde::{Deserialize, Serialize};

pub type Map = HashMap<String, Value>;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Value {
    Null,
    Bool(bool),
    Number(BigDecimal),
    Str(String),
    List(Vec<Value>),
    Map(Map),
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Str(s) => write!(f, "{s}"),
            Value::Number(n) => write!(f, "{n}"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::List(items) => {
                write!(f, "[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{item}")?;
                }
                write!(f, "]")
            }
            Value::Map(map) => {
                write!(f, "{{")?;
                for (i, (k, v)) in map.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{k} = {v}")?;
                }
                write!(f, "}}")
            }
            Value::Null => write!(f, "null"),
        }
    }
}

impl Value {
    /// Format as a `.bit` literal (strings are quoted).
    pub fn to_literal(&self) -> String {
        match self {
            Value::Str(s) => format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")),
            Value::Number(n) => n.to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Null => "null".into(),
            Value::List(items) => {
                let inner: Vec<String> = items.iter().map(|v| v.to_literal()).collect();
                format!("[{}]", inner.join(", "))
            }
            Value::Map(map) => {
                let inner: Vec<String> = map.iter().map(|(k, v)| format!("{k} = {}", v.to_literal())).collect();
                format!("{{{}}}", inner.join(", "))
            }
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_number(&self) -> Option<&BigDecimal> {
        match self {
            Value::Number(n) => Some(n),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&[Value]> {
        match self {
            Value::List(items) => Some(items),
            _ => None,
        }
    }

    pub fn as_map(&self) -> Option<&Map> {
        match self {
            Value::Map(map) => Some(map),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }
}

impl std::fmt::Display for Type {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Type::String => write!(f, "string"),
            Type::Number => write!(f, "number"),
            Type::Bool => write!(f, "bool"),
            Type::List(inner) => write!(f, "[{inner}]"),
            Type::Map(inner) => write!(f, "{{string = {inner}}}"),
            Type::Path => write!(f, "path"),
            Type::Secret => write!(f, "secret"),
        }
    }
}

/// Types used in the .bit language for param declarations.
#[derive(Debug, Clone, PartialEq)]
pub enum Type {
    String,
    Number,
    Bool,
    List(Box<Type>),
    Map(Box<Type>),
    Path,
    Secret,
}

/// Check that a value matches a declared type, recursively.
///
/// Returns a human-readable error message on mismatch.
pub fn validate_type(value: &Value, typ: &Type) -> Result<(), String> {
    match (typ, value) {
        (Type::String | Type::Path | Type::Secret, Value::Str(_)) => Ok(()),
        (Type::Number, Value::Number(_)) => Ok(()),
        (Type::Bool, Value::Bool(_)) => Ok(()),
        (Type::List(inner), Value::List(items)) => {
            for (i, item) in items.iter().enumerate() {
                validate_type(item, inner).map_err(|e| format!("[{i}]: {e}"))?;
            }
            Ok(())
        }
        (Type::Map(val_type), Value::Map(map)) => {
            for (k, v) in map {
                validate_type(v, val_type).map_err(|e| format!(".{k}: {e}"))?;
            }
            Ok(())
        }
        _ => Err(format!("expected {typ}, got {}", type_name(value))),
    }
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "bool",
        Value::Number(_) => "number",
        Value::Str(_) => "string",
        Value::List(_) => "list",
        Value::Map(_) => "map",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_str() {
        assert_eq!(Value::Str("hello".into()).to_string(), "hello");
    }

    #[test]
    fn display_number() {
        assert_eq!(Value::Number(42.into()).to_string(), "42");
    }

    #[test]
    fn display_bool() {
        assert_eq!(Value::Bool(true).to_string(), "true");
    }

    #[test]
    fn display_list() {
        let list = Value::List(vec![Value::Number(1.into()), Value::Number(2.into())]);
        assert_eq!(list.to_string(), "[1, 2]");
    }

    #[test]
    fn display_null() {
        assert_eq!(Value::Null.to_string(), "null");
    }

    #[test]
    fn accessors() {
        assert_eq!(Value::Str("hi".into()).as_str(), Some("hi"));
        assert_eq!(Value::Number(5.into()).as_number(), Some(&BigDecimal::from(5)));
        assert_eq!(Value::Bool(false).as_bool(), Some(false));
        assert!(Value::List(vec![]).as_list().is_some());
        assert!(Value::Map(Map::new()).as_map().is_some());
        assert!(Value::Null.is_null());
    }

    #[test]
    fn accessor_wrong_type_returns_none() {
        assert_eq!(Value::Number(1.into()).as_str(), None);
        assert_eq!(Value::Str("x".into()).as_number(), None);
        assert_eq!(Value::Null.as_bool(), None);
        assert!(Value::Number(1.into()).as_list().is_none());
        assert!(Value::Number(1.into()).as_map().is_none());
        assert!(!Value::Number(1.into()).is_null());
    }

    #[test]
    fn validate_scalar_types() {
        assert!(validate_type(&Value::Str("hi".into()), &Type::String).is_ok());
        assert!(validate_type(&Value::Number(1.into()), &Type::Number).is_ok());
        assert!(validate_type(&Value::Bool(true), &Type::Bool).is_ok());
        assert!(validate_type(&Value::Str("/tmp".into()), &Type::Path).is_ok());
        assert!(validate_type(&Value::Str("s3cr3t".into()), &Type::Secret).is_ok());
    }

    #[test]
    fn validate_scalar_mismatch() {
        assert!(validate_type(&Value::Str("hi".into()), &Type::Number).is_err());
        assert!(validate_type(&Value::Number(1.into()), &Type::String).is_err());
        assert!(validate_type(&Value::Bool(true), &Type::Number).is_err());
    }

    #[test]
    fn validate_list() {
        let val = Value::List(vec![Value::Str("a".into()), Value::Str("b".into())]);
        assert!(validate_type(&val, &Type::List(Box::new(Type::String))).is_ok());
    }

    #[test]
    fn validate_list_element_mismatch() {
        let val = Value::List(vec![Value::Str("a".into()), Value::Number(1.into())]);
        let err = validate_type(&val, &Type::List(Box::new(Type::String))).unwrap_err();
        assert!(err.contains("[1]"), "error should reference index: {err}");
    }

    #[test]
    fn validate_map() {
        let mut m = Map::new();
        m.insert("a".into(), Value::Number(1.into()));
        m.insert("b".into(), Value::Number(2.into()));
        assert!(validate_type(&Value::Map(m), &Type::Map(Box::new(Type::Number))).is_ok());
    }

    #[test]
    fn validate_map_value_mismatch() {
        let mut m = Map::new();
        m.insert("a".into(), Value::Number(1.into()));
        m.insert("b".into(), Value::Str("oops".into()));
        let err = validate_type(&Value::Map(m), &Type::Map(Box::new(Type::Number))).unwrap_err();
        assert!(err.contains(".b"), "error should reference key: {err}");
    }

    #[test]
    fn validate_nested() {
        let val = Value::List(vec![
            Value::List(vec![Value::Number(1.into())]),
            Value::List(vec![Value::Number(2.into())]),
        ]);
        let typ = Type::List(Box::new(Type::List(Box::new(Type::Number))));
        assert!(validate_type(&val, &typ).is_ok());
    }

    #[test]
    fn validate_empty_list_ok() {
        assert!(validate_type(&Value::List(vec![]), &Type::List(Box::new(Type::String))).is_ok());
    }

    #[test]
    fn validate_empty_map_ok() {
        assert!(validate_type(&Value::Map(Map::new()), &Type::Map(Box::new(Type::String))).is_ok());
    }
}
