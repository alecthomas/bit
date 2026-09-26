use winnow::combinator::{alt, cut_err, delimited, opt, preceded, repeat, separated};
use winnow::error::{AddContext, ContextError, ErrMode, StrContext};
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

/// Source details retained for formatting, separate from the semantic AST.
pub(crate) struct ParsedModule<'a> {
    pub(crate) module: Module,
    pub(crate) comments: Vec<Comment<'a>>,
    pub(crate) statements: Vec<StatementSource<'a>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Comment<'a> {
    /// Byte offset from the start of the original source.
    pub(crate) offset: usize,
    pub(crate) text: &'a str,
}

pub(crate) struct StatementSource<'a> {
    pub(crate) start: usize,
    pub(crate) end: usize,
    pub(crate) expression: &'a str,
    pub(crate) param_type_explicit: bool,
    pub(crate) fields: Vec<FieldSource<'a>>,
}

pub(crate) struct FieldSource<'a> {
    pub(crate) start: usize,
    pub(crate) expression: &'a str,
}

struct ParsedBlock<'a> {
    block: Block,
    comments: Vec<Comment<'a>>,
    fields: Vec<FieldSource<'a>>,
}

pub fn parse(input: &str, filename: &str) -> Result<Module, ParseError> {
    Ok(parse_with_comments(input, filename)?.module)
}

pub(crate) fn parse_with_comments<'a>(input: &'a str, filename: &str) -> Result<ParsedModule<'a>, ParseError> {
    // The inner parser captures byte offsets in Pos.line (temporarily).
    // We fix them up to real line:col after parsing.
    let mut parsed = module.parse(input).map_err(|e| {
        let message = format_parse_error(input, filename, e.offset(), e.inner());
        ParseError { message }
    })?;
    // Convert byte offsets to line:col and attach filename
    for stmt in &mut parsed.module.statements {
        let pos = match stmt {
            Statement::Import(i) => &mut i.pos,
            Statement::Block(b) => &mut b.pos,
            Statement::Let(l) => &mut l.pos,
            Statement::Param(p) => &mut p.pos,
            Statement::Target(t) => &mut t.pos,
            Statement::Output(o) => &mut o.pos,
        };
        let offset = pos.line; // byte offset stored temporarily in line
        let prefix = &input[..offset.min(input.len())];
        pos.file = filename.to_owned();
        pos.line = prefix.chars().filter(|&c| c == '\n').count() + 1;
        pos.col = prefix.len() - prefix.rfind('\n').map(|i| i + 1).unwrap_or(0) + 1;
    }
    Ok(parsed)
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

/// Skip whitespace only (no comments). Used by `lex`.
fn ws(input: &mut &str) -> ModalResult<()> {
    take_while(0.., |c: char| c == ' ' || c == '\t' || c == '\r' || c == '\n')
        .void()
        .parse_next(input)
}

/// True if a `#` at the start of `input` introduces a comment.
///
/// `# ` (followed by whitespace, newline, or EOF) is a comment; `#{` opens an
/// interpolation; anything else after `#` is reserved for the language. This
/// disambiguation lets bit use both `#` for comments and `#{...}` for string
/// interpolation without collision.
fn is_comment_start(input: &str) -> bool {
    if !input.starts_with('#') {
        return false;
    }
    match input.as_bytes().get(1) {
        None => true, // `#` at EOF
        Some(b) => matches!(*b, b' ' | b'\t' | b'\n' | b'\r'),
    }
}

fn comment_line<'i>(input: &mut &'i str) -> &'i str {
    let before = *input;
    let end = before.find('\n').unwrap_or(before.len());
    *input = &before[end..];
    if input.starts_with('\n') {
        *input = &input[1..];
    }
    &before[..end]
}

/// Skip whitespace and comment lines, retaining their offsets in this slice.
fn ws_and_comments<'i>(input: &mut &'i str) -> ModalResult<Vec<Comment<'i>>> {
    let start_len = input.len();
    let mut comments = Vec::new();
    loop {
        ws(input)?;
        if is_comment_start(input) {
            let offset = start_len - input.len();
            comments.push(Comment {
                offset,
                text: comment_line(input),
            });
        } else {
            break;
        }
    }
    Ok(comments)
}

/// Skip whitespace and comments, capturing the comment block directly
/// adjacent to the following statement as a doc string. A blank line
/// between a comment block and the statement detaches it as documentation.
/// All comments remain available in the parsed source details.
fn ws_capturing_doc<'i>(input: &mut &'i str) -> (Option<String>, Vec<Comment<'i>>) {
    let start_len = input.len();
    let mut doc_lines: Vec<String> = Vec::new();
    let mut comments = Vec::new();
    let mut had_blank_line = false;
    loop {
        // Skip whitespace, tracking blank lines
        let before = *input;
        let _ =
            take_while::<_, _, ErrMode<ContextError>>(0.., |c: char| c == ' ' || c == '\t' || c == '\r' || c == '\n')
                .parse_next(input);
        if before != *input {
            let skipped = &before[..before.len() - input.len()];
            // A newline in whitespace between comments means a blank line
            // (the newline at the end of the previous comment was already consumed)
            if skipped.contains('\n') {
                had_blank_line = true;
            }
        }

        if is_comment_start(input) {
            // Blank line before this comment block — earlier lines were
            // not attached to anything, discard them
            if had_blank_line {
                doc_lines.clear();
            }
            had_blank_line = false;
            let offset = start_len - input.len();
            let text = comment_line(input);
            comments.push(Comment { offset, text });
            let line = text
                .strip_prefix("# ")
                .unwrap_or_else(|| text.strip_prefix('#').unwrap_or(text));
            doc_lines.push(line.trim_end_matches('\r').to_owned());
        } else {
            // A blank line between the final comment block and the statement
            // means the comment is not attached — discard it.
            if had_blank_line {
                doc_lines.clear();
            }
            break;
        }
    }
    if doc_lines.is_empty() {
        (None, comments)
    } else {
        (Some(doc_lines.join("\n")), comments)
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
    let first = typ_atom.context(StrContext::Label("type")).parse_next(input)?;
    let rest: Vec<Type> = repeat(0.., preceded(lex('|'), typ_atom)).parse_next(input)?;
    if rest.is_empty() {
        Ok(first)
    } else {
        let mut variants = vec![first];
        variants.extend(rest);
        Ok(Type::Union(variants))
    }
}

fn typ_atom(input: &mut &str) -> ModalResult<Type> {
    alt((typ_list, typ_map, typ_scalar)).parse_next(input)
}

/// `[type]`
fn typ_list(input: &mut &str) -> ModalResult<Type> {
    let inner = delimited(lex('['), cut_err(typ), cut_err(lex(']'))).parse_next(input)?;
    Ok(Type::List(Box::new(inner)))
}

/// `{string: type}`
fn typ_map(input: &mut &str) -> ModalResult<Type> {
    lex('{').parse_next(input)?;
    cut_err(keyword("string")).parse_next(input)?;
    cut_err(lex('=')).parse_next(input)?;
    let inner = cut_err(typ).parse_next(input)?;
    cut_err(lex('}')).parse_next(input)?;
    Ok(Type::Map(Box::new(inner)))
}

fn typ_scalar(input: &mut &str) -> ModalResult<Type> {
    lex(ident)
        .verify_map(|s| match s {
            "string" => Some(Type::String),
            "number" => Some(Type::Number),
            "bool" => Some(Type::Bool),
            "duration" => Some(Type::Duration),
            "path" => Some(Type::Path),
            "secret" => Some(Type::Secret),
            _ => None,
        })
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
        raw_string_expr,
        heredoc_expr,
        number_expr,
        bool_expr,
        null_expr,
        list_expr,
        map_expr,
        call_or_ref,
    ))
    .context(StrContext::Label("expression"))
    .parse_next(input)
}

fn number_expr(input: &mut &str) -> ModalResult<Expr> {
    use bigdecimal::BigDecimal;
    use std::str::FromStr;

    // Digits + optional decimal part (no trailing-whitespace consumption
    // yet — a duration unit must sit flush against the digits).
    let digits: &str = take_while(1.., |c: char| c.is_ascii_digit()).parse_next(input)?;
    let frac = opt(('.', take_while(1.., |c: char| c.is_ascii_digit()))).parse_next(input)?;
    let num_str = match frac {
        Some((_, f)) => format!("{digits}.{f}"),
        None => digits.to_owned(),
    };

    // If a duration unit follows immediately (no whitespace), emit a
    // Duration literal. `ns`/`us`/`ms` are tried before `s`/`m` so the
    // longer suffix wins. After the unit we require that the next char
    // isn't alphanumeric/underscore — otherwise `5seconds` would be
    // mis-parsed as `5s` + `econds`.
    let checkpoint = input.checkpoint();
    if let Some(unit) = opt(alt(("ns", "us", "ms", "s", "m", "h", "d"))).parse_next(input)? {
        let next_is_word = input
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_');
        if !next_is_word {
            let duration =
                crate::value::parse_duration(&num_str, unit).map_err(|_| ErrMode::Backtrack(ContextError::new()))?;
            ws(input)?;
            return Ok(Expr::Duration(duration));
        }
        input.reset(&checkpoint);
    }

    ws(input)?;
    let n = BigDecimal::from_str(&num_str).map_err(|_| ErrMode::Backtrack(ContextError::new()))?;
    Ok(Expr::Number(n))
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
    delimited(
        lex('['),
        opt((separated(1.., expr, lex(',')), opt(lex(','))))
            .map(|items| items.map(|(values, _)| values).unwrap_or_default()),
        lex(']'),
    )
    .map(Expr::List)
    .parse_next(input)
}

fn map_expr(input: &mut &str) -> ModalResult<Expr> {
    delimited(
        lex('{'),
        opt((separated(1.., map_entry, lex(',')), opt(lex(','))))
            .map(|items| items.map(|(values, _)| values).unwrap_or_default()),
        lex('}'),
    )
    .map(Expr::Map)
    .parse_next(input)
}

/// `"key" = value` or `'key' = value` or `key = value`
fn map_entry(input: &mut &str) -> ModalResult<Field> {
    let name = alt((lex(plain_string), lex(plain_raw_string), ident_string)).parse_next(input)?;
    cut_err(lex('='))
        .context(StrContext::Label("'=' in map entry"))
        .parse_next(input)?;
    let value = cut_err(expr)
        .context(StrContext::Label("map entry value"))
        .parse_next(input)?;
    Ok(Field { name, value })
}

fn matrix_ref_key(input: &mut &str) -> ModalResult<Expr> {
    expr.parse_next(input)
}

fn call_or_ref(input: &mut &str) -> ModalResult<Expr> {
    let checkpoint = input.checkpoint();
    let name = ident_string.parse_next(input)?;
    // Reject keywords so they don't get parsed as references
    if matches!(name.as_str(), "if" | "then" | "else") {
        input.reset(&checkpoint);
        return Err(ErrMode::Backtrack(ContextError::new()));
    }

    // A named-argument call is a parameterized block reference. Positional
    // calls remain built-in functions.
    if opt(lex('(')).parse_next(input)?.is_some() {
        let args_checkpoint = input.checkpoint();
        match named_argument_list.parse_next(input) {
            Ok(args) => {
                cut_err(lex(')'))
                    .context(StrContext::Label("closing ')'"))
                    .parse_next(input)?;
                let fields: Vec<String> = repeat(0.., preceded(lex('.'), ident_string)).parse_next(input)?;
                return Ok(Expr::BlockCall { name, args, fields });
            }
            Err(ErrMode::Backtrack(_)) => input.reset(&args_checkpoint),
            Err(error) => return Err(error),
        }
        let args = arg_list.parse_next(input)?;
        cut_err(lex(')'))
            .context(StrContext::Label("closing ')'"))
            .parse_next(input)?;
        return Ok(Expr::Call(name, args));
    }

    // Provider function call or dotted reference: provider.function(args)
    if opt(lex('.')).parse_next(input)?.is_some() {
        let mut parts = vec![name];
        parts.push(ident_string.parse_next(input)?);
        if opt(lex('(')).parse_next(input)?.is_some() {
            let args = arg_list.parse_next(input)?;
            cut_err(lex(')'))
                .context(StrContext::Label("closing ')'"))
                .parse_next(input)?;
            return Ok(Expr::Call(parts.join("."), args));
        }
        while opt(lex('.')).parse_next(input)?.is_some() {
            parts.push(ident_string.parse_next(input)?);
        }
        return Ok(Expr::Ref(parts));
    }

    // Matrix slice reference: name[expr1, expr2]
    if opt('[').parse_next(input)?.is_some() {
        let keys: Vec<Expr> = separated(1.., matrix_ref_key, lex(',')).parse_next(input)?;
        cut_err(lex(']'))
            .context(StrContext::Label("closing ']' in matrix ref"))
            .parse_next(input)?;
        let fields: Vec<String> = repeat(0.., preceded(lex('.'), ident_string)).parse_next(input)?;
        return Ok(Expr::MatrixRef { name, keys, fields });
    }

    Ok(Expr::Ref(vec![name]))
}

fn arg_list(input: &mut &str) -> ModalResult<Vec<Expr>> {
    separated(0.., expr, lex(',')).parse_next(input)
}

fn named_argument(input: &mut &str) -> ModalResult<Field> {
    let checkpoint = input.checkpoint();
    let name = ident_string.parse_next(input)?;
    if opt(lex('=')).parse_next(input)?.is_none() {
        input.reset(&checkpoint);
        return Err(ErrMode::Backtrack(ContextError::new()));
    }
    let value = cut_err(expr)
        .context(StrContext::Label("named argument value"))
        .parse_next(input)?;
    Ok(Field { name, value })
}

fn named_argument_list(input: &mut &str) -> ModalResult<Vec<Field>> {
    (separated(1.., named_argument, lex(',')), opt(lex(',')))
        .map(|(args, _)| args)
        .parse_next(input)
}

// ── String Parsing ──

/// Parse a plain double-quoted string with no interpolation, returning just the content.
fn plain_string(input: &mut &str) -> ModalResult<String> {
    '"'.parse_next(input)?;
    let content: String = take_while(0.., |c: char| c != '"' && c != '\n')
        .parse_next(input)?
        .to_owned();
    cut_err('"')
        .context(StrContext::Label("closing '\"'"))
        .parse_next(input)?;
    Ok(content)
}

/// Single-quoted raw string: no escapes, no interpolation.
/// Content is taken verbatim until the closing `'`.
fn raw_string_expr(input: &mut &str) -> ModalResult<Expr> {
    '\''.parse_next(input)?;
    let content: &str = take_while(0.., |c: char| c != '\'').parse_next(input)?;
    cut_err('\'')
        .context(StrContext::Label("closing '\\''"))
        .parse_next(input)?;
    ws(input)?;
    Ok(Expr::Str(vec![StringPart::Literal(content.to_owned())]))
}

/// Parse a plain single-quoted string, returning just the content.
fn plain_raw_string(input: &mut &str) -> ModalResult<String> {
    '\''.parse_next(input)?;
    let content: String = take_while(0.., |c: char| c != '\'').parse_next(input)?.to_owned();
    cut_err('\'')
        .context(StrContext::Label("closing '\\''"))
        .parse_next(input)?;
    Ok(content)
}

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
    "#{".parse_next(input)?;
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
        let chunk: &str = take_while(0.., |c: char| c != '"' && c != '\\' && c != '#').parse_next(input)?;
        result.push_str(chunk);

        if input.is_empty() || input.starts_with('"') || input.starts_with("#{") {
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
                '#' => result.push('#'),
                other => {
                    result.push('\\');
                    result.push(other);
                }
            }
            continue;
        }
        if input.starts_with('#') {
            let _: char = any.parse_next(input)?;
            result.push('#');
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
            if rest_after_label.is_empty() || rest_after_label.starts_with('\n') || rest_after_label.starts_with('\r') {
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
/// handling `#{}` interpolation.
fn heredoc_line(parts: &mut Vec<StringPart>, input: &mut &str) -> ModalResult<()> {
    loop {
        let chunk: &str = take_while(0.., |c: char| c != '\n' && c != '#').parse_next(input)?;
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
        if input.starts_with("#{") {
            "#{".parse_next(input)?;
            let e = cut_err(expr)
                .context(StrContext::Label("heredoc interpolation"))
                .parse_next(input)?;
            cut_err('}')
                .context(StrContext::Label("closing '}'"))
                .parse_next(input)?;
            parts.push(StringPart::Interpolation(e));
            continue;
        }
        if input.starts_with('#') {
            let _: char = any.parse_next(input)?;
            push_literal(parts, "#");
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

fn field<'i>(input: &mut &'i str) -> ModalResult<(Field, &'i str)> {
    let name = ident_string.parse_next(input)?;
    cut_err(lex('='))
        .context(StrContext::Label("'=' in field"))
        .parse_next(input)?;
    let before_value = *input;
    let value = cut_err(expr)
        .context(StrContext::Label("field value"))
        .parse_next(input)?;
    let value_source = &before_value[..before_value.len() - input.len()];
    Ok((Field { name, value }, value_source))
}

// ── Statements ──

fn module<'i>(input: &mut &'i str) -> ModalResult<ParsedModule<'i>> {
    let full_len = input.len();

    // Capture the leading comment block at the top of the file.
    let (module_doc, mut comments) = leading_module_doc(input);

    let mut statements = Vec::new();
    let mut sources = Vec::new();
    loop {
        let preceding = full_len - input.len();
        let (doc, mut leading_comments) = ws_capturing_doc(input);
        for comment in &mut leading_comments {
            comment.offset += preceding;
        }
        comments.extend(leading_comments);
        if input.is_empty() {
            break;
        }
        // Byte offset of the statement start (stored temporarily in pos.line,
        // converted to real line:col by parse() after parsing completes).
        let offset = full_len - input.len();
        let (mut stmt, mut inner_comments, fields, expression, param_type_explicit) = alt((
            import_stmt.map(|(value, source)| (Statement::Import(value), Vec::new(), Vec::new(), source, false)),
            let_stmt.map(|(value, source)| (Statement::Let(value), Vec::new(), Vec::new(), source, false)),
            |input: &mut &'i str| {
                param_stmt(doc.clone(), input).map(|(value, explicit, source)| {
                    (
                        Statement::Param(value),
                        Vec::new(),
                        Vec::new(),
                        source.unwrap_or(""),
                        explicit,
                    )
                })
            },
            |input: &mut &'i str| {
                target_stmt(doc.clone(), input)
                    .map(|value| (Statement::Target(value), Vec::new(), Vec::new(), "", false))
            },
            |input: &mut &'i str| {
                output_stmt(doc.clone(), input)
                    .map(|(value, source)| (Statement::Output(value), Vec::new(), Vec::new(), source, false))
            },
            |input: &mut &'i str| {
                block_stmt(doc.clone(), input).map(|parsed| {
                    (
                        Statement::Block(parsed.block),
                        parsed.comments,
                        parsed.fields,
                        "",
                        false,
                    )
                })
            },
        ))
        .context(StrContext::Label("statement"))
        .parse_next(input)?;
        // Stash byte offset in pos.line — parse() will fix up to real line:col
        match &mut stmt {
            Statement::Import(i) => i.pos.line = offset,
            Statement::Block(b) => b.pos.line = offset,
            Statement::Let(l) => l.pos.line = offset,
            Statement::Param(p) => p.pos.line = offset,
            Statement::Target(t) => t.pos.line = offset,
            Statement::Output(o) => o.pos.line = offset,
        }
        for comment in &mut inner_comments {
            comment.offset += offset;
        }
        comments.extend(inner_comments);
        sources.push(StatementSource {
            start: offset,
            end: full_len - input.len(),
            expression,
            param_type_explicit,
            fields: fields
                .into_iter()
                .map(|mut field| {
                    field.start += offset;
                    field
                })
                .collect(),
        });
        statements.push(stmt);
    }
    Ok(ParsedModule {
        module: Module {
            doc: module_doc,
            statements,
        },
        comments,
        statements: sources,
    })
}

/// Extract the leading comment block if it's followed by a blank line
/// (making it "unattached" to any statement). Consumes the comment and
/// trailing blank line(s). If the comment runs directly into a statement
/// without a blank line separator, nothing is consumed and None is returned
/// (ws_capturing_doc will pick it up for the first statement instead).
fn leading_module_doc<'i>(input: &mut &'i str) -> (Option<String>, Vec<Comment<'i>>) {
    let start_len = input.len();
    // Skip leading whitespace (but not comments)
    let _ = take_while::<_, _, ErrMode<ContextError>>(0.., |c: char| c == ' ' || c == '\t' || c == '\r' || c == '\n')
        .parse_next(input);

    if !input.starts_with('#') {
        return (None, Vec::new());
    }

    // Save position — we'll only commit if we find a blank line after
    let saved = *input;

    // Collect comment lines
    let mut doc_lines: Vec<String> = Vec::new();
    let mut comments = Vec::new();
    loop {
        if !input.starts_with('#') {
            break;
        }
        let offset = start_len - input.len();
        let text = comment_line(input);
        comments.push(Comment { offset, text });
        let line = text
            .strip_prefix("# ")
            .unwrap_or_else(|| text.strip_prefix('#').unwrap_or(text));
        doc_lines.push(line.trim_end_matches('\r').to_owned());
    }

    // Check if there's a blank line (or EOF) separating this block from what follows
    let _ =
        take_while::<_, _, ErrMode<ContextError>>(0.., |c: char| c == ' ' || c == '\t' || c == '\r').parse_next(input);
    let has_blank = input.is_empty() || input.starts_with('\n');

    if has_blank {
        // Unattached comment — consume and return as module doc
        if !doc_lines.is_empty() {
            return (Some(doc_lines.join("\n")), comments);
        }
        (None, comments)
    } else {
        // Comment is directly attached to a statement — rewind
        *input = saved;
        (None, Vec::new())
    }
}

fn import_stmt<'i>(input: &mut &'i str) -> ModalResult<(Import, &'i str)> {
    keyword("import").parse_next(input)?;
    let before_url = *input;
    let url = cut_err(alt((plain_raw_string, plain_string)))
        .context(StrContext::Label("import URL string"))
        .parse_next(input)?;
    let url_source = &before_url[..before_url.len() - input.len()];
    // `plain_string` / `plain_raw_string` don't eat trailing whitespace, so
    // do it here before probing for the optional `as <ident>` clause.
    ws(input)?;
    let alias = opt(preceded(
        keyword("as"),
        cut_err(ident_string).context(StrContext::Label("import alias")),
    ))
    .parse_next(input)?;
    Ok((
        Import {
            pos: Pos::default(),
            url,
            alias,
        },
        url_source,
    ))
}

fn let_stmt<'i>(input: &mut &'i str) -> ModalResult<(Let, &'i str)> {
    keyword("let").parse_next(input)?;
    let name = cut_err(ident_string)
        .context(StrContext::Label("let binding name"))
        .parse_next(input)?;
    let typ = opt(preceded(lex(':'), typ)).parse_next(input)?;
    cut_err(lex('='))
        .context(StrContext::Label("'=' in let"))
        .parse_next(input)?;
    let before_value = *input;
    let value = cut_err(expr)
        .context(StrContext::Label("let value"))
        .parse_next(input)?;
    let value_source = &before_value[..before_value.len() - input.len()];
    Ok((
        Let {
            pos: Pos::default(),
            name,
            typ,
            value,
        },
        value_source,
    ))
}

fn param_stmt<'i>(doc: Option<String>, input: &mut &'i str) -> ModalResult<(Param, bool, Option<&'i str>)> {
    keyword("param").parse_next(input)?;
    let name = cut_err(ident_string)
        .context(StrContext::Label("param name"))
        .parse_next(input)?;

    // Type annotation is optional if a default value is provided.
    let explicit_type = opt(preceded(lex(':'), typ)).parse_next(input)?;
    let has_explicit_type = explicit_type.is_some();
    let (default, default_source) = if opt(lex('=')).parse_next(input)?.is_some() {
        let before_value = *input;
        let value = cut_err(expr).parse_next(input)?;
        (Some(value), Some(&before_value[..before_value.len() - input.len()]))
    } else {
        (None, None)
    };

    let t = match (explicit_type, &default) {
        (Some(t), _) => t,
        (None, Some(d)) => match infer_type(d) {
            Some(t) => t,
            None => {
                return cut_err(winnow::combinator::fail::<_, (Param, bool, Option<&'i str>), _>)
                    .context(StrContext::Label("cannot infer type for param; add explicit : type"))
                    .parse_next(input);
            }
        },
        (None, None) => {
            return cut_err(winnow::combinator::fail::<_, (Param, bool, Option<&'i str>), _>)
                .context(StrContext::Label("param requires a type or default value"))
                .parse_next(input);
        }
    };

    Ok((
        Param {
            pos: Pos::default(),
            name,
            doc,
            typ: t,
            default,
        },
        has_explicit_type,
        default_source,
    ))
}

fn formal_param(input: &mut &str) -> ModalResult<Param> {
    let name = ident_string.parse_next(input)?;
    let explicit_type = opt(preceded(lex(':'), typ)).parse_next(input)?;
    let default = if opt(lex('=')).parse_next(input)?.is_some() {
        Some(
            cut_err(expr)
                .context(StrContext::Label("parameter default"))
                .parse_next(input)?,
        )
    } else {
        None
    };
    let typ = match (explicit_type, &default) {
        (Some(typ), _) => typ,
        (None, Some(value)) => match infer_type(value) {
            Some(typ) => typ,
            None => {
                return cut_err(winnow::combinator::fail::<_, Param, _>)
                    .context(StrContext::Label("cannot infer parameter type; add : type"))
                    .parse_next(input);
            }
        },
        (None, None) => {
            return cut_err(winnow::combinator::fail::<_, Param, _>)
                .context(StrContext::Label("parameter requires a type or default value"))
                .parse_next(input);
        }
    };
    Ok(Param {
        pos: Pos::default(),
        name,
        doc: None,
        typ,
        default,
    })
}

fn formal_params(input: &mut &str) -> ModalResult<Vec<Param>> {
    delimited(
        lex('('),
        opt((separated(1.., formal_param, lex(',')), opt(lex(','))))
            .map(|params| params.map(|(values, _)| values).unwrap_or_default()),
        lex(')'),
    )
    .parse_next(input)
}

/// Infer the type of a parameter from its default expression.
fn infer_type(expr: &Expr) -> Option<Type> {
    match expr {
        Expr::Str(_) => Some(Type::String),
        Expr::Number(_) => Some(Type::Number),
        Expr::Bool(_) => Some(Type::Bool),
        Expr::Duration(_) => Some(Type::Duration),
        Expr::List(items) => {
            let elem = infer_type(items.first()?)?;
            Some(Type::List(Box::new(elem)))
        }
        Expr::Map(fields) => {
            let val_type = infer_type(&fields.first()?.value)?;
            Some(Type::Map(Box::new(val_type)))
        }
        Expr::Call(name, _) => crate::expr::builtin_return_type(name),
        Expr::Pipe(_, name, _) => crate::expr::builtin_return_type(name),
        Expr::If(_, then_val, _) => infer_type(then_val),
        Expr::Add(lhs, _) => infer_type(lhs),
        _ => None,
    }
}

fn target_stmt(doc: Option<String>, input: &mut &str) -> ModalResult<Target> {
    keyword("target").parse_next(input)?;
    let name = cut_err(ident_string)
        .context(StrContext::Label("target name"))
        .parse_next(input)?;
    let params = opt(formal_params).parse_next(input)?.unwrap_or_default();
    cut_err(lex('='))
        .context(StrContext::Label("'=' in target"))
        .parse_next(input)?;
    let blocks = cut_err(delimited(lex('['), separated(0.., target_call, lex(',')), lex(']')))
        .context(StrContext::Label("target block list"))
        .parse_next(input)?;
    Ok(Target {
        pos: Pos::default(),
        name,
        doc,
        params,
        blocks,
    })
}

fn target_call(input: &mut &str) -> ModalResult<TargetCall> {
    let name = dotted_ident.parse_next(input)?;
    let args = opt(delimited(
        lex('('),
        opt(named_argument_list).map(Option::unwrap_or_default),
        lex(')'),
    ))
    .parse_next(input)?
    .unwrap_or_default();
    Ok(TargetCall { name, args })
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

fn output_stmt<'i>(doc: Option<String>, input: &mut &'i str) -> ModalResult<(Output, &'i str)> {
    keyword("output").parse_next(input)?;
    let name = cut_err(ident_string)
        .context(StrContext::Label("output name"))
        .parse_next(input)?;
    cut_err(lex('='))
        .context(StrContext::Label("'=' in output"))
        .parse_next(input)?;
    let before_value = *input;
    let value = cut_err(expr)
        .context(StrContext::Label("output value"))
        .parse_next(input)?;
    let value_source = &before_value[..before_value.len() - input.len()];
    Ok((
        Output {
            pos: Pos::default(),
            name,
            doc,
            value,
        },
        value_source,
    ))
}

/// A field inside a block body, with optional trailing comma.
fn block_field<'i>(input: &mut &'i str) -> ModalResult<(Field, &'i str, Vec<Comment<'i>>)> {
    let start_len = input.len();
    let (field, expression) = field(input)?;
    opt(lex(',')).parse_next(input)?;
    // Skip any comments after this field (before the next field or closing brace)
    let consumed = start_len - input.len();
    let mut comments = ws_and_comments(input)?;
    for comment in &mut comments {
        comment.offset += consumed;
    }
    Ok((field, expression, comments))
}

fn block_stmt<'i>(doc: Option<String>, input: &mut &'i str) -> ModalResult<ParsedBlock<'i>> {
    let start_len = input.len();
    let phase = if opt(keyword("pre")).parse_next(input)?.is_some() {
        Phase::Pre
    } else if opt(keyword("post")).parse_next(input)?.is_some() {
        Phase::Post
    } else {
        Phase::Default
    };
    // `protected` and `explicit` are independent modifiers that may appear in
    // either order. Loop until neither matches; reject duplicates so we surface
    // typos like `protected protected name = ...` as parse errors.
    let mut protected = false;
    let mut explicit = false;
    loop {
        if opt(keyword("protected")).parse_next(input)?.is_some() {
            if protected {
                return Err(ErrMode::Cut(ContextError::new().add_context(
                    input,
                    &input.checkpoint(),
                    StrContext::Label("duplicate 'protected' modifier"),
                )));
            }
            protected = true;
        } else if opt(keyword("explicit")).parse_next(input)?.is_some() {
            if explicit {
                return Err(ErrMode::Cut(ContextError::new().add_context(
                    input,
                    &input.checkpoint(),
                    StrContext::Label("duplicate 'explicit' modifier"),
                )));
            }
            explicit = true;
        } else {
            break;
        }
    }
    let name = ident_string.parse_next(input)?;
    let params = opt(formal_params).parse_next(input)?.unwrap_or_default();

    // Optional matrix keys: name[key1, key2]
    let matrix_keys = if opt(lex('[')).parse_next(input)?.is_some() {
        if !params.is_empty() {
            return cut_err(winnow::combinator::fail::<_, ParsedBlock<'i>, _>)
                .context(StrContext::Label("a block cannot combine parameters and matrix keys"))
                .parse_next(input);
        }
        let keys: Vec<String> = separated(1.., ident_string, lex(',')).parse_next(input)?;
        cut_err(lex(']'))
            .context(StrContext::Label("closing ']' in matrix keys"))
            .parse_next(input)?;
        keys
    } else {
        vec![]
    };

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
    let comment_start = start_len - input.len();
    let mut comments = ws_and_comments(input)?;
    for comment in &mut comments {
        comment.offset += comment_start;
    }
    let mut fields = Vec::new();
    let mut field_starts = Vec::new();
    loop {
        let before = *input;
        let field_start = start_len - input.len();
        match block_field(input) {
            Ok((field, expression, mut field_comments)) => {
                for comment in &mut field_comments {
                    comment.offset += field_start;
                }
                comments.extend(field_comments);
                field_starts.push(FieldSource {
                    start: field_start,
                    expression,
                });
                fields.push(field);
            }
            Err(ErrMode::Backtrack(_)) => {
                *input = before;
                break;
            }
            Err(error) => return Err(error),
        }
    }
    cut_err(lex('}')).context(StrContext::Label("'}'")).parse_next(input)?;

    let (provider, resource) = if resource.is_empty() {
        (provider.clone(), provider)
    } else {
        (provider, resource)
    };

    Ok(ParsedBlock {
        block: Block {
            pos: Pos::default(),
            name,
            params,
            doc,
            phase,
            protected,
            explicit,
            matrix_keys,
            provider,
            resource,
            fields,
        },
        comments,
        fields: field_starts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_import() {
        let result = parse(r#"import "github.com/user/repo""#, "<test>").unwrap();
        assert_eq!(result.statements.len(), 1);
        match &result.statements[0] {
            Statement::Import(i) => {
                assert_eq!(i.url, "github.com/user/repo");
                assert_eq!(i.alias, None);
            }
            other => panic!("expected Import, got {other:?}"),
        }
    }

    #[test]
    fn parse_import_with_alias() {
        let result = parse(r#"import "github.com/user/repo" as bm"#, "<test>").unwrap();
        match &result.statements[0] {
            Statement::Import(i) => {
                assert_eq!(i.url, "github.com/user/repo");
                assert_eq!(i.alias.as_deref(), Some("bm"));
            }
            other => panic!("expected Import, got {other:?}"),
        }
    }

    #[test]
    fn parse_import_alias_requires_identifier() {
        // Missing identifier after `as` is a hard error.
        let err = parse(r#"import "github.com/user/repo" as"#, "<test>").unwrap_err();
        assert!(
            err.message.contains("import alias") || err.message.contains("alias"),
            "got: {err:?}"
        );
    }

    #[test]
    fn parse_import_with_ref() {
        let result = parse(r#"import "github.com/user/repo/path#v1.0""#, "<test>").unwrap();
        match &result.statements[0] {
            Statement::Import(i) => {
                assert_eq!(i.url, "github.com/user/repo/path#v1.0");
            }
            other => panic!("expected Import, got {other:?}"),
        }
    }

    #[test]
    fn parse_import_raw_string() {
        let result = parse("import 'github.com/user/repo'", "<test>").unwrap();
        match &result.statements[0] {
            Statement::Import(i) => {
                assert_eq!(i.url, "github.com/user/repo");
            }
            other => panic!("expected Import, got {other:?}"),
        }
    }

    #[test]
    fn parse_import_before_blocks() {
        let input = r#"
import "github.com/user/repo"

server = exec {
  command = "build"
  output = "out"
}
"#;
        let result = parse(input, "<test>").unwrap();
        assert_eq!(result.statements.len(), 2);
        assert!(matches!(&result.statements[0], Statement::Import(_)));
        assert!(matches!(&result.statements[1], Statement::Block(_)));
    }

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
        let result = parse(r#"let x = "hello #{name}""#, "<test>").unwrap();
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
        let result = parse(r##"let x = "#{exec("cmd") | trim}""##, "<test>").unwrap();
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
            Statement::Let(l) => assert_eq!(l.value, Expr::Number(42.into())),
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
    fn parse_duration_literals() {
        use crate::value::Duration;
        let cases: &[(&str, Duration)] = &[
            ("5s", Duration::from_secs(5)),
            ("500ms", Duration::from_millis(500)),
            ("2m", Duration::from_secs(120)),
            ("1h", Duration::from_secs(3600)),
            ("1.5h", Duration::from_secs(5400)),
            ("1d", Duration::from_secs(86400)),
        ];
        for (literal, expected) in cases {
            let src = format!("let t = {literal}");
            let result = parse(&src, "<test>").unwrap_or_else(|e| panic!("parse '{literal}': {e}"));
            match &result.statements[0] {
                Statement::Let(l) => assert_eq!(l.value, Expr::Duration(*expected), "literal {literal}"),
                _ => panic!("expected Let"),
            }
        }
    }

    #[test]
    fn parse_duration_vs_number_disambiguation() {
        // Plain digits with no suffix → Number.
        let result = parse("let x = 5", "<test>").unwrap();
        assert!(matches!(&result.statements[0], Statement::Let(l) if matches!(l.value, Expr::Number(_))));
        // Digits + duration unit → Duration.
        let result = parse("let x = 5s", "<test>").unwrap();
        assert!(matches!(&result.statements[0], Statement::Let(l) if matches!(l.value, Expr::Duration(_))));
    }

    #[test]
    fn parse_duration_type_annotation() {
        let result = parse("param timeout : duration = 30s", "<test>").unwrap();
        match &result.statements[0] {
            Statement::Param(p) => {
                assert_eq!(p.typ, Type::Duration);
                assert_eq!(p.default, Some(Expr::Duration(crate::value::Duration::from_secs(30))));
            }
            _ => panic!("expected Param"),
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
    fn parse_list_with_trailing_comma() {
        let result = parse("let x = [\n  \"a\",\n  \"b\",\n]", "<test>").unwrap();
        assert!(
            matches!(&result.statements[0], Statement::Let(l) if matches!(&l.value, Expr::List(items) if items.len() == 2))
        );
        assert!(parse("let x = []", "<test>").is_ok());
        assert!(parse("let x = [,]", "<test>").is_err());
        assert!(parse("let x = [1,,]", "<test>").is_err());
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
    fn parse_map_with_trailing_comma() {
        let result = parse("let x = {\n  a = 1,\n  b = 2,\n}", "<test>").unwrap();
        assert!(
            matches!(&result.statements[0], Statement::Let(l) if matches!(&l.value, Expr::Map(fields) if fields.len() == 2))
        );
        assert!(parse("let x = {}", "<test>").is_ok());
        assert!(parse("let x = {,}", "<test>").is_err());
        assert!(parse("let x = {a = 1,,}", "<test>").is_err());
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
    fn parse_provider_function_call() {
        let result = parse(r#"let packages = go.packages("./...")"#, "<test>").unwrap();
        match &result.statements[0] {
            Statement::Let(l) => match &l.value {
                Expr::Call(name, args) => {
                    assert_eq!(name, "go.packages");
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
                    assert_eq!(**then_val, Expr::Number(3.into()));
                    assert_eq!(**else_val, Expr::Number(1.into()));
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
        let result = parse("param replicas : number = 1", "<test>").unwrap();
        match &result.statements[0] {
            Statement::Param(p) => {
                assert_eq!(p.name, "replicas");
                assert_eq!(p.typ, Type::Number);
                assert_eq!(p.default, Some(Expr::Number(1.into())));
            }
            _ => panic!("expected Param"),
        }
    }

    #[test]
    fn parse_parameterized_block() {
        let result = parse(
            r#"image(tag : string, push = false) = exec { command = "build #{tag} #{push}" }"#,
            "<test>",
        )
        .unwrap();
        let Statement::Block(block) = &result.statements[0] else {
            panic!("expected block");
        };
        assert_eq!(block.name, "image");
        assert_eq!(block.params.len(), 2);
        assert_eq!(block.params[0].name, "tag");
        assert_eq!(block.params[0].typ, Type::String);
        assert_eq!(block.params[1].default, Some(Expr::Bool(false)));
    }

    #[test]
    fn parse_parameterized_target_calls_block_with_named_arguments() {
        let result = parse(
            r#"target publish(tag : string) = [image(tag = tag), notify(message = "done")]"#,
            "<test>",
        )
        .unwrap();
        let Statement::Target(target) = &result.statements[0] else {
            panic!("expected target");
        };
        assert_eq!(target.params.len(), 1);
        assert_eq!(target.blocks[0].name, "image");
        assert_eq!(target.blocks[0].args[0].name, "tag");
        assert_eq!(target.blocks[0].args[0].value, Expr::Ref(vec!["tag".into()]));
        assert_eq!(target.blocks[1].name, "notify");
    }

    #[test]
    fn parse_parameterized_block_reference_with_output() {
        let result = parse(
            r#"consumer = exec { command = compile(package = "api").path depends_on = [prepare(env = "prod")] }"#,
            "<test>",
        )
        .unwrap();
        let Statement::Block(block) = &result.statements[0] else {
            panic!("expected block");
        };
        assert_eq!(
            block.fields[0].value,
            Expr::BlockCall {
                name: "compile".into(),
                args: vec![Field {
                    name: "package".into(),
                    value: Expr::Str(vec![StringPart::Literal("api".into())]),
                }],
                fields: vec!["path".into()],
            }
        );
        let Expr::List(items) = &block.fields[1].value else {
            panic!("expected list");
        };
        assert!(matches!(&items[0], Expr::BlockCall { name, fields, .. } if name == "prepare" && fields.is_empty()));
    }

    #[test]
    fn parse_param_inferred_type() {
        let result = parse("param verbose = false", "<test>").unwrap();
        match &result.statements[0] {
            Statement::Param(p) => {
                assert_eq!(p.name, "verbose");
                assert_eq!(p.typ, Type::Bool);
                assert_eq!(p.default, Some(Expr::Bool(false)));
            }
            _ => panic!("expected Param"),
        }
    }

    #[test]
    fn parse_param_inferred_list() {
        let result = parse(r#"param tags = ["a", "b"]"#, "<test>").unwrap();
        match &result.statements[0] {
            Statement::Param(p) => {
                assert_eq!(p.typ, Type::List(Box::new(Type::String)));
            }
            _ => panic!("expected Param"),
        }
    }

    #[test]
    fn parse_param_inferred_int_list() {
        let result = parse("param ids = [1, 2]", "<test>").unwrap();
        match &result.statements[0] {
            Statement::Param(p) => {
                assert_eq!(p.typ, Type::List(Box::new(Type::Number)));
            }
            _ => panic!("expected Param"),
        }
    }

    #[test]
    fn parse_param_empty_list_fails() {
        assert!(parse("param items = []", "<test>").is_err());
    }

    #[test]
    fn parse_param_explicit_list_type() {
        let result = parse("param tags : [string]", "<test>").unwrap();
        match &result.statements[0] {
            Statement::Param(p) => {
                assert_eq!(p.typ, Type::List(Box::new(Type::String)));
                assert_eq!(p.default, None);
            }
            _ => panic!("expected Param"),
        }
    }

    #[test]
    fn parse_param_explicit_map_type() {
        let result = parse("param config : {string = number}", "<test>").unwrap();
        match &result.statements[0] {
            Statement::Param(p) => {
                assert_eq!(p.typ, Type::Map(Box::new(Type::Number)));
                assert_eq!(p.default, None);
            }
            _ => panic!("expected Param"),
        }
    }

    #[test]
    fn parse_param_nested_list_type() {
        let result = parse("param matrix : [[number]]", "<test>").unwrap();
        match &result.statements[0] {
            Statement::Param(p) => {
                assert_eq!(p.typ, Type::List(Box::new(Type::List(Box::new(Type::Number)))));
            }
            _ => panic!("expected Param"),
        }
    }

    #[test]
    fn parse_param_inferred_string() {
        let result = parse(r#"param name = "world""#, "<test>").unwrap();
        match &result.statements[0] {
            Statement::Param(p) => {
                assert_eq!(p.name, "name");
                assert_eq!(p.typ, Type::String);
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
                assert_eq!(
                    t.blocks.iter().map(|call| call.name.as_str()).collect::<Vec<_>>(),
                    vec!["server", "image"]
                );
            }
            _ => panic!("expected Target"),
        }
    }

    #[test]
    fn parse_target_dotted() {
        let result = parse("target deploy = [staging.deploy]", "<test>").unwrap();
        match &result.statements[0] {
            Statement::Target(t) => {
                assert_eq!(
                    t.blocks.iter().map(|call| call.name.as_str()).collect::<Vec<_>>(),
                    vec!["staging.deploy"]
                );
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
                assert!(!b.explicit);
                assert_eq!(b.provider, "aws");
                assert_eq!(b.resource, "aurora");
            }
            _ => panic!("expected Block"),
        }
    }

    #[test]
    fn parse_explicit_block() {
        let result = parse(r#"explicit migrate = exec { command = "./migrate" }"#, "<test>").unwrap();
        match &result.statements[0] {
            Statement::Block(b) => {
                assert!(!b.protected);
                assert!(b.explicit);
            }
            _ => panic!("expected Block"),
        }
    }

    #[test]
    fn parse_protected_and_explicit_either_order() {
        for src in [
            r#"protected explicit db = aws.aurora {}"#,
            r#"explicit protected db = aws.aurora {}"#,
        ] {
            let result = parse(src, "<test>").unwrap();
            match &result.statements[0] {
                Statement::Block(b) => {
                    assert!(b.protected, "protected for {src}");
                    assert!(b.explicit, "explicit for {src}");
                }
                _ => panic!("expected Block for {src}"),
            }
        }
    }

    #[test]
    fn parse_duplicate_modifier_is_error() {
        assert!(parse("protected protected db = aws.aurora {}", "<test>").is_err());
        assert!(parse("explicit explicit db = aws.aurora {}", "<test>").is_err());
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
    fn parsed_source_records_comments_and_field_boundaries() {
        let input = "# Heading\n\njob = exec {\n  command = \"# literal\" # inline\n  # field note\n  inputs = [1]\n}\n# section\n\nnext = exec {}\n";
        let parsed = parse_with_comments(input, "<test>").unwrap();
        assert_eq!(
            parsed.comments.iter().map(|comment| comment.text).collect::<Vec<_>>(),
            ["# Heading", "# inline", "# field note", "# section"]
        );
        for comment in &parsed.comments {
            assert!(input[comment.offset..].starts_with(comment.text));
        }
        assert_eq!(parsed.statements.len(), 2);
        assert_eq!(parsed.statements[0].fields.len(), 2);
        assert!(input[parsed.statements[0].fields[0].start..].starts_with("command"));
        assert!(input[parsed.statements[0].fields[1].start..].starts_with("inputs"));
        assert!(parsed.statements[0].fields[0].expression.starts_with("\"# literal\""));
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
            "  command = \"go build -o #{output}/server ./cmd/server\"\n",
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
            "param replicas    : number = 1\n",
            "\n",
            "let git_sha = exec(\"git rev-parse --short HEAD\") | trim\n",
            "\n",
            "server = go.binary { main = \"./cmd/server\" }\n",
            "\n",
            "image = docker.image {\n",
            "  tag = \"#{registry}/myapp:#{git_sha}\"\n",
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
        let input = "let x = <<EOF\n#{name} --version\nEOF\n";
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
        let input = "let x = <<-EOF\n  #{name} --version\n  #{name} graph\n  EOF\n";
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
        let input = "let x = <<EOF\nhello #{name}\nEOF\n";
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

    #[test]
    fn module_doc_from_leading_comment() {
        let input = "# This is the module description\n\nparam x : string\n";
        let result = parse(input, "<test>").unwrap();
        assert_eq!(result.doc.as_deref(), Some("This is the module description"));
        assert_eq!(result.statements.len(), 1);
    }

    #[test]
    fn module_doc_multiline() {
        let input = "# Line one\n# Line two\n\nlet x = 1\n";
        let result = parse(input, "<test>").unwrap();
        assert_eq!(result.doc.as_deref(), Some("Line one\nLine two"));
    }

    #[test]
    fn no_module_doc_when_attached() {
        // Comment directly before a param (no blank line) attaches to the param
        let input = "# Param doc\nparam x : string\n";
        let result = parse(input, "<test>").unwrap();
        assert_eq!(result.doc, None);
        match &result.statements[0] {
            Statement::Param(p) => assert_eq!(p.doc.as_deref(), Some("Param doc")),
            _ => panic!("expected Param"),
        }
    }

    #[test]
    fn module_doc_with_attached_param_doc() {
        let input = "# Module desc\n\n# Param doc\nparam x : string\n";
        let result = parse(input, "<test>").unwrap();
        assert_eq!(result.doc.as_deref(), Some("Module desc"));
        match &result.statements[0] {
            Statement::Param(p) => assert_eq!(p.doc.as_deref(), Some("Param doc")),
            _ => panic!("expected Param"),
        }
    }

    #[test]
    fn no_module_doc_when_empty() {
        let input = "let x = 1\n";
        let result = parse(input, "<test>").unwrap();
        assert_eq!(result.doc, None);
    }

    #[test]
    fn section_comment_between_blocks_not_attached() {
        // A comment separated from the following block by a blank line is
        // a section header, not a doc comment — it should not attach.
        let input = "first = exec { command = \"a\" }\n\n# ── Section ──\n\nsecond = exec { command = \"b\" }\n";
        let result = parse(input, "<test>").unwrap();
        assert_eq!(result.statements.len(), 2);
        match &result.statements[1] {
            Statement::Block(b) => assert_eq!(b.doc, None),
            _ => panic!("expected Block"),
        }
    }

    #[test]
    fn parse_raw_string() {
        let result = parse("let x = 'hello'", "<test>").unwrap();
        match &result.statements[0] {
            Statement::Let(l) => {
                assert_eq!(l.value, Expr::Str(vec![StringPart::Literal("hello".into())]));
            }
            _ => panic!("expected Let"),
        }
    }

    #[test]
    fn raw_string_no_escapes() {
        let result = parse(r"let x = 'hello\nworld'", "<test>").unwrap();
        match &result.statements[0] {
            Statement::Let(l) => {
                assert_eq!(l.value, Expr::Str(vec![StringPart::Literal("hello\\nworld".into())]));
            }
            _ => panic!("expected Let"),
        }
    }

    #[test]
    fn raw_string_no_interpolation() {
        let result = parse("let x = '#{name}'", "<test>").unwrap();
        match &result.statements[0] {
            Statement::Let(l) => {
                assert_eq!(l.value, Expr::Str(vec![StringPart::Literal("#{name}".into())]));
            }
            _ => panic!("expected Let"),
        }
    }

    #[test]
    fn raw_string_with_double_quotes() {
        let result = parse(r#"let x = 'say "hello"'"#, "<test>").unwrap();
        match &result.statements[0] {
            Statement::Let(l) => {
                assert_eq!(l.value, Expr::Str(vec![StringPart::Literal("say \"hello\"".into())]));
            }
            _ => panic!("expected Let"),
        }
    }

    #[test]
    fn raw_string_as_map_key() {
        let result = parse("let x = {'key' = 1}", "<test>").unwrap();
        match &result.statements[0] {
            Statement::Let(l) => match &l.value {
                Expr::Map(fields) => assert_eq!(fields[0].name, "key"),
                _ => panic!("expected Map"),
            },
            _ => panic!("expected Let"),
        }
    }

    #[test]
    fn infer_type_from_exec_call() {
        let result = parse("param x = exec('git rev-parse HEAD')", "<test>").unwrap();
        match &result.statements[0] {
            Statement::Param(p) => assert_eq!(p.typ, Type::String),
            _ => panic!("expected Param"),
        }
    }

    #[test]
    fn infer_type_from_pipe() {
        let result = parse("param x = exec('cmd') | lines", "<test>").unwrap();
        match &result.statements[0] {
            Statement::Param(p) => assert_eq!(p.typ, Type::List(Box::new(Type::String))),
            _ => panic!("expected Param"),
        }
    }

    #[test]
    fn infer_type_from_pipe_chain() {
        let result = parse("param x = exec('cmd') | trim", "<test>").unwrap();
        match &result.statements[0] {
            Statement::Param(p) => assert_eq!(p.typ, Type::String),
            _ => panic!("expected Param"),
        }
    }

    #[test]
    fn infer_type_from_glob() {
        let result = parse("param x = glob('*.rs')", "<test>").unwrap();
        match &result.statements[0] {
            Statement::Param(p) => assert_eq!(p.typ, Type::List(Box::new(Type::String))),
            _ => panic!("expected Param"),
        }
    }

    #[test]
    fn parse_union_type() {
        let result = parse("param x : string | [string]", "<test>").unwrap();
        match &result.statements[0] {
            Statement::Param(p) => {
                assert_eq!(
                    p.typ,
                    Type::Union(vec![Type::String, Type::List(Box::new(Type::String))])
                );
            }
            _ => panic!("expected Param"),
        }
    }

    #[test]
    fn parse_union_type_three() {
        let result = parse("param x : string | number | bool", "<test>").unwrap();
        match &result.statements[0] {
            Statement::Param(p) => {
                assert_eq!(p.typ, Type::Union(vec![Type::String, Type::Number, Type::Bool]));
            }
            _ => panic!("expected Param"),
        }
    }

    #[test]
    fn parse_single_type_no_union() {
        let result = parse("param x : string", "<test>").unwrap();
        match &result.statements[0] {
            Statement::Param(p) => assert_eq!(p.typ, Type::String),
            _ => panic!("expected Param"),
        }
    }

    #[test]
    fn parse_matrix_block() {
        let input = r#"
param arch = ["amd64", "arm64"]
image[arch] = exec {
  command = "build #{arch}"
  output = "out"
}
"#;
        let result = parse(input, "<test>").unwrap();
        match &result.statements[1] {
            Statement::Block(b) => {
                assert_eq!(b.name, "image");
                assert_eq!(b.matrix_keys, vec!["arch"]);
            }
            _ => panic!("expected Block"),
        }
    }

    #[test]
    fn parse_matrix_block_multi_key() {
        let input = r#"
image[arch, region] = exec {
  command = "build"
  output = "out"
}
"#;
        let result = parse(input, "<test>").unwrap();
        match &result.statements[0] {
            Statement::Block(b) => {
                assert_eq!(b.matrix_keys, vec!["arch", "region"]);
            }
            _ => panic!("expected Block"),
        }
    }

    #[test]
    fn parse_block_no_matrix() {
        let input = "a = exec {\n  command = \"build\"\n  output = \"out\"\n}\n";
        let result = parse(input, "<test>").unwrap();
        match &result.statements[0] {
            Statement::Block(b) => assert!(b.matrix_keys.is_empty()),
            _ => panic!("expected Block"),
        }
    }

    #[test]
    fn parse_matrix_ref_quoted() {
        let input = r#"let x = build["amd64", "cachew"].path"#;
        let result = parse(input, "<test>").unwrap();
        match &result.statements[0] {
            Statement::Let(l) => {
                assert_eq!(
                    l.value,
                    Expr::MatrixRef {
                        name: "build".into(),
                        keys: vec![
                            Expr::Str(vec![StringPart::Literal("amd64".into())]),
                            Expr::Str(vec![StringPart::Literal("cachew".into())]),
                        ],
                        fields: vec!["path".into()],
                    }
                );
            }
            _ => panic!("expected Let"),
        }
    }

    #[test]
    fn parse_matrix_ref_single_key() {
        let input = r#"let x = build["amd64"].path"#;
        let result = parse(input, "<test>").unwrap();
        match &result.statements[0] {
            Statement::Let(l) => {
                assert_eq!(
                    l.value,
                    Expr::MatrixRef {
                        name: "build".into(),
                        keys: vec![Expr::Str(vec![StringPart::Literal("amd64".into())])],
                        fields: vec!["path".into()],
                    }
                );
            }
            _ => panic!("expected Let"),
        }
    }

    #[test]
    fn parse_matrix_ref_bare_ident() {
        let input = "let x = build[amd64].path";
        let result = parse(input, "<test>").unwrap();
        match &result.statements[0] {
            Statement::Let(l) => {
                assert_eq!(
                    l.value,
                    Expr::MatrixRef {
                        name: "build".into(),
                        keys: vec![Expr::Ref(vec!["amd64".into()])],
                        fields: vec!["path".into()],
                    }
                );
            }
            _ => panic!("expected Let"),
        }
    }

    #[test]
    fn parse_matrix_ref_typed_keys() {
        let input = r#"let x = build["1", 1, true, 5s].path"#;
        let result = parse(input, "<test>").unwrap();
        match &result.statements[0] {
            Statement::Let(l) => {
                assert_eq!(
                    l.value,
                    Expr::MatrixRef {
                        name: "build".into(),
                        keys: vec![
                            Expr::Str(vec![StringPart::Literal("1".into())]),
                            Expr::Number(1.into()),
                            Expr::Bool(true),
                            Expr::Duration(crate::value::Duration::from_secs(5)),
                        ],
                        fields: vec!["path".into()],
                    }
                );
            }
            _ => panic!("expected Let"),
        }
    }

    #[test]
    fn parse_matrix_ref_reference_key() {
        let input = r#"let x = consumer[crate["core"]].path"#;
        let result = parse(input, "<test>").unwrap();
        match &result.statements[0] {
            Statement::Let(l) => {
                assert_eq!(
                    l.value,
                    Expr::MatrixRef {
                        name: "consumer".into(),
                        keys: vec![Expr::MatrixRef {
                            name: "crate".into(),
                            keys: vec![Expr::Str(vec![StringPart::Literal("core".into())])],
                            fields: vec![],
                        }],
                        fields: vec!["path".into()],
                    }
                );
            }
            _ => panic!("expected Let"),
        }
    }
}
