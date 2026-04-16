use bigdecimal::BigDecimal;

use crate::value::Type;

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
    Block(Block),
    Let(Let),
    Param(Param),
    Target(Target),
    Output(Output),
}

/// Execution phase for a block.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Phase {
    Pre,
    #[default]
    Default,
    Post,
}

/// `[pre|post] [protected] name = provider.resource { fields... }`
/// or `name[key1, key2] = provider.resource { fields... }` (matrix expansion)
#[derive(Debug, Clone, PartialEq)]
pub struct Block {
    pub pos: Pos,
    pub name: String,
    pub doc: Option<String>,
    pub phase: Phase,
    pub protected: bool,
    /// Matrix expansion keys — list params to expand over.
    pub matrix_keys: Vec<String>,
    pub provider: String,
    pub resource: String,
    pub fields: Vec<Field>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Field {
    pub name: String,
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

/// `target name = [block1, block2]`
#[derive(Debug, Clone, PartialEq)]
pub struct Target {
    pub pos: Pos,
    pub name: String,
    pub doc: Option<String>,
    pub blocks: Vec<String>,
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
                        StringPart::Interpolation(e) => write!(f, "${{{e}}}")?,
                    }
                }
                write!(f, "\"")
            }
            Expr::Number(n) => write!(f, "{n}"),
            Expr::Bool(b) => write!(f, "{b}"),
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
    Null,
    /// `[a, b, c]`
    List(Vec<Expr>),
    /// `{ key = value, ... }`
    Map(Vec<Field>),
    /// Variable or block reference: `name` or `block.field`
    Ref(Vec<String>),
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
    fn build_simple_block() {
        let block = Block {
            pos: Pos::default(),
            name: "server".into(),
            doc: None,
            phase: Phase::Default,
            protected: false,
            matrix_keys: vec![],
            provider: "go".into(),
            resource: "binary".into(),
            fields: vec![Field {
                name: "main".into(),
                value: Expr::Str(vec![StringPart::Literal("./cmd/server".into())]),
            }],
        };
        assert_eq!(block.name, "server");
        assert_eq!(block.provider, "go");
        assert_eq!(block.resource, "binary");
        assert!(!block.protected);
        assert_eq!(block.fields.len(), 1);
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
                Statement::Target(Target {
                    pos: Pos::default(),
                    name: "build".into(),
                    doc: None,
                    blocks: vec!["server".into(), "image".into()],
                }),
            ],
        };
        assert_eq!(module.statements.len(), 3);
    }
}
