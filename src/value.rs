use std::collections::HashMap;
use std::fmt;

use bigdecimal::BigDecimal;
use serde::de;
use serde::ser::SerializeSeq;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub type Map = HashMap<String, Value>;

/// A named, typed field within a struct — carries all metadata.
#[derive(Debug, Clone, PartialEq)]
pub struct StructField {
    pub typ: Type,
    pub default: Option<Value>,
    pub description: Option<String>,
}

/// A struct type with an optional description and ordered fields.
#[derive(Debug, Clone, PartialEq)]
pub struct StructType {
    pub description: Option<String>,
    pub fields: Vec<(String, StructField)>,
}

impl StructType {
    /// Look up a field by name.
    pub fn field(&self, name: &str) -> Option<&StructField> {
        self.fields.iter().find(|(n, _)| n == name).map(|(_, f)| f)
    }
}

/// Runtime value in the .bit language.
///
/// `List` and `Map` carry their element/value `Type` for homogeneous type checking.
/// `Struct` carries per-field types for heterogeneous named fields (e.g. block outputs).
/// Types are **not** serialised — they are inferred on deserialisation.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Number(BigDecimal),
    Str(String),
    List(Type, Vec<Value>),
    Map(Type, Map),
    Struct(StructType, Map),
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Str(s) => write!(f, "{s}"),
            Value::Number(n) => write!(f, "{n}"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::List(_, items) => {
                write!(f, "[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{item}")?;
                }
                write!(f, "]")
            }
            Value::Map(_, map) | Value::Struct(_, map) => {
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
    /// Construct a `List`, inferring the element type from the first item.
    ///
    /// Falls back to `Type::String` for empty lists.
    pub fn list(items: Vec<Value>) -> Self {
        let typ = items.first().map(Value::value_type).unwrap_or(Type::String);
        Value::List(typ, items)
    }

    /// Construct a `Struct` from a map, inferring per-field types from values.
    pub fn strct(map: Map) -> Self {
        let fields = map
            .iter()
            .map(|(k, v)| {
                (
                    k.clone(),
                    StructField {
                        typ: v.value_type(),
                        default: None,
                        description: None,
                    },
                )
            })
            .collect();
        Value::Struct(
            StructType {
                description: None,
                fields,
            },
            map,
        )
    }

    /// Infer the `Type` of this value.
    pub fn value_type(&self) -> Type {
        match self {
            Value::Null => Type::String,
            Value::Bool(_) => Type::Bool,
            Value::Number(_) => Type::Number,
            Value::Str(_) => Type::String,
            Value::List(typ, _) => Type::List(Box::new(typ.clone())),
            Value::Map(typ, _) => Type::Map(Box::new(typ.clone())),
            Value::Struct(st, _) => Type::Struct(st.clone()),
        }
    }

    /// Format as a `.bit` literal (strings are quoted).
    pub fn to_literal(&self) -> String {
        self.to_expr().to_string()
    }

    /// Convert a runtime Value into an AST Expr.
    pub fn to_expr(&self) -> crate::ast::Expr {
        use crate::ast::{Expr, Field, StringPart};
        match self {
            Value::Str(s) => Expr::Str(vec![StringPart::Literal(s.clone())]),
            Value::Number(n) => Expr::Number(n.clone()),
            Value::Bool(b) => Expr::Bool(*b),
            Value::Null => Expr::Null,
            Value::List(_, items) => Expr::List(items.iter().map(|v| v.to_expr()).collect()),
            Value::Map(_, map) | Value::Struct(_, map) => Expr::Map(
                map.iter()
                    .map(|(k, v)| Field {
                        name: k.clone(),
                        value: v.to_expr(),
                    })
                    .collect(),
            ),
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
            Value::List(_, items) => Some(items),
            _ => None,
        }
    }

    pub fn as_map(&self) -> Option<&Map> {
        match self {
            Value::Map(_, map) | Value::Struct(_, map) => Some(map),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }
}

/// Serialize a Value without the embedded Type — the wire format is unchanged.
impl Serialize for Value {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Value::Null => serializer.serialize_none(),
            Value::Bool(b) => serializer.serialize_bool(*b),
            Value::Number(n) => n.serialize(serializer),
            Value::Str(s) => serializer.serialize_str(s),
            Value::List(_, items) => {
                let mut seq = serializer.serialize_seq(Some(items.len()))?;
                for item in items {
                    seq.serialize_element(item)?;
                }
                seq.end()
            }
            Value::Map(_, map) | Value::Struct(_, map) => map.serialize(serializer),
        }
    }
}

/// Deserialize a Value, inferring the List element type from the first item.
impl<'de> Deserialize<'de> for Value {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        // Leverage serde_json::Value as an untagged intermediary, then convert.
        let raw = serde_json::Value::deserialize(deserializer)?;
        value_from_json(raw).map_err(de::Error::custom)
    }
}

/// Convert a `serde_json::Value` into our `Value`, inferring list element types.
fn value_from_json(raw: serde_json::Value) -> Result<Value, String> {
    match raw {
        serde_json::Value::Null => Ok(Value::Null),
        serde_json::Value::Bool(b) => Ok(Value::Bool(b)),
        serde_json::Value::Number(n) => {
            let s = n.to_string();
            let bd: BigDecimal = s.parse().map_err(|e| format!("invalid number: {e}"))?;
            Ok(Value::Number(bd))
        }
        serde_json::Value::String(s) => Ok(Value::Str(s)),
        serde_json::Value::Array(arr) => {
            let items: Result<Vec<Value>, String> = arr.into_iter().map(value_from_json).collect();
            Ok(Value::list(items?))
        }
        serde_json::Value::Object(obj) => {
            let map: Result<Map, String> = obj
                .into_iter()
                .map(|(k, v)| value_from_json(v).map(|val| (k, val)))
                .collect();
            Ok(Value::strct(map?))
        }
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
            Type::Struct(st) => {
                write!(f, "{{")?;
                for (i, (k, field)) in st.fields.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{k} = {}", field.typ)?;
                }
                write!(f, "}}")
            }
            Type::Optional(inner) => write!(f, "{inner}?"),
            Type::Path => write!(f, "path"),
            Type::Secret => write!(f, "secret"),
            Type::Union(types) => {
                for (i, t) in types.iter().enumerate() {
                    if i > 0 {
                        write!(f, " | ")?;
                    }
                    write!(f, "{t}")?;
                }
                Ok(())
            }
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
    Struct(StructType),
    Optional(Box<Type>),
    Path,
    Secret,
    Union(Vec<Type>),
}

/// Check that a value matches a declared type, recursively.
///
/// Returns a human-readable error message on mismatch.
pub fn validate_type(value: &Value, typ: &Type) -> Result<(), String> {
    match (typ, value) {
        (Type::String | Type::Path | Type::Secret, Value::Str(_)) => Ok(()),
        (Type::Number, Value::Number(_)) => Ok(()),
        (Type::Bool, Value::Bool(_)) => Ok(()),
        (Type::List(inner), Value::List(_, items)) => {
            for (i, item) in items.iter().enumerate() {
                validate_type(item, inner).map_err(|e| format!("[{i}]: {e}"))?;
            }
            Ok(())
        }
        (Type::Map(val_type), Value::Map(_, map)) => {
            for (k, v) in map {
                validate_type(v, val_type).map_err(|e| format!(".{k}: {e}"))?;
            }
            Ok(())
        }
        (Type::Struct(st), Value::Struct(_, map)) => {
            for (k, field) in &st.fields {
                match map.get(k) {
                    Some(v) => validate_type(v, &field.typ).map_err(|e| format!(".{k}: {e}"))?,
                    None if matches!(field.typ, Type::Optional(_)) => {}
                    None => return Err(format!("missing field: {k}")),
                }
            }
            Ok(())
        }
        (Type::Optional(_), Value::Null) => Ok(()),
        (Type::Optional(inner), _) => validate_type(value, inner),
        (Type::Union(variants), _) => {
            for variant in variants {
                if validate_type(value, variant).is_ok() {
                    return Ok(());
                }
            }
            let names: Vec<String> = variants.iter().map(|t| t.to_string()).collect();
            Err(format!("expected {}, got {}", names.join(" | "), type_name(value)))
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
        Value::List(..) => "list",
        Value::Map(..) => "map",
        Value::Struct(..) => "struct",
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
        let list = Value::list(vec![Value::Number(1.into()), Value::Number(2.into())]);
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
        assert!(Value::list(vec![]).as_list().is_some());
        assert!(Value::Map(Type::String, Map::new()).as_map().is_some());
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
        let val = Value::list(vec![Value::Str("a".into()), Value::Str("b".into())]);
        assert!(validate_type(&val, &Type::List(Box::new(Type::String))).is_ok());
    }

    #[test]
    fn validate_list_element_mismatch() {
        let val = Value::list(vec![Value::Str("a".into()), Value::Number(1.into())]);
        let err = validate_type(&val, &Type::List(Box::new(Type::String))).unwrap_err();
        assert!(err.contains("[1]"), "error should reference index: {err}");
    }

    #[test]
    fn validate_map() {
        let mut m = Map::new();
        m.insert("a".into(), Value::Number(1.into()));
        m.insert("b".into(), Value::Number(2.into()));
        assert!(validate_type(&Value::Map(Type::Number, m), &Type::Map(Box::new(Type::Number))).is_ok());
    }

    #[test]
    fn validate_map_value_mismatch() {
        let mut m = Map::new();
        m.insert("a".into(), Value::Number(1.into()));
        m.insert("b".into(), Value::Str("oops".into()));
        let err = validate_type(&Value::Map(Type::Number, m), &Type::Map(Box::new(Type::Number))).unwrap_err();
        assert!(err.contains(".b"), "error should reference key: {err}");
    }

    #[test]
    fn validate_nested() {
        let val = Value::list(vec![
            Value::list(vec![Value::Number(1.into())]),
            Value::list(vec![Value::Number(2.into())]),
        ]);
        let typ = Type::List(Box::new(Type::List(Box::new(Type::Number))));
        assert!(validate_type(&val, &typ).is_ok());
    }

    #[test]
    fn validate_empty_list_ok() {
        assert!(validate_type(&Value::list(vec![]), &Type::List(Box::new(Type::String))).is_ok());
    }

    #[test]
    fn validate_empty_map_ok() {
        assert!(
            validate_type(
                &Value::Map(Type::String, Map::new()),
                &Type::Map(Box::new(Type::String))
            )
            .is_ok()
        );
    }

    #[test]
    fn validate_union_string_matches() {
        let typ = Type::Union(vec![Type::String, Type::List(Box::new(Type::String))]);
        assert!(validate_type(&Value::Str("hello".into()), &typ).is_ok());
    }

    #[test]
    fn validate_union_list_matches() {
        let typ = Type::Union(vec![Type::String, Type::List(Box::new(Type::String))]);
        let val = Value::list(vec![Value::Str("a".into())]);
        assert!(validate_type(&val, &typ).is_ok());
    }

    #[test]
    fn validate_union_mismatch() {
        let typ = Type::Union(vec![Type::String, Type::Bool]);
        let err = validate_type(&Value::Number(1.into()), &typ).unwrap_err();
        assert!(err.contains("string | bool"), "error: {err}");
    }

    #[test]
    fn display_union_type() {
        let typ = Type::Union(vec![Type::String, Type::List(Box::new(Type::String))]);
        assert_eq!(typ.to_string(), "string | [string]");
    }
}
