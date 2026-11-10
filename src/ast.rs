use bigdecimal::BigDecimal;

use crate::value::{Duration, Type};

/// Source position (1-indexed line and column).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Pos {
    pub file: String,
    pub line: usize,
    pub col: usize,
}

impl std::fmt::Display for Pos {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}:{}", self.file, self.line, self.col)
    }
}

/// A parsed `.bit` file.
#[derive(Debug, Clone, PartialEq)]
pub struct Module {
    /// Leading doc comment at the top of the file (before any statement).
    pub doc: Option<String>,
    pub statements: Vec<Statement>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    Import(Import),
    Target(Target),
    Let(Let),
    Param(Param),
    Group(Group),
    Output(Output),
}

/// `import "<host/path>" [as <ident>]`
#[derive(Debug, Clone, PartialEq)]
pub struct Import {
    pub pos: Pos,
    pub url: String,
    /// Override for the auto-derived provider name. When `None`, the
    /// provider name is the last path segment of `url`.
    pub alias: Option<String>,
}

/// Execution phase for a target.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Phase {
    Pre,
    #[default]
    Default,
    Post,
}

impl std::fmt::Display for Phase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Phase::Pre => "pre",
            Phase::Default => "default",
            Phase::Post => "post",
        })
    }
}

/// `[pre|post] [protected] [explicit] [large] name = provider.resource { fields... }`
/// or `name[key1, key2 = expr] = provider.resource { fields... }` (matrix expansion).
///
/// `protected`, `explicit`, and `large` may appear in any order and are independent:
/// `protected` targets must be named directly to destroy; `explicit` targets are
/// excluded from the `...` selector and must be named to run; `large` targets
/// are long-running or resource-heavy.
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub pos: Pos,
    pub name: String,
    /// Inputs supplied when this target is invoked by a group or the CLI.
    pub params: Vec<Param>,
    pub doc: Option<String>,
    pub phase: Phase,
    pub protected: bool,
    pub explicit: bool,
    pub large: bool,
    /// Matrix expansion keys, in declaration order.
    pub matrix_keys: Vec<MatrixKey>,
    pub provider: String,
    pub resource: String,
    pub fields: Vec<Field>,
}

impl Target {
    pub fn matrix_key_names(&self) -> Vec<String> {
        self.matrix_keys.iter().map(|key| key.name.clone()).collect()
    }
}

/// One matrix axis.
///
/// `values` is evaluated per combination of the earlier keys, so its list may depend on them.
#[derive(Debug, Clone, PartialEq)]
pub struct MatrixKey {
    pub name: String,
    pub values: Option<Expr>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub name: String,
    pub value: Expr,
}

#[derive(Debug, Clone, PartialEq)]
pub struct MapEntry {
    pub name: String,
    pub typ: Option<Type>,
    pub value: Expr,
}

/// `let name = expr` or `let name : type = expr`
#[derive(Debug, Clone, PartialEq)]
pub struct Let {
    pub pos: Pos,
    pub name: String,
    pub typ: Option<Type>,
    pub value: Expr,
}

/// `param name : type` or `param name : type = default`
#[derive(Debug, Clone, PartialEq)]
pub struct Param {
    pub pos: Pos,
    pub name: String,
    pub doc: Option<String>,
    pub typ: Type,
    pub default: Option<Expr>,
}

/// `group name = [target1, target2]`
#[derive(Debug, Clone, PartialEq)]
pub struct Group {
    pub pos: Pos,
    pub name: String,
    pub doc: Option<String>,
    pub params: Vec<Param>,
    pub members: Vec<GroupMember>,
}

/// A target or group selected from a group definition.
#[derive(Debug, Clone, PartialEq)]
pub struct GroupMember {
    pub name: String,
    pub keys: Option<Vec<Expr>>,
    pub args: Vec<Field>,
}

/// `output name = expr`
#[derive(Debug, Clone, PartialEq)]
pub struct Output {
    pub pos: Pos,
    pub name: String,
    pub doc: Option<String>,
    pub value: Expr,
}

impl std::fmt::Display for Expr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Expr::Str(parts) => {
                write!(f, "\"")?;
                for part in parts {
                    match part {
                        StringPart::Literal(s) => {
                            for c in s.chars() {
                                match c {
                                    '"' => write!(f, "\\\"")?,
                                    '\\' => write!(f, "\\\\")?,
                                    '\n' => write!(f, "\\n")?,
                                    '\t' => write!(f, "\\t")?,
                                    '\r' => write!(f, "\\r")?,
                                    c => write!(f, "{c}")?,
                                }
                            }
                        }
                        StringPart::Interpolation(e) => write!(f, "#{{{e}}}")?,
                    }
                }
                write!(f, "\"")
            }
            Expr::Number(n) => write!(f, "{n}"),
            Expr::Bool(b) => write!(f, "{b}"),
            Expr::Duration(d) => write!(f, "{d}"),
            Expr::Null => write!(f, "null"),
            Expr::List(items) => {
                write!(f, "[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{item}")?;
                }
                write!(f, "]")
            }
            Expr::Ref(parts) => write!(f, "{}", parts.join(".")),
            Expr::TargetRef(name) => write!(f, "{name}"),
            Expr::MatrixRef { name, keys, fields } => {
                write!(f, "{name}[")?;
                for (index, key) in keys.iter().enumerate() {
                    if index > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{key}")?;
                }
                write!(f, "]")?;
                for field in fields {
                    write!(f, ".{field}")?;
                }
                Ok(())
            }
            Expr::TargetCall {
                name,
                keys,
                args,
                fields,
            } => {
                write!(f, "{name}")?;
                if let Some(keys) = keys {
                    write!(f, "[")?;
                    for (index, key) in keys.iter().enumerate() {
                        if index > 0 {
                            write!(f, ", ")?;
                        }
                        write!(f, "{key}")?;
                    }
                    write!(f, "]")?;
                }
                write!(f, "(")?;
                for (index, arg) in args.iter().enumerate() {
                    if index > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{} = {}", arg.name, arg.value)?;
                }
                write!(f, ")")?;
                for field in fields {
                    write!(f, ".{field}")?;
                }
                Ok(())
            }
            Expr::Call(name, args) => {
                write!(f, "{name}(")?;
                for (i, arg) in args.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{arg}")?;
                }
                write!(f, ")")
            }
            _ => write!(f, "..."),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// String literal, possibly with interpolated expressions.
    Str(Vec<StringPart>),
    Number(BigDecimal),
    Bool(bool),
    Duration(Duration),
    Null,
    /// `[a, b, c]`
    List(Vec<Expr>),
    /// `{ key = value, typed: type = value, ... }`
    Map(Vec<MapEntry>),
    /// Variable or target reference: `name` or `target.field`
    Ref(Vec<String>),
    /// A typed target-reference value.
    TargetRef(String),
    /// A matrix slice selected by typed expressions: `name[key].field`.
    MatrixRef {
        name: String,
        keys: Vec<Expr>,
        fields: Vec<String>,
    },
    /// A parameterized target reference: `name(arg = value).field`.
    TargetCall {
        name: String,
        keys: Option<Vec<Expr>>,
        args: Vec<Field>,
        fields: Vec<String>,
    },
    /// `func(args...)`
    Call(String, Vec<Expr>),
    /// `expr | pipe` or `expr | pipe(args...)`
    Pipe(Box<Expr>, String, Vec<Expr>),
    /// `if cond then a else b`
    If(Box<Expr>, Box<Expr>, Box<Expr>),
    /// `a == b`, `a != b`
    BinOp(Box<Expr>, BinOp, Box<Expr>),
    /// `a + b` for list concatenation
    Add(Box<Expr>, Box<Expr>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum StringPart {
    Literal(String),
    Interpolation(Expr),
}

#[derive(Debug, Clone, PartialEq)]
pub enum BinOp {
    Eq,
    Ne,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_simple_target() {
        let target = Target {
            pos: Pos::default(),
            name: "server".into(),
            params: vec![],
            doc: None,
            phase: Phase::Default,
            protected: false,
            explicit: false,
            large: false,
            matrix_keys: vec![],
            provider: "go".into(),
            resource: "binary".into(),
            fields: vec![Field {
                name: "main".into(),
                value: Expr::Str(vec![StringPart::Literal("./cmd/server".into())]),
            }],
        };
        assert_eq!(target.name, "server");
        assert_eq!(target.provider, "go");
        assert_eq!(target.resource, "binary");
        assert!(!target.protected);
        assert_eq!(target.fields.len(), 1);
    }

    #[test]
    fn build_interpolated_string() {
        let expr = Expr::Str(vec![
            StringPart::Literal("myapp:".into()),
            StringPart::Interpolation(Expr::Ref(vec!["git_sha".into()])),
        ]);
        match &expr {
            Expr::Str(parts) => assert_eq!(parts.len(), 2),
            _ => panic!("expected Str"),
        }
    }

    #[test]
    fn build_pipe_chain() {
        // exec("cmd") | trim | lines
        let expr = Expr::Pipe(
            Box::new(Expr::Pipe(
                Box::new(Expr::Call(
                    "exec".into(),
                    vec![Expr::Str(vec![StringPart::Literal("cmd".into())])],
                )),
                "trim".into(),
                vec![],
            )),
            "lines".into(),
            vec![],
        );
        match &expr {
            Expr::Pipe(inner, name, _) => {
                assert_eq!(name, "lines");
                match inner.as_ref() {
                    Expr::Pipe(_, name, _) => assert_eq!(name, "trim"),
                    _ => panic!("expected inner Pipe"),
                }
            }
            _ => panic!("expected Pipe"),
        }
    }

    #[test]
    fn build_if_expr() {
        let expr = Expr::If(
            Box::new(Expr::BinOp(
                Box::new(Expr::Ref(vec!["env".into()])),
                BinOp::Eq,
                Box::new(Expr::Str(vec![StringPart::Literal("prod".into())])),
            )),
            Box::new(Expr::Number(3.into())),
            Box::new(Expr::Number(1.into())),
        );
        match &expr {
            Expr::If(cond, _, _) => {
                assert!(matches!(cond.as_ref(), Expr::BinOp(_, BinOp::Eq, _)));
            }
            _ => panic!("expected If"),
        }
    }

    #[test]
    fn build_module() {
        let module = Module {
            doc: None,
            statements: vec![
                Statement::Param(Param {
                    pos: Pos::default(),
                    name: "env".into(),
                    doc: None,
                    typ: Type::String,
                    default: None,
                }),
                Statement::Let(Let {
                    pos: Pos::default(),
                    name: "sha".into(),
                    typ: None,
                    value: Expr::Call(
                        "exec".into(),
                        vec![Expr::Str(vec![StringPart::Literal("git rev-parse HEAD".into())])],
                    ),
                }),
                Statement::Group(Group {
                    pos: Pos::default(),
                    name: "build".into(),
                    doc: None,
                    params: vec![],
                    members: vec![
                        GroupMember {
                            name: "server".into(),
                            keys: None,
                            args: vec![],
                        },
                        GroupMember {
                            name: "image".into(),
                            keys: None,
                            args: vec![],
                        },
                    ],
                }),
            ],
        };
        assert_eq!(module.statements.len(), 3);
    }
}
