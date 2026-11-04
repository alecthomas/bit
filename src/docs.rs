//! bit's own documentation, for `bit --docs`.
//!
//! The README is embedded at build time so the text always matches the
//! installed binary. Topics are README headings, addressed by their GitHub
//! anchor slug, so `[large](#blocks)` in the README and `bit --docs blocks`
//! name the same section.

use std::ops::Range;

use pulldown_cmark::{Alignment, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use yansi::{Color, Paint, Style};

/// The README this binary was built with.
pub const README: &str = include_str!("../README.md");

/// Wrapping wider than this makes prose hard to read, however wide the
/// terminal is.
const MAX_WIDTH: usize = 100;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DocsError {
    #[error("unknown docs topic '{topic}'; available topics:\n{}", list_topics(available))]
    Unknown { topic: String, available: Vec<String> },
    #[error("docs topic '{topic}' is ambiguous; it matches:\n{}", list_topics(matches))]
    Ambiguous { topic: String, matches: Vec<String> },
}

fn list_topics(topics: &[String]) -> String {
    topics.iter().map(|t| format!("  {t}")).collect::<Vec<_>>().join("\n")
}

/// A heading and everything up to the next heading at the same or a higher
/// level.
struct Section {
    slug: String,
    range: Range<usize>,
}

/// A Markdown document split into topics.
pub struct Docs<'a> {
    source: &'a str,
    sections: Vec<Section>,
}

impl<'a> Docs<'a> {
    /// Index the headings of `source`. The level-one heading titles the whole
    /// document, so it is not a topic.
    pub fn new(source: &'a str) -> Self {
        let mut headings: Vec<(HeadingLevel, String, usize)> = Vec::new();
        let mut current: Option<(HeadingLevel, String, usize)> = None;
        for (event, range) in Parser::new_ext(source, options()).into_offset_iter() {
            match event {
                Event::Start(Tag::Heading { level, .. }) => current = Some((level, String::new(), range.start)),
                Event::Text(text) | Event::Code(text) => {
                    if let Some((_, title, _)) = current.as_mut() {
                        title.push_str(&text);
                    }
                }
                Event::End(TagEnd::Heading(_)) => headings.extend(current.take()),
                _ => {}
            }
        }

        let mut seen: Vec<String> = Vec::new();
        let mut sections = Vec::new();
        for (i, (level, title, start)) in headings.iter().enumerate() {
            let slug = unique_slug(&slugify(title), &seen);
            seen.push(slug.clone());
            if *level == HeadingLevel::H1 {
                continue;
            }
            let end = headings[i + 1..]
                .iter()
                .find(|(next, _, _)| next <= level)
                .map_or(source.len(), |(_, _, next_start)| *next_start);
            sections.push(Section {
                slug,
                range: *start..end,
            });
        }
        Self { source, sections }
    }

    /// The Markdown for `topic`, or the whole document when there is none.
    ///
    /// A topic matches a heading whose slug equals the topic's slug, so case
    /// and punctuation don't matter. Failing that, it matches the headings
    /// whose slug starts with the topic's words, so `ci` finds "CI workflows".
    pub fn select(&self, topic: Option<&str>) -> Result<&'a str, DocsError> {
        let Some(topic) = topic else {
            return Ok(self.source);
        };
        let key = slugify(topic);
        if let Some(section) = self.sections.iter().find(|s| s.slug == key) {
            return Ok(&self.source[section.range.clone()]);
        }
        let prefixed: Vec<&Section> = self
            .sections
            .iter()
            .filter(|s| !key.is_empty() && s.slug.strip_prefix(&key).is_some_and(|rest| rest.starts_with('-')))
            .collect();
        match prefixed.as_slice() {
            [section] => Ok(&self.source[section.range.clone()]),
            [] => Err(DocsError::Unknown {
                topic: topic.to_owned(),
                available: self.topics(),
            }),
            matches => Err(DocsError::Ambiguous {
                topic: topic.to_owned(),
                matches: matches.iter().map(|s| s.slug.clone()).collect(),
            }),
        }
    }

    /// Every topic name, in document order.
    pub fn topics(&self) -> Vec<String> {
        self.sections.iter().map(|s| s.slug.clone()).collect()
    }
}

fn options() -> Options {
    Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH
}

/// GitHub's heading anchor: lowercase, punctuation other than `-` and `_`
/// removed, and spaces turned into `-`.
fn slugify(title: &str) -> String {
    title
        .trim()
        .chars()
        .filter_map(|c| match c {
            ' ' => Some('-'),
            '-' | '_' => Some(c),
            c if c.is_alphanumeric() => Some(c),
            _ => None,
        })
        .flat_map(char::to_lowercase)
        .collect()
}

/// GitHub suffixes repeated anchors with `-1`, `-2`, and so on.
fn unique_slug(slug: &str, seen: &[String]) -> String {
    let taken = |candidate: &str| seen.iter().any(|s| s == candidate);
    if !taken(slug) {
        return slug.to_owned();
    }
    (1..)
        .map(|n| format!("{slug}-{n}"))
        .find(|candidate| !taken(candidate))
        .unwrap_or_default()
}

/// Render Markdown for a terminal `width` columns wide, with ANSI styles and
/// OSC 8 hyperlinks.
pub fn render(markdown: &str, width: usize) -> String {
    let mut renderer = Renderer::new(width.min(MAX_WIDTH));
    for event in Parser::new_ext(markdown, options()) {
        renderer.event(event);
    }
    renderer.finish()
}

const CODE: Style = Style::new().fg(Color::Yellow);
const DIM: Style = Style::new().dim();

/// A run of inline text in one style.
#[derive(Clone)]
struct Span {
    text: String,
    style: Style,
    /// The URL an OSC 8 hyperlink opens.
    link: Option<String>,
}

/// One level of block nesting, which prefixes every line inside it.
struct Indent {
    /// The prefix for the next line, such as a list marker.
    first: Option<String>,
    rest: String,
}

/// A link being rendered: an external URL, or a README anchor shown as the
/// topic that `bit --docs` prints for it.
enum Link {
    Url(String),
    Topic(String),
}

struct Table {
    alignments: Vec<Alignment>,
    rows: Vec<Vec<Vec<Span>>>,
    row: Vec<Vec<Span>>,
}

struct Renderer {
    width: usize,
    out: String,
    /// Whether a blank line separates the next block from the previous one.
    blank_before: bool,
    spans: Vec<Span>,
    styles: Vec<Style>,
    indents: Vec<Indent>,
    lists: Vec<Option<u64>>,
    links: Vec<Link>,
    code_block: Option<String>,
    table: Option<Table>,
}

impl Renderer {
    fn new(width: usize) -> Self {
        Self {
            width,
            out: String::new(),
            blank_before: false,
            spans: Vec::new(),
            styles: vec![Style::new()],
            indents: Vec::new(),
            lists: Vec::new(),
            links: Vec::new(),
            code_block: None,
            table: None,
        }
    }

    fn finish(mut self) -> String {
        self.flush();
        self.out
    }

    fn style(&self) -> Style {
        self.styles.last().copied().unwrap_or_default()
    }

    fn push_style(&mut self, change: impl FnOnce(Style) -> Style) {
        self.styles.push(change(self.style()));
    }

    fn pop_style(&mut self) {
        if self.styles.len() > 1 {
            self.styles.pop();
        }
    }

    fn text(&mut self, text: &str, style: Style) {
        let link = match self.links.last() {
            Some(Link::Url(url)) => Some(url.clone()),
            _ => None,
        };
        self.spans.push(Span {
            text: text.to_owned(),
            style,
            link,
        });
    }

    fn event(&mut self, event: Event) {
        if let Some(code) = self.code_block.as_mut() {
            match event {
                Event::Text(text) => code.push_str(&text),
                Event::End(TagEnd::CodeBlock) => self.end_code_block(),
                _ => {}
            }
            return;
        }
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) | Event::InlineHtml(text) => self.text(&text, self.style()),
            Event::Code(text) => self.text(&text, self.style().fg(Color::Yellow)),
            Event::SoftBreak => self.text(" ", self.style()),
            Event::HardBreak => self.text("\n", self.style()),
            Event::Html(html) => {
                for line in html.lines() {
                    self.line(&line.paint(DIM).to_string());
                }
            }
            Event::Rule => {
                self.block_start();
                let width = self.width.saturating_sub(self.indent_width());
                self.line(&"─".repeat(width).paint(DIM).to_string());
                self.blank_before = true;
            }
            _ => {}
        }
    }

    fn start(&mut self, tag: Tag) {
        match tag {
            Tag::Paragraph => self.block_start(),
            Tag::Heading { level, .. } => {
                self.block_start();
                self.push_style(|_| heading_style(level));
            }
            Tag::BlockQuote(_) => {
                self.block_start();
                let bar = "│ ".paint(DIM).to_string();
                self.indents.push(Indent { first: None, rest: bar });
            }
            Tag::CodeBlock(_) => {
                self.block_start();
                self.code_block = Some(String::new());
            }
            Tag::HtmlBlock => self.block_start(),
            Tag::List(start) => {
                self.flush();
                if self.lists.is_empty() {
                    self.block_start();
                }
                self.lists.push(start);
            }
            Tag::Item => {
                self.flush();
                let depth = self.lists.len();
                let marker = match self.lists.last_mut() {
                    Some(Some(n)) => {
                        let marker = format!("{n}. ");
                        *n += 1;
                        marker
                    }
                    _ if depth.is_multiple_of(2) => "◦ ".to_owned(),
                    _ => "• ".to_owned(),
                };
                let width = console::measure_text_width(&marker);
                self.indents.push(Indent {
                    first: Some(marker),
                    rest: " ".repeat(width),
                });
            }
            Tag::Table(alignments) => {
                self.block_start();
                self.table = Some(Table {
                    alignments,
                    rows: Vec::new(),
                    row: Vec::new(),
                });
            }
            Tag::TableHead => self.push_style(Style::bold),
            Tag::Emphasis => self.push_style(Style::italic),
            Tag::Strong => self.push_style(Style::bold),
            Tag::Strikethrough => self.push_style(Style::strike),
            Tag::Link { dest_url, .. } => {
                let link = match dest_url.strip_prefix('#') {
                    Some(anchor) => Link::Topic(anchor.to_owned()),
                    None => Link::Url(dest_url.into_string()),
                };
                if matches!(link, Link::Url(_)) {
                    self.push_style(Style::underline);
                } else {
                    self.push_style(|style| style);
                }
                self.links.push(link);
            }
            _ => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph | TagEnd::HtmlBlock => self.block_end(),
            TagEnd::Heading(_) => {
                self.pop_style();
                self.block_end();
            }
            TagEnd::BlockQuote(_) => {
                self.block_end();
                self.indents.pop();
            }
            TagEnd::List(_) => {
                self.flush();
                self.lists.pop();
                if self.lists.is_empty() {
                    self.blank_before = true;
                }
            }
            TagEnd::Item => {
                self.flush();
                self.indents.pop();
            }
            TagEnd::TableCell => {
                let cell = std::mem::take(&mut self.spans);
                if let Some(table) = self.table.as_mut() {
                    table.row.push(cell);
                }
            }
            TagEnd::TableHead | TagEnd::TableRow => {
                if tag == TagEnd::TableHead {
                    self.pop_style();
                }
                if let Some(table) = self.table.as_mut() {
                    let row = std::mem::take(&mut table.row);
                    table.rows.push(row);
                }
            }
            TagEnd::Table => self.end_table(),
            TagEnd::Emphasis | TagEnd::Strong | TagEnd::Strikethrough => self.pop_style(),
            TagEnd::Link => {
                self.pop_style();
                if let Some(Link::Topic(topic)) = self.links.pop() {
                    self.text(&format!(" (bit --docs {topic})"), DIM);
                }
            }
            _ => {}
        }
    }

    /// Separate a new block from the one before it.
    fn block_start(&mut self) {
        self.flush();
        if self.blank_before && !self.out.is_empty() {
            self.out.push('\n');
        }
        self.blank_before = false;
    }

    fn block_end(&mut self) {
        self.flush();
        self.blank_before = true;
    }

    fn indent_width(&self) -> usize {
        self.indents.iter().map(|i| console::measure_text_width(&i.rest)).sum()
    }

    /// Write one line under the current indentation. Each list marker
    /// prefixes only the first line of its item.
    fn line(&mut self, content: &str) {
        for indent in &mut self.indents {
            let prefix = indent.first.take().unwrap_or_else(|| indent.rest.clone());
            self.out.push_str(&prefix);
        }
        self.out.push_str(content);
        self.out.push('\n');
    }

    /// Wrap and write the pending inline text.
    fn flush(&mut self) {
        if self.spans.is_empty() {
            return;
        }
        let spans = std::mem::take(&mut self.spans);
        let width = self.width.saturating_sub(self.indent_width()).max(10);
        for line in wrap(&spans, width) {
            self.line(&line);
        }
    }

    fn end_code_block(&mut self) {
        let code = self.code_block.take().unwrap_or_default();
        let gutter = "│ ".paint(DIM).to_string();
        for line in code.trim_end_matches('\n').lines() {
            self.line(&format!("{gutter}{}", line.paint(CODE)));
        }
        self.blank_before = true;
    }

    fn end_table(&mut self) {
        let Some(table) = self.table.take() else {
            return;
        };
        let plain = |cell: &[Span]| cell.iter().map(|s| s.text.as_str()).collect::<String>();
        let columns = table.rows.iter().map(Vec::len).max().unwrap_or(0);
        let mut widths = vec![0; columns];
        for row in &table.rows {
            for (width, cell) in widths.iter_mut().zip(row) {
                *width = (*width).max(console::measure_text_width(&plain(cell)));
            }
        }
        let separator = " │ ".paint(DIM).to_string();
        for (i, row) in table.rows.iter().enumerate() {
            let cells: Vec<String> = widths
                .iter()
                .enumerate()
                .map(|(column, &width)| {
                    let cell = row.get(column).map(Vec::as_slice).unwrap_or_default();
                    let padding = width - console::measure_text_width(&plain(cell));
                    let painted: String = cell.iter().map(paint_span).collect();
                    let (left, right) = match table.alignments.get(column) {
                        Some(Alignment::Right) => (padding, 0),
                        Some(Alignment::Center) => (padding / 2, padding - padding / 2),
                        _ => (0, padding),
                    };
                    format!("{}{painted}{}", " ".repeat(left), " ".repeat(right))
                })
                .collect();
            self.line(cells.join(&separator).trim_end());
            if i == 0 {
                let rule: Vec<String> = widths.iter().map(|&w| "─".repeat(w)).collect();
                self.line(&rule.join("─┼─").paint(DIM).to_string());
            }
        }
        self.blank_before = true;
    }
}

fn heading_style(level: HeadingLevel) -> Style {
    match level {
        HeadingLevel::H1 => Style::new().fg(Color::Magenta).bold().underline(),
        HeadingLevel::H2 => Style::new().fg(Color::Cyan).bold().underline(),
        HeadingLevel::H3 => Style::new().fg(Color::Cyan).bold(),
        _ => Style::new().fg(Color::Cyan),
    }
}

/// Paint a span, wrapping it in an OSC 8 hyperlink when it links somewhere.
fn paint_span(span: &Span) -> String {
    let painted = span.text.as_str().paint(span.style).to_string();
    match &span.link {
        Some(url) => format!("\x1b]8;;{url}\x1b\\{painted}\x1b]8;;\x1b\\"),
        None => painted,
    }
}

/// A word made of pieces of one or more spans, such as a code span followed
/// by a comma.
struct Word {
    pieces: Vec<Span>,
    width: usize,
}

/// Break inline text into lines at most `width` columns wide. A word wider
/// than the line gets a line of its own.
fn wrap(spans: &[Span], width: usize) -> Vec<String> {
    let mut lines: Vec<Vec<Word>> = vec![Vec::new()];
    let mut word = Word {
        pieces: Vec::new(),
        width: 0,
    };
    let mut line_width = 0;

    let place = |word: &mut Word, lines: &mut Vec<Vec<Word>>, line_width: &mut usize| {
        if word.pieces.is_empty() {
            return;
        }
        let word = std::mem::replace(
            word,
            Word {
                pieces: Vec::new(),
                width: 0,
            },
        );
        let current = lines.last().map_or(0, Vec::len);
        if current > 0 && *line_width + 1 + word.width > width {
            lines.push(Vec::new());
            *line_width = 0;
        }
        let current = lines.last().map_or(0, Vec::len);
        *line_width += word.width + usize::from(current > 0);
        if let Some(line) = lines.last_mut() {
            line.push(word);
        }
    };

    for span in spans {
        let mut rest = span.text.as_str();
        while !rest.is_empty() {
            let end = rest.find([' ', '\n']).unwrap_or(rest.len());
            if end > 0 {
                let text = &rest[..end];
                word.width += console::measure_text_width(text);
                word.pieces.push(Span {
                    text: text.to_owned(),
                    ..span.clone()
                });
                rest = &rest[end..];
                continue;
            }
            place(&mut word, &mut lines, &mut line_width);
            if rest.starts_with('\n') {
                lines.push(Vec::new());
                line_width = 0;
            }
            rest = &rest[1..];
        }
    }
    place(&mut word, &mut lines, &mut line_width);

    lines
        .into_iter()
        .filter(|line| !line.is_empty())
        .map(|line| {
            line.iter()
                .map(|word| word.pieces.iter().map(paint_span).collect::<String>())
                .collect::<Vec<_>>()
                .join(" ")
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "\
# Tool

Intro.

## Install bit

Run it.

## CI workflows

See [large](#blocks).

### Templates

Jinja.

## Blocks

Blocks text.

## CI caching

Cache.
";

    #[test]
    fn no_topic_selects_the_whole_document() {
        assert_eq!(Docs::new(DOC).select(None).unwrap(), DOC);
    }

    #[test]
    fn topic_matches_heading_text_or_slug_case_insensitively() {
        let docs = Docs::new(DOC);
        let section = "## Blocks\n\nBlocks text.\n\n";
        assert_eq!(docs.select(Some("blocks")).unwrap(), section);
        assert_eq!(docs.select(Some("BLOCKS")).unwrap(), section);
        assert_eq!(
            docs.select(Some("CI Workflows")).unwrap(),
            docs.select(Some("ci-workflows")).unwrap()
        );
    }

    #[test]
    fn section_includes_subsections_up_to_the_next_sibling() {
        let section = Docs::new(DOC).select(Some("ci-workflows")).unwrap();
        assert!(section.starts_with("## CI workflows\n"));
        assert!(section.contains("### Templates\n\nJinja."));
        assert!(!section.contains("## Blocks"));
    }

    #[test]
    fn topic_matches_a_unique_leading_word() {
        assert!(
            Docs::new(DOC)
                .select(Some("install"))
                .unwrap()
                .starts_with("## Install bit")
        );
        assert!(
            Docs::new(DOC)
                .select(Some("Install BIT"))
                .unwrap()
                .starts_with("## Install bit")
        );
    }

    #[test]
    fn ambiguous_prefix_lists_its_matches() {
        let error = Docs::new(DOC).select(Some("ci")).unwrap_err();
        assert_eq!(
            error,
            DocsError::Ambiguous {
                topic: "ci".to_owned(),
                matches: vec!["ci-workflows".to_owned(), "ci-caching".to_owned()],
            }
        );
    }

    #[test]
    fn unknown_topic_lists_every_topic() {
        let error = Docs::new(DOC).select(Some("nope")).unwrap_err();
        assert_eq!(
            error.to_string(),
            "unknown docs topic 'nope'; available topics:\n  install-bit\n  ci-workflows\n  templates\n  blocks\n  ci-caching"
        );
    }

    #[test]
    fn readme_ci_topic_is_the_ci_workflows_section() {
        let section = Docs::new(README).select(Some("ci")).unwrap();
        assert!(section.starts_with("## CI workflows\n"), "{section}");
        assert!(section.contains("A template sees:"));
        assert!(!section.contains("## Providers"));
    }

    #[test]
    fn readme_anchor_links_name_existing_topics() {
        let topics = Docs::new(README).topics();
        for (event, _) in Parser::new_ext(README, options()).into_offset_iter() {
            if let Event::Start(Tag::Link { dest_url, .. }) = event
                && let Some(anchor) = dest_url.strip_prefix('#')
            {
                assert!(topics.iter().any(|t| t == anchor), "no topic for #{anchor}");
            }
        }
    }

    #[test]
    fn slug_follows_github_anchors() {
        assert_eq!(
            slugify("Recursive resolution and BUILD.bit.lock"),
            "recursive-resolution-and-buildbitlock"
        );
        assert_eq!(slugify("bit — Build It"), "bit--build-it");
        assert_eq!(unique_slug("go", &["go".to_owned()]), "go-1");
    }

    #[test]
    fn renders_headings_and_inline_styles() {
        let out = render("## Title\n\nSome **bold**, _italic_ and `code`.\n", 80);
        assert_eq!(
            out,
            "\x1b[1;4;36mTitle\x1b[0m\n\
             \n\
             Some \x1b[1mbold\x1b[0m, \x1b[3mitalic\x1b[0m and \x1b[33mcode\x1b[0m.\n"
        );
    }

    #[test]
    fn renders_links_as_osc8_hyperlinks() {
        let out = render("See [docs](https://example.com).\n", 80);
        assert_eq!(
            out,
            "See \x1b]8;;https://example.com\x1b\\\x1b[4mdocs\x1b[0m\x1b]8;;\x1b\\.\n"
        );
    }

    #[test]
    fn renders_anchor_links_with_their_topic() {
        let out = render("Mark it [`large`](#blocks).\n", 80);
        assert_eq!(
            out,
            "Mark it \x1b[33mlarge\x1b[0m \x1b[2m(bit\x1b[0m \x1b[2m--docs\x1b[0m \x1b[2mblocks)\x1b[0m.\n"
        );
    }

    #[test]
    fn renders_lists_with_markers_and_hanging_indent() {
        let out = render(
            "- one two three four\n  - nested\n- **two**\n\n1. first\n2. second\n",
            12,
        );
        assert_eq!(
            out,
            "• one two\n  three four\n  ◦ nested\n• \x1b[1mtwo\x1b[0m\n\n1. first\n2. second\n"
        );
    }

    #[test]
    fn renders_code_blocks_with_a_gutter() {
        let out = render("```sh\nbit --docs\nbit --docs ci\n```\n", 80);
        assert_eq!(
            out,
            "\x1b[2m│ \x1b[0m\x1b[33mbit --docs\x1b[0m\n\x1b[2m│ \x1b[0m\x1b[33mbit --docs ci\x1b[0m\n"
        );
    }

    #[test]
    fn renders_tables_with_aligned_columns() {
        let out = render(
            "| Name | Use |\n| --- | --- |\n| `jobs` | The jobs |\n| id | An id |\n",
            80,
        );
        assert_eq!(
            out,
            "\x1b[1mName\x1b[0m\x1b[2m │ \x1b[0m\x1b[1mUse\x1b[0m\n\
             \x1b[2m─────┼─────────\x1b[0m\n\
             \x1b[33mjobs\x1b[0m\x1b[2m │ \x1b[0mThe jobs\n\
             id  \x1b[2m │ \x1b[0mAn id\n"
        );
    }

    #[test]
    fn wraps_paragraphs_to_the_width() {
        let out = render("alpha beta gamma delta epsilon zeta eta theta iota kappa\n", 20);
        assert_eq!(out, "alpha beta gamma\ndelta epsilon zeta\neta theta iota kappa\n");
    }
}
