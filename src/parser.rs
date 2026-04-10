use winnow::combinator::{alt, cut_err, delimited, opt, preceded, repeat, separated};
use winnow::error::{ContextError, ErrMode, StrContext};
use winnow::prelude::*;
use winnow::stream::Stream;
use winnow::token::{any, take_while};

use crate::ast::*;
use crate::value::Type;

#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct ParseError {
    pub message: String,
}

pub fn parse(input: &str, filename: &str) -> Result<Module, ParseError> {
    module.parse(input).map_err(|e| {
        let message = format_parse_error(input, filename, e.offset(), e.inner());
        ParseError { message }
    })
}

fn format_parse_error(input: &str, filename: &str, position: usize, err: &ContextError) -> String {
    let prefix = &input[..position];
    let line = prefix.chars().filter(|&c| c == '\n').count() + 1;
    let col = prefix.len() - prefix.rfind('\n').map(|i| i + 1).unwrap_or(0) + 1;

    let source_line = input[prefix.rfind('\n').map(|i| i + 1).unwrap_or(0)..]
        .lines()
        .next()
        .unwrap_or("");

    let context = err.to_string();
    // Take the last (most specific) context line
    let detail = context
        .lines()
        .last()
        .filter(|s| !s.is_empty())
        .unwrap_or("unexpected token");
    let mut msg = format!("{filename}:{line}:{col}: {detail}");
    msg.push('\n');
    msg.push_str(&format!("  {source_line}\n"));
    msg.push_str(&format!("  {:>width$}", "^", width = col));
    msg
}

// ── Whitespace & Comments ──

fn ws(input: &mut &str) -> ModalResult<()> {
    take_while(0.., |c: char| c == ' ' || c == '\t' || c == '\r' || c == '\n')
        .void()
        .parse_next(input)
}

/// Collect adjacent `# ...` comment lines as a doc string.
/// Only consumes comment lines — leaves other whitespace to `ws`/`lex`.
fn doc_comments(input: &mut &str) -> ModalResult<Option<String>> {
    // Skip leading whitespace/blank lines
    take_while(0.., |c: char| c == ' ' || c == '\t' || c == '\r' || c == '\n').parse_next(input)?;

    let mut lines = Vec::new();
    loop {
        if input.starts_with('#') {
            let _: char = any.parse_next(input)?;
            opt(' ').parse_next(input)?;
            let text: &str = take_while(0.., |c: char| c != '\n').parse_next(input)?;
            lines.push(text.to_owned());
            // Consume the newline after this comment line
            opt('\n').parse_next(input)?;
            // If next line is blank (not a comment), this doc comment block is done
            let rest = input.trim_start_matches([' ', '\t', '\r']);
            if !rest.starts_with('#') {
                // Check if there's a blank line before the next content
                // If so, discard accumulated comments (they were a commented-out block)
                if rest.starts_with('\n') && !lines.is_empty() {
                    // Blank line after comments — reset and skip to next group
                    lines.clear();
                    take_while(0.., |c: char| c == ' ' || c == '\t' || c == '\r' || c == '\n').parse_next(input)?;
                    continue;
                }
                break;
            }
        } else {
            break;
        }
    }
    if lines.is_empty() {
        Ok(None)
    } else {
        Ok(Some(lines.join("\n")))
    }
}

/// Consume trailing whitespace after a parser.
fn lex<'i, O>(
    mut parser: impl Parser<&'i str, O, ErrMode<ContextError>>,
) -> impl FnMut(&mut &'i str) -> ModalResult<O> {
    move |input| {
        let o = parser.parse_next(input)?;
        ws(input)?;
        Ok(o)
    }
}

// ── Identifiers ──

fn ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn ident_cont(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

fn ident<'i>(input: &mut &'i str) -> ModalResult<&'i str> {
    take_while(1.., ident_cont)
        .verify(|s: &str| s.starts_with(ident_start))
        .parse_next(input)
}

fn ident_string(input: &mut &str) -> ModalResult<String> {
    lex(ident).map(String::from).parse_next(input)
}

/// Match a keyword, ensuring it's not a prefix of a longer identifier.
fn keyword<'i>(kw: &'static str) -> impl FnMut(&mut &'i str) -> ModalResult<()> {
    move |input: &mut &'i str| {
        let checkpoint = input.checkpoint();
        let word = lex(ident).parse_next(input)?;
        if word == kw {
            Ok(())
        } else {
            input.reset(&checkpoint);
            Err(ErrMode::Backtrack(ContextError::new()))
        }
    }
}

// ── Types ──

fn typ(input: &mut &str) -> ModalResult<Type> {
    lex(ident)
        .verify_map(|s| match s {
            "string" => Some(Type::String),
            "int" => Some(Type::Int),
            "bool" => Some(Type::Bool),
            "list" => Some(Type::List(Box::new(Type::String))),
            "map" => Some(Type::Map),
            "path" => Some(Type::Path),
            "secret" => Some(Type::Secret),
            _ => None,
        })
        .context(StrContext::Label("type"))
        .parse_next(input)
}

// ── Expressions ──

fn expr(input: &mut &str) -> ModalResult<Expr> {
    if_expr.parse_next(input)
}

fn if_expr(input: &mut &str) -> ModalResult<Expr> {
    let checkpoint = input.checkpoint();
    if keyword("if").parse_next(input).is_ok() {
        let cond = cut_err(expr)
            .context(StrContext::Label("if condition"))
            .parse_next(input)?;
        cut_err(keyword("then"))
            .context(StrContext::Label("'then'"))
            .parse_next(input)?;
        let then_val = cut_err(expr)
            .context(StrContext::Label("then value"))
            .parse_next(input)?;
        cut_err(keyword("else"))
            .context(StrContext::Label("'else'"))
            .parse_next(input)?;
        let else_val = cut_err(expr)
            .context(StrContext::Label("else value"))
            .parse_next(input)?;
        Ok(Expr::If(Box::new(cond), Box::new(then_val), Box::new(else_val)))
    } else {
        input.reset(&checkpoint);
        add_expr.parse_next(input)
    }
}

fn add_expr(input: &mut &str) -> ModalResult<Expr> {
    let first = pipe_expr.parse_next(input)?;
    repeat(0.., preceded(lex('+'), pipe_expr))
        .fold(
            move || first.clone(),
            |acc, rhs| Expr::Add(Box::new(acc), Box::new(rhs)),
        )
        .parse_next(input)
}

fn pipe_expr(input: &mut &str) -> ModalResult<Expr> {
    let first = cmp_expr.parse_next(input)?;
    let pipes: Vec<(String, Vec<Expr>)> = repeat(0.., preceded(lex('|'), pipe_segment)).parse_next(input)?;
    Ok(pipes
        .into_iter()
        .fold(first, |acc, (name, args)| Expr::Pipe(Box::new(acc), name, args)))
}

fn pipe_segment(input: &mut &str) -> ModalResult<(String, Vec<Expr>)> {
    let name = ident_string.parse_next(input)?;
    let args = opt(delimited(lex('('), arg_list, lex(')')))
        .map(|a| a.unwrap_or_default())
        .parse_next(input)?;
    Ok((name, args))
}

fn cmp_expr(input: &mut &str) -> ModalResult<Expr> {
    let lhs = primary.parse_next(input)?;
    let op = opt(alt((lex("==").value(BinOp::Eq), lex("!=").value(BinOp::Ne)))).parse_next(input)?;
    match op {
        Some(op) => {
            let rhs = cut_err(primary)
                .context(StrContext::Label("comparison rhs"))
                .parse_next(input)?;
            Ok(Expr::BinOp(Box::new(lhs), op, Box::new(rhs)))
        }
        None => Ok(lhs),
    }
}

fn primary(input: &mut &str) -> ModalResult<Expr> {
    alt((
        string_expr,
        heredoc_expr,
        int_expr,
        bool_expr,
        null_expr,
        list_expr,
        map_expr,
        call_or_ref,
    ))
    .context(StrContext::Label("expression"))
    .parse_next(input)
}

fn int_expr(input: &mut &str) -> ModalResult<Expr> {
    lex(take_while(1.., |c: char| c.is_ascii_digit()))
        .parse_to::<i64>()
        .map(Expr::Int)
        .parse_next(input)
}

fn bool_expr(input: &mut &str) -> ModalResult<Expr> {
    let checkpoint = input.checkpoint();
    let word = lex(ident).parse_next(input)?;
    match word {
        "true" => Ok(Expr::Bool(true)),
        "false" => Ok(Expr::Bool(false)),
        _ => {
            input.reset(&checkpoint);
            Err(ErrMode::Backtrack(ContextError::new()))
        }
    }
}

fn null_expr(input: &mut &str) -> ModalResult<Expr> {
    let checkpoint = input.checkpoint();
    let word = lex(ident).parse_next(input)?;
    match word {
        "null" => Ok(Expr::Null),
        _ => {
            input.reset(&checkpoint);
            Err(ErrMode::Backtrack(ContextError::new()))
        }
    }
}

fn list_expr(input: &mut &str) -> ModalResult<Expr> {
    delimited(lex('['), separated(0.., expr, lex(',')), lex(']'))
        .map(Expr::List)
        .parse_next(input)
}

fn map_expr(input: &mut &str) -> ModalResult<Expr> {
    delimited(lex('{'), separated(0.., field, lex(',')), lex('}'))
        .map(Expr::Map)
        .parse_next(input)
}

fn call_or_ref(input: &mut &str) -> ModalResult<Expr> {
    let checkpoint = input.checkpoint();
    let name = ident_string.parse_next(input)?;
    // Reject keywords so they don't get parsed as references
    if matches!(name.as_str(), "if" | "then" | "else") {
        input.reset(&checkpoint);
        return Err(ErrMode::Backtrack(ContextError::new()));
    }

    // Function call: name(args)
    if opt(lex('(')).parse_next(input)?.is_some() {
        let args = arg_list.parse_next(input)?;
        cut_err(lex(')'))
            .context(StrContext::Label("closing ')'"))
            .parse_next(input)?;
        return Ok(Expr::Call(name, args));
    }

    // Dotted reference: name.field.subfield
    let mut parts = vec![name];
    while opt(lex('.')).parse_next(input)?.is_some() {
        parts.push(ident_string.parse_next(input)?);
    }
    Ok(Expr::Ref(parts))
}

fn arg_list(input: &mut &str) -> ModalResult<Vec<Expr>> {
    separated(0.., expr, lex(',')).parse_next(input)
}

// ── String Parsing ──

fn string_expr(input: &mut &str) -> ModalResult<Expr> {
    '"'.parse_next(input)?;
    let parts: Vec<StringPart> = repeat(0.., string_part).parse_next(input)?;
    cut_err('"')
        .context(StrContext::Label("closing '\"'"))
        .parse_next(input)?;
    ws(input)?;
    Ok(Expr::Str(parts))
}

fn string_part(input: &mut &str) -> ModalResult<StringPart> {
    alt((string_interpolation, string_literal)).parse_next(input)
}

fn string_interpolation(input: &mut &str) -> ModalResult<StringPart> {
    "${".parse_next(input)?;
    let e = cut_err(expr)
        .context(StrContext::Label("interpolation expression"))
        .parse_next(input)?;
    cut_err('}')
        .context(StrContext::Label("closing '}'"))
        .parse_next(input)?;
    Ok(StringPart::Interpolation(e))
}

fn string_literal(input: &mut &str) -> ModalResult<StringPart> {
    let mut result = String::new();
    loop {
        let chunk: &str = take_while(0.., |c: char| c != '"' && c != '\\' && c != '$').parse_next(input)?;
        result.push_str(chunk);

        if input.is_empty() || input.starts_with('"') || input.starts_with("${") {
            break;
        }
        if input.starts_with('\\') {
            let _: char = any.parse_next(input)?;
            let escaped: char = cut_err(any)
                .context(StrContext::Label("escape character"))
                .parse_next(input)?;
            match escaped {
                'n' => result.push('\n'),
                'r' => result.push('\r'),
                't' => result.push('\t'),
                '"' => result.push('"'),
                '\\' => result.push('\\'),
                '$' => result.push('$'),
                other => {
                    result.push('\\');
                    result.push(other);
                }
            }
            continue;
        }
        if input.starts_with('$') {
            let _: char = any.parse_next(input)?;
            result.push('$');
            continue;
        }
        break;
    }
    if result.is_empty() {
        return Err(ErrMode::Backtrack(ContextError::new()));
    }
    Ok(StringPart::Literal(result))
}

// ── Heredoc Parsing ──

fn heredoc_expr(input: &mut &str) -> ModalResult<Expr> {
    "<<".parse_next(input)?;
    let strip = opt('-').parse_next(input)?.is_some();
    let label: &str = cut_err(take_while(1.., |c: char| c.is_ascii_alphanumeric() || c == '_'))
        .context(StrContext::Label("heredoc label"))
        .parse_next(input)?;
    let label = label.to_owned();
    cut_err('\n')
        .context(StrContext::Label("newline after heredoc label"))
        .parse_next(input)?;

    let mut parts: Vec<StringPart> = Vec::new();
    loop {
        // Check if this line is the terminator
        let line_start = *input;
        let leading: &str = take_while(0.., |c: char| c == ' ' || c == '\t').parse_next(input)?;
        if input.starts_with(label.as_str()) {
            let rest_after_label = &input[label.len()..];
            // Label must be followed by newline, EOF, or only whitespace
            if rest_after_label.is_empty()
                || rest_after_label.starts_with('\n')
                || rest_after_label.starts_with('\r')
            {
                *input = &input[label.len()..];
                // Consume trailing newline if present
                opt('\n').parse_next(input)?;
                ws(input)?;
                break;
            }
        }
        // Not the terminator — restore and parse this line as content
        *input = line_start;
        heredoc_line(&mut parts, input)?;
        if !leading.is_empty() && !strip {
            // Leading whitespace was consumed by our check; it's already
            // re-parsed by heredoc_line since we reset input above
        }
    }

    // Strip common leading indentation if <<- was used
    if strip {
        strip_indent(&mut parts);
        parts.retain(|p| !matches!(p, StringPart::Literal(s) if s.is_empty()));
    }

    // Remove trailing newline if present
    if let Some(StringPart::Literal(s)) = parts.last() {
        if s == "\n" {
            parts.pop();
        } else if s.ends_with('\n') {
            let trimmed = s[..s.len() - 1].to_owned();
            *parts.last_mut().expect("non-empty") = StringPart::Literal(trimmed);
        }
    }

    Ok(Expr::Str(parts))
}

/// Parse one line of heredoc content (up to and including the newline),
/// handling `${}` interpolation.
fn heredoc_line(parts: &mut Vec<StringPart>, input: &mut &str) -> ModalResult<()> {
    loop {
        let chunk: &str =
            take_while(0.., |c: char| c != '\n' && c != '$').parse_next(input)?;
        if !chunk.is_empty() {
            push_literal(parts, chunk);
        }

        if input.is_empty() {
            break;
        }
        if input.starts_with('\n') {
            let _: char = any.parse_next(input)?;
            push_literal(parts, "\n");
            break;
        }
        if input.starts_with("${") {
            "${".parse_next(input)?;
            let e = cut_err(expr)
                .context(StrContext::Label("heredoc interpolation"))
                .parse_next(input)?;
            cut_err('}')
                .context(StrContext::Label("closing '}'"))
                .parse_next(input)?;
            parts.push(StringPart::Interpolation(e));
            continue;
        }
        if input.starts_with('$') {
            let _: char = any.parse_next(input)?;
            push_literal(parts, "$");
            continue;
        }
        break;
    }
    Ok(())
}

/// Append to the last literal part if possible, otherwise push a new one.
fn push_literal(parts: &mut Vec<StringPart>, s: &str) {
    if let Some(StringPart::Literal(last)) = parts.last_mut() {
        last.push_str(s);
    } else {
        parts.push(StringPart::Literal(s.to_owned()));
    }
}

/// Strip the common leading whitespace from all lines in the heredoc.
///
/// A "line start" is either the very first part or the content immediately
/// after a `\n` in a literal. We only measure/strip indentation at those
/// positions — not in the middle of a line that happens to be split across
/// literal and interpolation parts.
fn strip_indent(parts: &mut [StringPart]) {
    // Pass 1: find minimum indentation at line starts
    let mut min_indent = usize::MAX;
    let mut at_line_start = true;
    for part in parts.iter() {
        if let StringPart::Literal(s) = part {
            for (i, segment) in s.split('\n').enumerate() {
                if i > 0 {
                    at_line_start = true;
                }
                if at_line_start && !segment.is_empty() {
                    let indent = segment.len() - segment.trim_start().len();
                    min_indent = min_indent.min(indent);
                    at_line_start = false;
                }
            }
        } else {
            at_line_start = false;
        }
    }
    if min_indent == 0 || min_indent == usize::MAX {
        return;
    }

    // Pass 2: strip min_indent chars from the start of each line
    at_line_start = true;
    for part in parts.iter_mut() {
        if let StringPart::Literal(s) = part {
            let mut result = String::new();
            for (i, segment) in s.split('\n').enumerate() {
                if i > 0 {
                    result.push('\n');
                    at_line_start = true;
                }
                if at_line_start && segment.len() >= min_indent {
                    result.push_str(&segment[min_indent..]);
                } else {
                    result.push_str(segment);
                }
                if !segment.is_empty() {
                    at_line_start = false;
                }
            }
            *s = result;
        } else {
            at_line_start = false;
        }
    }
}

// ── Fields ──

fn field(input: &mut &str) -> ModalResult<Field> {
    let name = ident_string.parse_next(input)?;
    cut_err(lex('='))
        .context(StrContext::Label("'=' in field"))
        .parse_next(input)?;
    let value = cut_err(expr)
        .context(StrContext::Label("field value"))
        .parse_next(input)?;
    Ok(Field { name, value })
}

// ── Statements ──

fn module(input: &mut &str) -> ModalResult<Module> {
    let statements = repeat(0.., doc_statement).parse_next(input)?;
    Ok(Module { statements })
}

fn doc_statement(input: &mut &str) -> ModalResult<Statement> {
    let doc = doc_comments(input)?;
    alt((
        let_stmt.map(Statement::Let),
        param_stmt.map(Statement::Param),
        |input: &mut &str| target_stmt(doc.clone(), input).map(Statement::Target),
        output_stmt.map(Statement::Output),
        |input: &mut &str| block_stmt(doc.clone(), input).map(Statement::Block),
    ))
    .context(StrContext::Label("statement"))
    .parse_next(input)
}

fn let_stmt(input: &mut &str) -> ModalResult<Let> {
    keyword("let").parse_next(input)?;
    let name = cut_err(ident_string)
        .context(StrContext::Label("let binding name"))
        .parse_next(input)?;
    cut_err(lex('='))
        .context(StrContext::Label("'=' in let"))
        .parse_next(input)?;
    let value = cut_err(expr)
        .context(StrContext::Label("let value"))
        .parse_next(input)?;
    Ok(Let { name, value })
}

fn param_stmt(input: &mut &str) -> ModalResult<Param> {
    keyword("param").parse_next(input)?;
    let name = cut_err(ident_string)
        .context(StrContext::Label("param name"))
        .parse_next(input)?;
    cut_err(lex(':'))
        .context(StrContext::Label("':' after param name"))
        .parse_next(input)?;
    let t = cut_err(typ)
        .context(StrContext::Label("param type"))
        .parse_next(input)?;
    let default = opt(preceded(lex('='), expr)).parse_next(input)?;
    Ok(Param { name, typ: t, default })
}

fn target_stmt(doc: Option<String>, input: &mut &str) -> ModalResult<Target> {
    keyword("target").parse_next(input)?;
    let name = cut_err(ident_string)
        .context(StrContext::Label("target name"))
        .parse_next(input)?;
    cut_err(lex('='))
        .context(StrContext::Label("'=' in target"))
        .parse_next(input)?;
    let blocks = cut_err(delimited(lex('['), separated(0.., dotted_ident, lex(',')), lex(']')))
        .context(StrContext::Label("target block list"))
        .parse_next(input)?;
    Ok(Target { name, doc, blocks })
}

fn dotted_ident(input: &mut &str) -> ModalResult<String> {
    let first = ident_string.parse_next(input)?;
    let rest: Vec<String> = repeat(0.., preceded(lex('.'), ident_string)).parse_next(input)?;
    if rest.is_empty() {
        Ok(first)
    } else {
        let mut result = first;
        for part in rest {
            result.push('.');
            result.push_str(&part);
        }
        Ok(result)
    }
}

fn output_stmt(input: &mut &str) -> ModalResult<Output> {
    keyword("output").parse_next(input)?;
    let name = cut_err(ident_string)
        .context(StrContext::Label("output name"))
        .parse_next(input)?;
    cut_err(lex('='))
        .context(StrContext::Label("'=' in output"))
        .parse_next(input)?;
    let value = cut_err(expr)
        .context(StrContext::Label("output value"))
        .parse_next(input)?;
    Ok(Output { name, value })
}

/// A field inside a block body, with optional trailing comma.
fn block_field(input: &mut &str) -> ModalResult<Field> {
    let f = field(input)?;
    opt(lex(',')).parse_next(input)?;
    Ok(f)
}

fn block_stmt(doc: Option<String>, input: &mut &str) -> ModalResult<Block> {
    let protected = opt(keyword("protected")).map(|o| o.is_some()).parse_next(input)?;
    let name = ident_string.parse_next(input)?;
    // Once we see `name =`, this must be a block statement — commit to it
    cut_err(lex('='))
        .context(StrContext::Expected(winnow::error::StrContextValue::Description("'='")))
        .parse_next(input)?;
    let provider = cut_err(ident_string)
        .context(StrContext::Label("provider name"))
        .parse_next(input)?;

    // provider.resource or bare provider (like "exec")
    let resource = if opt(lex('.')).parse_next(input)?.is_some() {
        cut_err(ident_string)
            .context(StrContext::Label("resource name"))
            .parse_next(input)?
    } else {
        String::new()
    };

    cut_err(lex('{')).context(StrContext::Label("'{'")).parse_next(input)?;
    let fields: Vec<Field> = repeat(0.., block_field).parse_next(input)?;
    cut_err(lex('}')).context(StrContext::Label("'}'")).parse_next(input)?;

    let (provider, resource) = if resource.is_empty() {
        (provider.clone(), provider)
    } else {
        (provider, resource)
    };

    Ok(Block {
        name,
        doc,
        protected,
        provider,
        resource,
        fields,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_string_literal() {
        let result = parse(r#"let x = "hello""#, "<test>").unwrap();
        assert_eq!(result.statements.len(), 1);
        match &result.statements[0] {
            Statement::Let(l) => {
                assert_eq!(l.name, "x");
                assert_eq!(l.value, Expr::Str(vec![StringPart::Literal("hello".into())]));
            }
            _ => panic!("expected Let"),
        }
    }

    #[test]
    fn parse_string_interpolation() {
        let result = parse(r#"let x = "hello ${name}""#, "<test>").unwrap();
        match &result.statements[0] {
            Statement::Let(l) => match &l.value {
                Expr::Str(parts) => {
                    assert_eq!(parts.len(), 2);
                    assert_eq!(parts[0], StringPart::Literal("hello ".into()));
                    assert_eq!(parts[1], StringPart::Interpolation(Expr::Ref(vec!["name".into()])));
                }
                _ => panic!("expected Str"),
            },
            _ => panic!("expected Let"),
        }
    }

    #[test]
    fn parse_interpolation_with_pipe() {
        let result = parse(r#"let x = "${exec("cmd") | trim}""#, "<test>").unwrap();
        match &result.statements[0] {
            Statement::Let(l) => match &l.value {
                Expr::Str(parts) => {
                    assert_eq!(parts.len(), 1);
                    match &parts[0] {
                        StringPart::Interpolation(Expr::Pipe(inner, name, _)) => {
                            assert_eq!(name, "trim");
                            assert!(matches!(inner.as_ref(), Expr::Call(..)));
                        }
                        _ => panic!("expected interpolated pipe"),
                    }
                }
                _ => panic!("expected Str"),
            },
            _ => panic!("expected Let"),
        }
    }

    #[test]
    fn parse_int() {
        let result = parse("let x = 42", "<test>").unwrap();
        match &result.statements[0] {
            Statement::Let(l) => assert_eq!(l.value, Expr::Int(42)),
            _ => panic!("expected Let"),
        }
    }

    #[test]
    fn parse_bool() {
        let result = parse("let x = true", "<test>").unwrap();
        match &result.statements[0] {
            Statement::Let(l) => assert_eq!(l.value, Expr::Bool(true)),
            _ => panic!("expected Let"),
        }
    }

    #[test]
    fn parse_list() {
        let result = parse(r#"let x = ["a", "b"]"#, "<test>").unwrap();
        match &result.statements[0] {
            Statement::Let(l) => match &l.value {
                Expr::List(items) => assert_eq!(items.len(), 2),
                _ => panic!("expected List"),
            },
            _ => panic!("expected Let"),
        }
    }

    #[test]
    fn parse_map() {
        let result = parse(r#"let x = { a = 1, b = 2 }"#, "<test>").unwrap();
        match &result.statements[0] {
            Statement::Let(l) => match &l.value {
                Expr::Map(fields) => {
                    assert_eq!(fields.len(), 2);
                    assert_eq!(fields[0].name, "a");
                    assert_eq!(fields[1].name, "b");
                }
                _ => panic!("expected Map"),
            },
            _ => panic!("expected Let"),
        }
    }

    #[test]
    fn parse_function_call() {
        let result = parse(r#"let x = exec("git rev-parse HEAD")"#, "<test>").unwrap();
        match &result.statements[0] {
            Statement::Let(l) => match &l.value {
                Expr::Call(name, args) => {
                    assert_eq!(name, "exec");
                    assert_eq!(args.len(), 1);
                }
                _ => panic!("expected Call"),
            },
            _ => panic!("expected Let"),
        }
    }

    #[test]
    fn parse_pipe_chain() {
        let result = parse(r#"let x = exec("cmd") | trim | lines | uniq"#, "<test>").unwrap();
        match &result.statements[0] {
            Statement::Let(l) => match &l.value {
                Expr::Pipe(inner, name, _) => {
                    assert_eq!(name, "uniq");
                    match inner.as_ref() {
                        Expr::Pipe(_, name, _) => assert_eq!(name, "lines"),
                        _ => panic!("expected inner Pipe"),
                    }
                }
                _ => panic!("expected Pipe"),
            },
            _ => panic!("expected Let"),
        }
    }

    #[test]
    fn parse_pipe_with_args() {
        let result = parse(r#"let x = "a:b:c" | split(":")"#, "<test>").unwrap();
        match &result.statements[0] {
            Statement::Let(l) => match &l.value {
                Expr::Pipe(_, name, args) => {
                    assert_eq!(name, "split");
                    assert_eq!(args.len(), 1);
                }
                _ => panic!("expected Pipe"),
            },
            _ => panic!("expected Let"),
        }
    }

    #[test]
    fn parse_if_expr() {
        let result = parse(r#"let x = if env == "prod" then 3 else 1"#, "<test>").unwrap();
        match &result.statements[0] {
            Statement::Let(l) => match &l.value {
                Expr::If(cond, then_val, else_val) => {
                    assert!(matches!(cond.as_ref(), Expr::BinOp(_, BinOp::Eq, _)));
                    assert_eq!(**then_val, Expr::Int(3));
                    assert_eq!(**else_val, Expr::Int(1));
                }
                _ => panic!("expected If"),
            },
            _ => panic!("expected Let"),
        }
    }

    #[test]
    fn parse_list_concat() {
        let result = parse(r#"let x = ["a"] + ["b"]"#, "<test>").unwrap();
        match &result.statements[0] {
            Statement::Let(l) => {
                assert!(matches!(l.value, Expr::Add(..)));
            }
            _ => panic!("expected Let"),
        }
    }

    #[test]
    fn parse_dotted_ref() {
        let result = parse("let x = server.path", "<test>").unwrap();
        match &result.statements[0] {
            Statement::Let(l) => match &l.value {
                Expr::Ref(parts) => {
                    assert_eq!(parts, &["server", "path"]);
                }
                _ => panic!("expected Ref"),
            },
            _ => panic!("expected Let"),
        }
    }

    #[test]
    fn parse_param() {
        let result = parse("param environment : string", "<test>").unwrap();
        match &result.statements[0] {
            Statement::Param(p) => {
                assert_eq!(p.name, "environment");
                assert_eq!(p.typ, Type::String);
                assert_eq!(p.default, None);
            }
            _ => panic!("expected Param"),
        }
    }

    #[test]
    fn parse_param_with_default() {
        let result = parse("param replicas : int = 1", "<test>").unwrap();
        match &result.statements[0] {
            Statement::Param(p) => {
                assert_eq!(p.name, "replicas");
                assert_eq!(p.typ, Type::Int);
                assert_eq!(p.default, Some(Expr::Int(1)));
            }
            _ => panic!("expected Param"),
        }
    }

    #[test]
    fn parse_target() {
        let result = parse("target build = [server, image]", "<test>").unwrap();
        match &result.statements[0] {
            Statement::Target(t) => {
                assert_eq!(t.name, "build");
                assert_eq!(t.blocks, vec!["server", "image"]);
            }
            _ => panic!("expected Target"),
        }
    }

    #[test]
    fn parse_target_dotted() {
        let result = parse("target deploy = [staging.deploy]", "<test>").unwrap();
        match &result.statements[0] {
            Statement::Target(t) => {
                assert_eq!(t.blocks, vec!["staging.deploy"]);
            }
            _ => panic!("expected Target"),
        }
    }

    #[test]
    fn parse_output() {
        let result = parse("output endpoint = app.endpoint", "<test>").unwrap();
        match &result.statements[0] {
            Statement::Output(o) => {
                assert_eq!(o.name, "endpoint");
                assert_eq!(o.value, Expr::Ref(vec!["app".into(), "endpoint".into()]));
            }
            _ => panic!("expected Output"),
        }
    }

    #[test]
    fn parse_simple_block() {
        let result = parse(r#"server = go.binary { main = "./cmd/server" }"#, "<test>").unwrap();
        match &result.statements[0] {
            Statement::Block(b) => {
                assert_eq!(b.name, "server");
                assert_eq!(b.provider, "go");
                assert_eq!(b.resource, "binary");
                assert!(!b.protected);
                assert_eq!(b.fields.len(), 1);
                assert_eq!(b.fields[0].name, "main");
            }
            _ => panic!("expected Block"),
        }
    }

    #[test]
    fn parse_protected_block() {
        let result = parse(r#"protected db = aws.aurora { cluster = "prod" }"#, "<test>").unwrap();
        match &result.statements[0] {
            Statement::Block(b) => {
                assert!(b.protected);
                assert_eq!(b.provider, "aws");
                assert_eq!(b.resource, "aurora");
            }
            _ => panic!("expected Block"),
        }
    }

    #[test]
    fn parse_bare_provider_block() {
        let input =
            "docs = exec {\n  command = \"mdbook build\"\n  inputs  = [\"docs/**/*.md\"]\n  output  = \"book/\"\n}";
        let result = parse(input, "<test>").unwrap();
        match &result.statements[0] {
            Statement::Block(b) => {
                assert_eq!(b.provider, "exec");
                assert_eq!(b.resource, "exec");
                assert_eq!(b.fields.len(), 3);
            }
            _ => panic!("expected Block"),
        }
    }

    #[test]
    fn parse_comments() {
        let input = "# This is a comment\nlet x = 42  # inline comment\n# Another comment\nlet y = 10\n";
        let result = parse(input, "<test>").unwrap();
        assert_eq!(result.statements.len(), 2);
    }

    #[test]
    fn parse_escape_sequences() {
        let result = parse(r#"let x = "hello\nworld""#, "<test>").unwrap();
        match &result.statements[0] {
            Statement::Let(l) => {
                assert_eq!(l.value, Expr::Str(vec![StringPart::Literal("hello\nworld".into())]));
            }
            _ => panic!("expected Let"),
        }
    }

    #[test]
    fn parse_exec_block_with_dynamic_inputs() {
        let input = concat!(
            "server = exec {\n",
            "  command = \"go build -o ${output}/server ./cmd/server\"\n",
            "  inputs  = [\"go.mod\", \"go.sum\"]\n",
            "            + exec(\"go list -deps -f '{{.Dir}}/*.go' ./cmd/server/...\") | lines\n",
            "  output  = \"server\"\n",
            "}",
        );
        let result = parse(input, "<test>").unwrap();
        match &result.statements[0] {
            Statement::Block(b) => {
                assert_eq!(b.name, "server");
                assert_eq!(b.fields.len(), 3);
                match &b.fields[1].value {
                    Expr::Add(lhs, rhs) => {
                        assert!(matches!(lhs.as_ref(), Expr::List(_)));
                        assert!(matches!(rhs.as_ref(), Expr::Pipe(..)));
                    }
                    _ => panic!("expected Add, got {:?}", b.fields[1].value),
                }
            }
            _ => panic!("expected Block"),
        }
    }

    #[test]
    fn parse_multistatement_module() {
        let input = concat!(
            "param environment : string\n",
            "param replicas    : int = 1\n",
            "\n",
            "let git_sha = exec(\"git rev-parse --short HEAD\") | trim\n",
            "\n",
            "server = go.binary { main = \"./cmd/server\" }\n",
            "\n",
            "image = docker.image {\n",
            "  tag = \"${registry}/myapp:${git_sha}\"\n",
            "}\n",
            "\n",
            "output image_ref = image.ref\n",
            "\n",
            "target build = [server, image]\n",
        );
        let result = parse(input, "<test>").unwrap();
        assert_eq!(result.statements.len(), 7);
    }

    #[test]
    fn parse_string_preserves_leading_whitespace() {
        let result = parse(r#"let x = "  hello  ""#, "<test>").unwrap();
        match &result.statements[0] {
            Statement::Let(l) => {
                assert_eq!(l.value, Expr::Str(vec![StringPart::Literal("  hello  ".into())]));
            }
            _ => panic!("expected Let"),
        }
    }

    #[test]
    fn parse_output_as_variable_name() {
        let result = parse("let x = output", "<test>").unwrap();
        match &result.statements[0] {
            Statement::Let(l) => {
                assert_eq!(l.value, Expr::Ref(vec!["output".into()]));
            }
            _ => panic!("expected Let"),
        }
    }

    #[test]
    fn parse_block_fields_with_commas() {
        let input = r#"a = exec { command = "echo hi", output = "out" }"#;
        let result = parse(input, "<test>").unwrap();
        match &result.statements[0] {
            Statement::Block(b) => {
                assert_eq!(b.fields.len(), 2);
                assert_eq!(b.fields[0].name, "command");
                assert_eq!(b.fields[1].name, "output");
            }
            _ => panic!("expected Block"),
        }
    }

    #[test]
    fn parse_doc_comment_on_target() {
        let input = "# Build everything\ntarget build = [server]\n";
        let result = parse(input, "<test>").unwrap();
        match &result.statements[0] {
            Statement::Target(t) => {
                assert_eq!(t.name, "build");
                assert_eq!(t.doc, Some("Build everything".into()));
            }
            _ => panic!("expected Target"),
        }
    }

    #[test]
    fn parse_doc_comment_on_block() {
        let input = "# The main server binary\nserver = go.binary { main = \"./cmd/server\" }\n";
        let result = parse(input, "<test>").unwrap();
        match &result.statements[0] {
            Statement::Block(b) => {
                assert_eq!(b.name, "server");
                assert_eq!(b.doc, Some("The main server binary".into()));
            }
            _ => panic!("expected Block"),
        }
    }

    #[test]
    fn parse_multiline_doc_comment() {
        let input = "# Build and push\n# the Docker image\ntarget deploy = [image]\n";
        let result = parse(input, "<test>").unwrap();
        match &result.statements[0] {
            Statement::Target(t) => {
                assert_eq!(t.doc, Some("Build and push\nthe Docker image".into()));
            }
            _ => panic!("expected Target"),
        }
    }

    #[test]
    fn parse_no_doc_comment() {
        let input = "target build = [server]\n";
        let result = parse(input, "<test>").unwrap();
        match &result.statements[0] {
            Statement::Target(t) => {
                assert_eq!(t.doc, None);
            }
            _ => panic!("expected Target"),
        }
    }

    #[test]
    fn parse_commented_out_block_not_doc_comment() {
        let input = concat!(
            "# image = docker.image {\n",
            "#   tag = \"bit:latest\"\n",
            "# }\n",
            "\n",
            "# Build debug binary only\n",
            "target debug = [debug]\n",
        );
        let result = parse(input, "<test>").unwrap();
        match &result.statements[0] {
            Statement::Target(t) => {
                assert_eq!(t.doc, Some("Build debug binary only".into()));
            }
            _ => panic!("expected Target"),
        }
    }

    #[test]
    fn parse_heredoc() {
        let input = "let x = <<EOF\nhello\nworld\nEOF\n";
        let result = parse(input, "<test>").unwrap();
        match &result.statements[0] {
            Statement::Let(l) => match &l.value {
                Expr::Str(parts) => {
                    assert_eq!(parts.len(), 1);
                    assert_eq!(parts[0], StringPart::Literal("hello\nworld".into()));
                }
                _ => panic!("expected Str"),
            },
            _ => panic!("expected Let"),
        }
    }

    #[test]
    fn parse_heredoc_strip_indent() {
        let input = "let x = <<-EOF\n    hello\n    world\n  EOF\n";
        let result = parse(input, "<test>").unwrap();
        match &result.statements[0] {
            Statement::Let(l) => match &l.value {
                Expr::Str(parts) => {
                    assert_eq!(parts.len(), 1);
                    assert_eq!(parts[0], StringPart::Literal("hello\nworld".into()));
                }
                _ => panic!("expected Str"),
            },
            _ => panic!("expected Let"),
        }
    }

    #[test]
    fn parse_heredoc_interpolation_preserves_spaces() {
        let input = "let x = <<EOF\n${name} --version\nEOF\n";
        let result = parse(input, "<test>").unwrap();
        match &result.statements[0] {
            Statement::Let(l) => match &l.value {
                Expr::Str(parts) => {
                    assert_eq!(parts.len(), 2);
                    assert!(matches!(&parts[0], StringPart::Interpolation(_)));
                    assert_eq!(parts[1], StringPart::Literal(" --version".into()));
                }
                _ => panic!("expected Str"),
            },
            _ => panic!("expected Let"),
        }
    }

    #[test]
    fn parse_heredoc_strip_indent_with_interpolation() {
        let input = "let x = <<-EOF\n  ${name} --version\n  ${name} graph\n  EOF\n";
        let result = parse(input, "<test>").unwrap();
        match &result.statements[0] {
            Statement::Let(l) => match &l.value {
                Expr::Str(parts) => {
                    assert_eq!(parts.len(), 4);
                    assert!(matches!(&parts[0], StringPart::Interpolation(_)));
                    assert_eq!(parts[1], StringPart::Literal(" --version\n".into()));
                    assert!(matches!(&parts[2], StringPart::Interpolation(_)));
                    assert_eq!(parts[3], StringPart::Literal(" graph".into()));
                }
                _ => panic!("expected Str"),
            },
            _ => panic!("expected Let"),
        }
    }

    #[test]
    fn parse_heredoc_with_interpolation() {
        let input = "let x = <<EOF\nhello ${name}\nEOF\n";
        let result = parse(input, "<test>").unwrap();
        match &result.statements[0] {
            Statement::Let(l) => match &l.value {
                Expr::Str(parts) => {
                    assert_eq!(parts.len(), 2);
                    assert_eq!(parts[0], StringPart::Literal("hello ".into()));
                    assert!(matches!(&parts[1], StringPart::Interpolation(Expr::Ref(r)) if r == &["name"]));
                }
                _ => panic!("expected Str"),
            },
            _ => panic!("expected Let"),
        }
    }
}
