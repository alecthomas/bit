//! Source formatter for `.bit` files. Formatting the AST would discard comments
//! and the original spelling of strings and heredocs, so this works on source.

use crate::ast::{Module, Pos, Statement};
use crate::parser::{self, ParseError};

#[derive(Debug, thiserror::Error)]
pub enum FormatError {
    #[error(transparent)]
    Parse(#[from] ParseError),
    #[error("formatting would change the meaning of the file")]
    ChangedMeaning,
}

/// Format source while retaining comments and literal contents verbatim.
pub fn format(source: &str, filename: &str) -> Result<String, FormatError> {
    let mut before = parser::parse(source, filename)?;
    let mut result = String::with_capacity(source.len());
    let mut depth = 0usize;
    let mut quote = Vec::new();
    let mut heredoc: Option<String> = None;

    for line in source.split_inclusive('\n') {
        if let Some(label) = &heredoc {
            result.push_str(line);
            if is_heredoc_end(line, label) {
                heredoc = None;
            }
            continue;
        }

        let (content, newline) = if let Some(content) = line.strip_suffix("\r\n") {
            (content, "\r\n")
        } else if let Some(content) = line.strip_suffix('\n') {
            (content, "\n")
        } else {
            (line, "")
        };

        if quote.is_empty() && content.trim().is_empty() {
            result.push_str(newline);
            continue;
        }

        let was_in_quote = !quote.is_empty();
        let (tokens, next_heredoc) = tokenize(content, &mut quote);
        if was_in_quote {
            // Continuation lines are literal content, including their indentation.
            result.push_str(line);
        } else {
            let leading_close = tokens.first().is_some_and(|token| token.text == "}");
            let indent = depth.saturating_sub(usize::from(leading_close));
            result.push_str(&"  ".repeat(indent));
            for (index, token) in tokens.iter().enumerate() {
                if index > 0 {
                    let previous = &tokens[index - 1];
                    if token.comment {
                        result.push_str("  ");
                    } else if needs_space(previous.text, token.text) {
                        result.push(' ');
                    }
                }
                result.push_str(token.text);
            }
            result.push_str(newline);
        }

        for token in tokens {
            match token.text {
                "{" => depth += 1,
                "}" => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
        heredoc = next_heredoc;
        if !quote.is_empty() {
            scan_quoted(newline, &mut quote);
        }
    }

    if !source.is_empty() && !result.ends_with('\n') {
        result.push('\n');
    }

    let mut after = parser::parse(&result, filename)?;
    clear_positions(&mut before);
    clear_positions(&mut after);
    if before != after {
        return Err(FormatError::ChangedMeaning);
    }
    Ok(result)
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

fn is_heredoc_end(line: &str, label: &str) -> bool {
    line.trim_start_matches([' ', '\t'])
        .strip_prefix(label)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(['\n', '\r']))
}

struct Token<'a> {
    text: &'a str,
    comment: bool,
}

fn tokenize<'a>(line: &'a str, quote: &mut Vec<QuoteFrame>) -> (Vec<Token<'a>>, Option<String>) {
    let mut tokens = Vec::new();
    let mut heredoc = None;
    let mut offset = 0;
    while offset < line.len() {
        let rest = &line[offset..];
        let Some(current) = rest.chars().next() else {
            break;
        };
        if !quote.is_empty() {
            let end = scan_quoted(rest, quote);
            tokens.push(Token {
                text: &rest[..end],
                comment: false,
            });
            offset += end;
            continue;
        }
        if current.is_whitespace() {
            offset += current.len_utf8();
            continue;
        }
        if current == '#' && (tokens.is_empty() || rest[1..].chars().next().is_none_or(char::is_whitespace)) {
            tokens.push(Token {
                text: rest,
                comment: true,
            });
            break;
        }
        if current == '\'' || current == '"' {
            quote.push(QuoteFrame::String {
                delimiter: current,
                escaped: false,
            });
            let end = scan_quoted(&rest[current.len_utf8()..], quote) + current.len_utf8();
            let text = &rest[..end];
            tokens.push(Token { text, comment: false });
            offset += end;
            continue;
        }
        if let Some(opener) = rest.strip_prefix("<<") {
            let label_start = usize::from(opener.starts_with('-'));
            let label_len = opener[label_start..]
                .bytes()
                .take_while(|byte| byte.is_ascii_alphanumeric() || *byte == b'_')
                .count();
            if label_len > 0 {
                let end = 2 + label_start + label_len;
                heredoc = Some(opener[label_start..label_start + label_len].to_owned());
                tokens.push(Token {
                    text: &rest[..end],
                    comment: false,
                });
                offset += end;
                continue;
            }
        }
        let symbol = if rest.starts_with("==") || rest.starts_with("!=") {
            Some(2)
        } else if "{}[](),.=+|:".contains(current) {
            Some(current.len_utf8())
        } else {
            None
        };
        if let Some(length) = symbol {
            tokens.push(Token {
                text: &rest[..length],
                comment: false,
            });
            offset += length;
            continue;
        }
        let length = rest
            .char_indices()
            .skip(1)
            .find(|(_, character)| character.is_whitespace() || "{}[](),.=+|:'\"#".contains(*character))
            .map_or(rest.len(), |(index, _)| index);
        tokens.push(Token {
            text: &rest[..length],
            comment: false,
        });
        offset += length;
    }
    (tokens, heredoc)
}

enum QuoteFrame {
    String { delimiter: char, escaped: bool },
    Interpolation { depth: usize },
}

fn scan_quoted(rest: &str, frames: &mut Vec<QuoteFrame>) -> usize {
    let mut offset = 0;
    while offset < rest.len() && !frames.is_empty() {
        let Some(current) = rest[offset..].chars().next() else {
            break;
        };
        let mut pop = false;
        let mut push = None;
        let mut length = current.len_utf8();
        match frames.last_mut() {
            Some(QuoteFrame::String { delimiter, escaped }) => {
                if *escaped {
                    *escaped = false;
                } else if *delimiter == '"' && current == '\\' {
                    *escaped = true;
                } else if *delimiter == '"' && rest[offset..].starts_with("#{") {
                    push = Some(QuoteFrame::Interpolation { depth: 1 });
                    length = 2;
                } else if current == *delimiter {
                    pop = true;
                }
            }
            Some(QuoteFrame::Interpolation { depth }) => {
                if current == '\'' || current == '"' {
                    push = Some(QuoteFrame::String {
                        delimiter: current,
                        escaped: false,
                    });
                } else if current == '{' {
                    *depth += 1;
                } else if current == '}' {
                    *depth -= 1;
                    pop = *depth == 0;
                }
            }
            None => break,
        }
        offset += length;
        if pop {
            frames.pop();
        }
        if let Some(frame) = push {
            frames.push(frame);
        }
    }
    offset
}

fn needs_space(previous: &str, current: &str) -> bool {
    if current == "." || previous == "." {
        return false;
    }
    if matches!(current, "," | ")" | "]") || matches!(previous, "(" | "[") {
        return false;
    }
    if matches!(current, "(" | "[") {
        return matches!(previous, "=" | "+" | "|" | "==" | "!=");
    }
    if current == "}" && previous == "{" {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::format;

    #[test]
    fn formats_spacing_and_indentation_without_moving_comments() {
        let source = "#Heading\n\n# Block docs\njob=exec{\n  # Field docs\ncommand=\"# literal #{name}\"#  inline\n  inputs=[1,2]\n}\n# Tail\n";
        let expected = "#Heading\n\n# Block docs\njob = exec {\n  # Field docs\n  command = \"# literal #{name}\"  #  inline\n  inputs = [1, 2]\n}\n# Tail\n";
        assert_eq!(format(source, "BUILD.bit").unwrap(), expected);
        assert_eq!(format(expected, "BUILD.bit").unwrap(), expected);
    }

    #[test]
    fn leaves_heredoc_and_multiline_string_contents_intact() {
        let source = "job=exec{\ncommand=<<-EOF\n    # Keep indentation\n    echo #{name}\n  EOF\nother='line one\n    \n  # line two'\n}\n";
        let formatted = format(source, "BUILD.bit").unwrap();
        assert!(formatted.contains("    # Keep indentation\n    echo #{name}\n  EOF\n"));
        assert!(formatted.contains("other = 'line one\n    \n  # line two'\n"));
        assert_eq!(format(&formatted, "BUILD.bit").unwrap(), formatted);
    }

    #[test]
    fn preserves_quotes_and_hashes_inside_interpolation() {
        let source = r##"let message="hello #{exec("printf '# done'")}"  # note
"##;
        let formatted = format(source, "BUILD.bit").unwrap();
        assert_eq!(
            formatted,
            r##"let message = "hello #{exec("printf '# done'")}"  # note
"##
        );
        assert_eq!(format(&formatted, "BUILD.bit").unwrap(), formatted);
    }

    #[test]
    fn rejects_invalid_source() {
        assert!(format("job = exec {", "BUILD.bit").is_err());
    }

    #[test]
    fn formats_repository_build_file_without_changing_its_meaning() {
        let formatted = format(include_str!("../BUILD.bit"), "BUILD.bit").unwrap();
        assert_eq!(format(&formatted, "BUILD.bit").unwrap(), formatted);
    }
}
