use std::collections::HashMap;
use std::fmt;

pub type Map = HashMap<String, Value>;

#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Str(String),
    Int(i64),
    Bool(bool),
    List(Vec<Value>),
    Map(Map),
    Null,
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Str(s) => write!(f, "{s}"),
            Value::Int(n) => write!(f, "{n}"),
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
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            Value::Int(n) => Some(*n),
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

    /// Check whether this value conforms to a type.
    /// Returns `Ok(())` if it matches, or an error describing the path to the mismatch.
    pub fn check_type(&self, typ: &Type) -> Result<(), String> {
        self.check_type_at(typ, "")
    }

    fn check_type_at(&self, typ: &Type, path: &str) -> Result<(), String> {
        match (self, typ) {
            (_, Type::Any) => Ok(()),
            (Value::Str(_), Type::String | Type::Path | Type::Secret) => Ok(()),
            (Value::Int(_), Type::Int) => Ok(()),
            (Value::Bool(_), Type::Bool) => Ok(()),
            (Value::List(items), Type::List(inner)) => {
                for (i, item) in items.iter().enumerate() {
                    item.check_type_at(inner, &format!("{path}[{i}]"))?;
                }
                Ok(())
            }
            (Value::Map(_), Type::Map) => Ok(()),
            (Value::Map(map), Type::Struct { fields, .. }) => {
                for field in fields {
                    let field_path = if path.is_empty() {
                        field.name.clone()
                    } else {
                        format!("{path}.{}", field.name)
                    };
                    match map.get(&field.name) {
                        Some(v) => v.check_type_at(&field.typ, &field_path)?,
                        None => {
                            if field.required && field.default.is_none() {
                                return Err(format!("{field_path}: missing required field"));
                            }
                        }
                    }
                }
                Ok(())
            }
            _ => {
                let at = if path.is_empty() {
                    String::new()
                } else {
                    format!("{path}: ")
                };
                Err(format!(
                    "{at}expected {}, got {}",
                    type_name(typ),
                    value_type_name(self),
                ))
            }
        }
    }
}

/// A field within a struct type.
#[derive(Debug, Clone, PartialEq)]
pub struct FieldDef {
    pub name: String,
    pub description: String,
    pub typ: Type,
    pub required: bool,
    pub default: Option<Value>,
}

impl FieldDef {
    pub fn defaults() -> Self {
        Self {
            name: String::new(),
            description: String::new(),
            typ: Type::Any,
            required: true,
            default: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Type {
    Any,
    String,
    Int,
    Bool,
    List(Box<Type>),
    Map,
    Path,
    Secret,
    Struct { description: String, fields: Vec<FieldDef> },
}

impl Type {
    /// Validate a map of values against this type (must be Struct).
    /// Returns a list of errors, empty if valid.
    pub fn validate(&self, values: &Map) -> Vec<String> {
        let Type::Struct { fields, .. } = self else {
            return vec!["validate() called on non-struct type".into()];
        };
        let mut errors = Vec::new();

        // Type-check each field
        if let Err(e) = Value::Map(values.clone()).check_type(self) {
            errors.push(e);
        }

        // Check for unknown fields
        let known: std::collections::HashSet<&str> = fields.iter().map(|f| f.name.as_str()).collect();
        for key in values.keys() {
            if !known.contains(key.as_str()) {
                errors.push(format!("unknown field '{key}'"));
            }
        }

        errors
    }

    /// Apply defaults to a map, filling in missing optional fields.
    pub fn apply_defaults(&self, values: &mut Map) {
        let Type::Struct { fields, .. } = self else {
            return;
        };
        for field in fields {
            if !values.contains_key(&field.name) {
                if let Some(default) = &field.default {
                    values.insert(field.name.clone(), default.clone());
                }
            }
        }
    }
}

fn type_name(typ: &Type) -> &'static str {
    match typ {
        Type::Any => "any",
        Type::String => "string",
        Type::Int => "int",
        Type::Bool => "bool",
        Type::List(_) => "list",
        Type::Map => "map",
        Type::Path => "path",
        Type::Secret => "secret",
        Type::Struct { .. } => "struct",
    }
}

fn value_type_name(val: &Value) -> &'static str {
    match val {
        Value::Str(_) => "string",
        Value::Int(_) => "int",
        Value::Bool(_) => "bool",
        Value::List(_) => "list",
        Value::Map(_) => "map",
        Value::Null => "null",
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
    fn display_int() {
        assert_eq!(Value::Int(42).to_string(), "42");
    }

    #[test]
    fn display_bool() {
        assert_eq!(Value::Bool(true).to_string(), "true");
    }

    #[test]
    fn display_list() {
        let list = Value::List(vec![Value::Int(1), Value::Int(2)]);
        assert_eq!(list.to_string(), "[1, 2]");
    }

    #[test]
    fn display_null() {
        assert_eq!(Value::Null.to_string(), "null");
    }

    #[test]
    fn accessors() {
        assert_eq!(Value::Str("hi".into()).as_str(), Some("hi"));
        assert_eq!(Value::Int(5).as_int(), Some(5));
        assert_eq!(Value::Bool(false).as_bool(), Some(false));
        assert!(Value::List(vec![]).as_list().is_some());
        assert!(Value::Map(Map::new()).as_map().is_some());
        assert!(Value::Null.is_null());
    }

    #[test]
    fn accessor_wrong_type_returns_none() {
        assert_eq!(Value::Int(1).as_str(), None);
        assert_eq!(Value::Str("x".into()).as_int(), None);
        assert_eq!(Value::Null.as_bool(), None);
        assert!(Value::Int(1).as_list().is_none());
        assert!(Value::Int(1).as_map().is_none());
        assert!(!Value::Int(1).is_null());
    }

    fn exec_input_type() -> Type {
        Type::Struct {
            description: String::new(),
            fields: vec![
                FieldDef {
                    name: "command".into(),
                    typ: Type::String,
                    ..FieldDef::defaults()
                },
                FieldDef {
                    name: "output".into(),
                    typ: Type::String,
                    ..FieldDef::defaults()
                },
                FieldDef {
                    name: "inputs".into(),
                    typ: Type::List(Box::new(Type::String)),
                    required: false,
                    default: Some(Value::List(vec![])),
                    ..FieldDef::defaults()
                },
            ],
        }
    }

    #[test]
    fn validate_valid_struct() {
        let typ = exec_input_type();
        let mut map = Map::new();
        map.insert("command".into(), Value::Str("echo hi".into()));
        map.insert("output".into(), Value::Str("out".into()));
        assert!(typ.validate(&map).is_empty());
    }

    #[test]
    fn validate_missing_required() {
        let typ = exec_input_type();
        let mut map = Map::new();
        map.insert("command".into(), Value::Str("echo hi".into()));
        let errors = typ.validate(&map);
        assert!(
            errors
                .iter()
                .any(|e| e.contains("output") && e.contains("missing required"))
        );
    }

    #[test]
    fn validate_unknown_field() {
        let typ = exec_input_type();
        let mut map = Map::new();
        map.insert("command".into(), Value::Str("echo".into()));
        map.insert("output".into(), Value::Str("out".into()));
        map.insert("bogus".into(), Value::Int(42));
        let errors = typ.validate(&map);
        assert!(errors.iter().any(|e| e.contains("unknown field 'bogus'")));
    }

    #[test]
    fn validate_wrong_type() {
        let typ = exec_input_type();
        let mut map = Map::new();
        map.insert("command".into(), Value::Int(42));
        map.insert("output".into(), Value::Str("out".into()));
        let errors = typ.validate(&map);
        assert!(errors.iter().any(|e| e.contains("expected string")));
    }

    #[test]
    fn apply_defaults_fills_missing() {
        let typ = exec_input_type();
        let mut map = Map::new();
        map.insert("command".into(), Value::Str("echo".into()));
        map.insert("output".into(), Value::Str("out".into()));
        typ.apply_defaults(&mut map);
        assert_eq!(map.get("inputs"), Some(&Value::List(vec![])));
    }

    #[test]
    fn check_type_basic() {
        assert!(Value::Str("hi".into()).check_type(&Type::String).is_ok());
        assert!(Value::Int(1).check_type(&Type::Int).is_ok());
        assert!(Value::Bool(true).check_type(&Type::Bool).is_ok());
        let err = Value::Str("hi".into()).check_type(&Type::Int).unwrap_err();
        assert!(err.contains("expected int"), "{err}");
        assert!(err.contains("got string"), "{err}");
    }

    #[test]
    fn check_type_any() {
        assert!(Value::Str("hi".into()).check_type(&Type::Any).is_ok());
        assert!(Value::Int(1).check_type(&Type::Any).is_ok());
        assert!(Value::Null.check_type(&Type::Any).is_ok());
    }

    #[test]
    fn check_type_nested_path() {
        let typ = Type::Struct {
            description: String::new(),
            fields: vec![FieldDef {
                name: "config".into(),
                typ: Type::Struct {
                    description: String::new(),
                    fields: vec![FieldDef {
                        name: "port".into(),
                        typ: Type::Int,
                        ..FieldDef::defaults()
                    }],
                },
                ..FieldDef::defaults()
            }],
        };
        let mut inner = Map::new();
        inner.insert("port".into(), Value::Str("not an int".into()));
        let mut outer = Map::new();
        outer.insert("config".into(), Value::Map(inner));
        let err = Value::Map(outer).check_type(&typ).unwrap_err();
        assert!(err.contains("config.port"), "{err}");
    }

    #[test]
    fn check_type_list_index() {
        let typ = Type::List(Box::new(Type::Int));
        let val = Value::List(vec![Value::Int(1), Value::Str("bad".into()), Value::Int(3)]);
        let err = val.check_type(&typ).unwrap_err();
        assert!(err.contains("[1]"), "{err}");
    }
}
