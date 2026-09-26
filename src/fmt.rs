//! Canonical source formatting from the parser's semantic AST and comments.

use crate::ast::{Block, Module, Phase, Pos, Statement};
use crate::parser::{self, Comment, ParseError, StatementSource};

#[derive(Debug, thiserror::Error)]
pub enum FormatError {
    #[error(transparent)]
    Parse(#[from] ParseError),
    #[error("formatting would change the meaning of the file")]
    ChangedMeaning,
    #[error("formatting would change the comments in the file")]
    ChangedComments,
}

pub fn format(source: &str, filename: &str) -> Result<String, FormatError> {
    let parsed = parser::parse_with_comments(source, filename)?;
    let mut result = String::new();
    let mut comment_index = 0;

    for (index, (statement, syntax)) in parsed.module.statements.iter().zip(&parsed.statements).enumerate() {
        let leading_start = comment_index;
        while comment_index < parsed.comments.len() && parsed.comments[comment_index].offset < syntax.start {
            comment_index += 1;
        }
        let leading = &parsed.comments[leading_start..comment_index];
        let doc_count = statement_doc(statement).map_or(0, |doc| doc.split('\n').count());
        let ordinary_count = leading.len().saturating_sub(doc_count);
        let mut ordinary_start = 0;

        if index == 0 {
            let module_doc_count = parsed.module.doc.as_ref().map_or(0, |doc| doc.split('\n').count());
            for comment in leading.iter().take(module_doc_count) {
                push_comment(&mut result, *comment, 0);
            }
            if module_doc_count > 0 {
                result.push('\n');
            }
            ordinary_start = module_doc_count;
        } else {
            // A comment parsed as documentation must stay before its statement,
            // even if it was originally written after the previous one.
            for comment in &leading[..ordinary_count] {
                if is_inline(source, comment.offset) {
                    push_inline_comment(&mut result, *comment);
                    ordinary_start += 1;
                } else {
                    break;
                }
            }
            result.push('\n');
        }

        for comment in &leading[ordinary_start..ordinary_count] {
            push_comment(&mut result, *comment, 0);
        }
        if ordinary_start < ordinary_count && !matches!(statement, Statement::Import(_) | Statement::Let(_)) {
            // Detached section comments must not become statement docs.
            result.push('\n');
        }
        for comment in &leading[ordinary_count..] {
            push_comment(&mut result, *comment, 0);
        }

        let inner_start = comment_index;
        while comment_index < parsed.comments.len() && parsed.comments[comment_index].offset < syntax.end {
            comment_index += 1;
        }
        render_statement(
            &mut result,
            statement,
            syntax,
            &parsed.comments[inner_start..comment_index],
            source,
        );
        result.push('\n');
    }

    for comment in &parsed.comments[comment_index..] {
        if !result.is_empty() && is_inline(source, comment.offset) {
            push_inline_comment(&mut result, *comment);
        } else {
            if !result.is_empty() && !result.ends_with("\n\n") {
                result.push('\n');
            }
            push_comment(&mut result, *comment, 0);
        }
    }

    let after_parsed = parser::parse_with_comments(&result, filename)?;
    if !parsed
        .comments
        .iter()
        .map(|comment| comment.text.trim_end_matches('\r'))
        .eq(after_parsed.comments.iter().map(|comment| comment.text))
    {
        return Err(FormatError::ChangedComments);
    }
    let mut before = parsed.module;
    let mut after = after_parsed.module;
    clear_positions(&mut before);
    clear_positions(&mut after);
    if before != after {
        return Err(FormatError::ChangedMeaning);
    }
    Ok(result)
}

fn statement_doc(statement: &Statement) -> Option<&str> {
    match statement {
        Statement::Block(value) => value.doc.as_deref(),
        Statement::Param(value) => value.doc.as_deref(),
        Statement::Target(value) => value.doc.as_deref(),
        Statement::Output(value) => value.doc.as_deref(),
        Statement::Import(_) | Statement::Let(_) => None,
    }
}

fn is_inline(source: &str, offset: usize) -> bool {
    source[..offset]
        .rsplit('\n')
        .next()
        .is_some_and(|line| !line.trim().is_empty())
}

fn push_comment(result: &mut String, comment: Comment<'_>, indent: usize) {
    result.push_str(&" ".repeat(indent));
    result.push_str(comment.text.trim_end_matches('\r'));
    result.push('\n');
}

fn push_inline_comment(result: &mut String, comment: Comment<'_>) {
    if result.ends_with('\n') {
        result.pop();
    }
    result.push_str("  ");
    result.push_str(comment.text.trim_end_matches('\r'));
    result.push('\n');
}

fn render_statement(
    result: &mut String,
    statement: &Statement,
    syntax: &StatementSource,
    comments: &[Comment<'_>],
    source: &str,
) {
    match statement {
        Statement::Import(value) => {
            result.push_str("import ");
            result.push_str(syntax.expression.trim_end());
            if let Some(alias) = &value.alias {
                result.push_str(" as ");
                result.push_str(alias);
            }
        }
        Statement::Let(value) => {
            result.push_str("let ");
            result.push_str(&value.name);
            if let Some(typ) = &value.typ {
                result.push_str(&format!(" : {typ}"));
            }
            result.push_str(" = ");
            result.push_str(syntax.expression.trim_end());
        }
        Statement::Param(value) => {
            result.push_str("param ");
            result.push_str(&value.name);
            if syntax.param_type_explicit {
                result.push_str(&format!(" : {}", value.typ));
            }
            if value.default.is_some() {
                result.push_str(" = ");
                result.push_str(syntax.expression.trim_end());
            }
        }
        Statement::Target(value) => {
            result.push_str("target ");
            result.push_str(&value.name);
            result.push_str(" = [");
            result.push_str(&value.blocks.join(", "));
            result.push(']');
        }
        Statement::Output(value) => {
            result.push_str("output ");
            result.push_str(&value.name);
            result.push_str(" = ");
            result.push_str(syntax.expression.trim_end());
        }
        Statement::Block(value) => render_block(result, value, syntax, comments, source),
    }
}

fn render_block(result: &mut String, block: &Block, syntax: &StatementSource, comments: &[Comment<'_>], source: &str) {
    match block.phase {
        Phase::Pre => result.push_str("pre "),
        Phase::Post => result.push_str("post "),
        Phase::Default => {}
    }
    if block.protected {
        result.push_str("protected ");
    }
    if block.explicit {
        result.push_str("explicit ");
    }
    result.push_str(&block.name);
    if !block.matrix_keys.is_empty() {
        result.push('[');
        result.push_str(&block.matrix_keys.join(", "));
        result.push(']');
    }
    result.push_str(" = ");
    result.push_str(&block.provider);
    if block.resource != block.provider {
        result.push('.');
        result.push_str(&block.resource);
    }
    if block.fields.is_empty() && comments.is_empty() {
        result.push_str(" {}");
        return;
    }
    result.push_str(" {\n");

    let mut comment_index = 0;
    for (index, field) in block.fields.iter().enumerate() {
        let field_start = syntax.fields[index].start;
        while comment_index < comments.len() && comments[comment_index].offset < field_start {
            let comment = comments[comment_index];
            if index > 0 && is_inline(source, comment.offset) {
                push_inline_comment(result, comment);
            } else {
                push_comment(result, comment, 2);
            }
            comment_index += 1;
        }
        result.push_str("  ");
        result.push_str(&field.name);
        result.push_str(" = ");
        result.push_str(syntax.fields[index].expression.trim_end());
        result.push('\n');
    }
    for comment in &comments[comment_index..] {
        if !block.fields.is_empty() && is_inline(source, comment.offset) {
            push_inline_comment(result, *comment);
        } else {
            push_comment(result, *comment, 2);
        }
    }
    result.push('}');
}

fn clear_positions(module: &mut Module) {
    for statement in &mut module.statements {
        let position = match statement {
            Statement::Import(value) => &mut value.pos,
            Statement::Block(value) => &mut value.pos,
            Statement::Let(value) => &mut value.pos,
            Statement::Param(value) => &mut value.pos,
            Statement::Target(value) => &mut value.pos,
            Statement::Output(value) => &mut value.pos,
        };
        *position = Pos::default();
    }
}

#[cfg(test)]
mod tests {
    use super::format;

    #[test]
    fn formats_comments_and_block_spacing() {
        let source = "# Heading\n\n# Block docs\njob=exec{\n  # Field docs\ncommand='echo' # inline\n}\nother=exec{}\n";
        let expected = "# Heading\n\n# Block docs\njob = exec {\n  # Field docs\n  command = 'echo'  # inline\n}\n\nother = exec {}\n";
        assert_eq!(format(source, "BUILD.bit").unwrap(), expected);
        assert_eq!(format(expected, "BUILD.bit").unwrap(), expected);
    }

    #[test]
    fn preserves_heredoc_and_multiline_string_spelling() {
        let source = "job=exec{\ncommand=<<-EOF\n    echo #{name}\n  EOF\nother='line one\nline two'\n}\n";
        let formatted = format(source, "BUILD.bit").unwrap();
        assert!(formatted.contains("command = <<-EOF\n    echo #{name}\n  EOF\n"));
        assert!(formatted.contains("other = 'line one\nline two'"));
        assert_eq!(format(&formatted, "BUILD.bit").unwrap(), formatted);
    }

    #[test]
    fn keeps_literal_interpolation_markers_literal() {
        let source = r#"let message = "\#{literal} #{name}""#;
        let formatted = format(source, "BUILD.bit").unwrap();
        assert_eq!(format(&formatted, "BUILD.bit").unwrap(), formatted);
    }

    #[test]
    fn keeps_detached_comments_detached() {
        let source = "first=exec{}\n# section\n\n# docs\nsecond=exec{}\n";
        let expected = "first = exec {}\n\n# section\n\n# docs\nsecond = exec {}\n";
        assert_eq!(format(source, "BUILD.bit").unwrap(), expected);
        assert_eq!(format(expected, "BUILD.bit").unwrap(), expected);
    }

    #[test]
    fn keeps_leading_comment_before_let_out_of_module_doc() {
        let source = "# section\nlet value=1\n";
        let expected = "# section\nlet value = 1\n";
        assert_eq!(format(source, "BUILD.bit").unwrap(), expected);
    }

    #[test]
    fn keeps_comment_adjacent_to_let_after_a_block() {
        let source = "job=exec{}\n# Explains the binding.\nlet tree_sitter_rev=exec('git')|trim\n";
        let expected = "job = exec {}\n\n# Explains the binding.\nlet tree_sitter_rev = exec('git')|trim\n";
        assert_eq!(format(source, "BUILD.bit").unwrap(), expected);
        assert_eq!(format(expected, "BUILD.bit").unwrap(), expected);
    }

    #[test]
    fn moves_attached_comment_after_block_before_next_statement() {
        let source = "first=exec{} # next block\nsecond=exec{}\n";
        let expected = "first = exec {}\n\n# next block\nsecond = exec {}\n";
        assert_eq!(format(source, "BUILD.bit").unwrap(), expected);
        assert_eq!(format(expected, "BUILD.bit").unwrap(), expected);
    }

    #[test]
    fn normalizes_crlf_and_preserves_comment_text() {
        let source = "# Heading\r\n\r\njob=exec{}\r\n";
        assert_eq!(format(source, "BUILD.bit").unwrap(), "# Heading\n\njob = exec {}\n");
    }

    #[test]
    fn formats_all_statement_and_expression_forms() {
        let source = "import 'example.com/mod' as mod\nparam names:[string]=['one','two']\nlet selected=if true then [1]+[2] else [3]\noutput chosen=selected[0].name\npre protected explicit job[names]=exec{config={answer=42}, command=\"#{mod.value | trim}\"}\ntarget default=[job]\n";
        let formatted = format(source, "BUILD.bit").unwrap();
        assert_eq!(format(&formatted, "BUILD.bit").unwrap(), formatted);
    }

    #[test]
    fn preserves_nested_expression_spelling() {
        let source = "let branch=if env('MODE')=='prod' then exec('cmd')|trim else 'dev'\nlet options={'a b'=1,'x\"y'=2}\nlet literal='line one\nline two'\n";
        let formatted = format(source, "BUILD.bit").unwrap();
        assert_eq!(format(&formatted, "BUILD.bit").unwrap(), formatted);
    }

    #[test]
    fn rejects_invalid_source() {
        assert!(format("job = exec {", "BUILD.bit").is_err());
    }

    #[test]
    fn formats_repository_build_files_without_changing_their_meaning() {
        for source in [include_str!("../BUILD.bit"), include_str!("../example/BUILD.bit")] {
            let formatted = format(source, "BUILD.bit").unwrap();
            assert_eq!(format(&formatted, "BUILD.bit").unwrap(), formatted);
        }
    }

    #[test]
    fn repository_build_file_keeps_heredocs_and_let_comment() {
        let formatted = format(include_str!("../BUILD.bit"), "BUILD.bit").unwrap();
        assert!(formatted.contains("command = <<-EOF\n"));
        assert!(formatted.contains("# extension's grammar revision.\nlet tree_sitter_rev"));
    }
}
