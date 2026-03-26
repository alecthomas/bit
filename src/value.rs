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
}

/// Types used in the .bit language for param declarations.
#[derive(Debug, Clone, PartialEq)]
pub enum Type {
    String,
    Int,
    Bool,
    List(Box<Type>),
    Map,
    Path,
    Secret,
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
}
