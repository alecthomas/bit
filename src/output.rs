use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{self, BufRead, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use yansi::{Color, Paint, Painted};

/// Emit a debug message via an `Output` or `BlockWriter`, formatting the
/// message only when debug is enabled on the target. Mirrors `log::debug!` /
/// `tracing::debug!` so argument expressions and the `format!` allocation
/// are skipped entirely on the common (debug-off) path.
///
/// The target must expose `debug_enabled()` and `debug(&str)` methods —
/// both [`Output`] and [`BlockWriter`] do.
#[macro_export]
macro_rules! debug {
    ($target:expr, $($arg:tt)*) => {{
        // Bind once so callers may pass method-call expressions (e.g.
        // `output.writer(name)`) without evaluating them twice.
        let __bit_debug_target = &$target;
        if __bit_debug_target.debug_enabled() {
            __bit_debug_target.debug(&format!($($arg)*));
        }
    }};
}

/// Extension trait for conditionally dimming styled output.
trait DimIf {
    fn dim_if(self, condition: bool) -> Self;
}

impl<T> DimIf for Painted<T> {
    fn dim_if(self, condition: bool) -> Self {
        if condition { self.dim() } else { self }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Starting,
    Skipped,
    Ok,
    Failed,
    Create,
    Update,
    Replace,
    Destroy,
    NoChange,
    Debug,
}

impl Event {
    fn symbol(&self) -> &'static str {
        match self {
            Event::Starting => "▶",
            Event::Skipped => "·",
            Event::Ok => "✔",
            Event::Failed => "✘",
            Event::Create => "+",
            Event::Update => "~",
            Event::Replace => "!",
            Event::Destroy => "-",
            Event::NoChange => "·",
            Event::Debug => "⚙",
        }
    }

    fn is_dim(&self) -> bool {
        matches!(self, Event::Skipped | Event::NoChange | Event::Debug)
    }

    pub fn color(&self) -> Color {
        match self {
            Event::Starting => Color::Cyan,
            Event::Skipped => Color::Primary,
            Event::Ok => Color::Green,
            Event::Failed => Color::Red,
            Event::Create => Color::Green,
            Event::Update => Color::Yellow,
            Event::Replace => Color::Magenta,
            Event::Destroy => Color::Red,
            Event::NoChange => Color::Primary,
            Event::Debug => Color::Blue,
        }
    }
}

/// Selects a color from the 256-color palette based on a hash of the name.
fn color_for_name(name: &str) -> Color {
    let mut hasher = DefaultHasher::new();
    name.hash(&mut hasher);
    let hash = hasher.finish();

    let usable_colors: Vec<u8> = (17u8..=231)
        .filter(|&c| {
            let idx = c - 16;
            let r = idx / 36;
            let g = (idx % 36) / 6;
            let b = idx % 6;
            let sum = r + g + b;
            if !(4..=11).contains(&sum) {
                return false;
            }
            if r >= 3 && r > g && r > b {
                return false;
            }
            if b >= 3 && g <= 1 && r <= 1 {
                return false;
            }
            true
        })
        .collect();

    let idx = (hash as usize) % usable_colors.len();
    Color::Fixed(usable_colors[idx])
}

/// Thread-safe output formatter. All output goes through this to keep
/// block prefixes aligned and interleaved output readable.
#[derive(Clone)]
pub struct Output {
    inner: Arc<Mutex<OutputInner>>,
    /// Whether `debug()` calls should actually emit output.
    debug: bool,
    /// Reference timestamp used to compute `+elapsed` prefixes on debug lines.
    start: Instant,
}

struct OutputInner {
    max_name_len: usize,
}

/// Format a `Duration` as a compact relative offset for debug output:
/// sub-second values are printed in ms (`+12ms`), sub-minute in seconds with
/// one decimal (`+1.2s`), and longer durations as `+1m23s`.
fn format_elapsed(d: Duration) -> String {
    let ms = d.as_millis();
    if ms < 1000 {
        format!("+{ms}ms")
    } else if ms < 60_000 {
        format!("+{:.1}s", d.as_secs_f64())
    } else {
        let secs = d.as_secs();
        format!("+{}m{}s", secs / 60, secs % 60)
    }
}

impl Output {
    pub fn new(block_names: &[&str]) -> Self {
        let max_name_len = block_names.iter().map(|n| n.len()).max().unwrap_or(0);
        Self {
            inner: Arc::new(Mutex::new(OutputInner { max_name_len })),
            debug: false,
            start: Instant::now(),
        }
    }

    /// Enable or disable debug output. Builder-style: returns the updated value.
    pub fn with_debug(mut self, debug: bool) -> Self {
        self.debug = debug;
        self
    }

    /// Whether debug output is enabled.
    pub fn debug_enabled(&self) -> bool {
        self.debug
    }

    /// Emit an engine-level debug message, padded to align with block-prefixed
    /// output so it lines up visually with per-block messages. Each line is
    /// prefixed with the elapsed time since this `Output` was constructed.
    /// No-op when debug is disabled.
    pub fn debug(&self, message: &str) {
        if !self.debug {
            return;
        }
        let stamped = format!("{} {message}", format_elapsed(self.start.elapsed()));
        // Passing an empty name produces a prefix of `<spaces> <symbol>` with
        // the same width as real block prefixes (right-aligned padding).
        self.print_event("", Event::Debug, None, &stamped);
    }

    /// Create a writer for a specific block.
    pub fn writer(&self, name: &str) -> BlockWriter {
        BlockWriter {
            output: self.clone(),
            name: name.to_owned(),
            color: color_for_name(name),
            indent: None,
        }
    }

    /// Create a writer for a specific block with left-aligned indentation.
    pub fn writer_indented(&self, name: &str, indent: usize) -> BlockWriter {
        BlockWriter {
            output: self.clone(),
            name: name.to_owned(),
            color: color_for_name(name),
            indent: Some(indent),
        }
    }

    fn prefix(&self, name: &str, sep: &str, indent: Option<usize>) -> String {
        match indent {
            Some(n) => {
                let pad = "  ".repeat(n);
                format!("{pad}{name} {sep}")
            }
            None => {
                let inner = self.inner.lock().unwrap();
                format!("{:>width$} {sep}", name, width = inner.max_name_len)
            }
        }
    }

    fn print_line(&self, name: &str, color: Color, indent: Option<usize>, content: &str) {
        let prefix = self.prefix(name, "│", indent);
        println!("{} {content}", prefix.paint(color));
    }

    fn print_stderr_line(&self, name: &str, color: Color, indent: Option<usize>, content: &str) {
        let prefix = self.prefix(name, "│", indent);
        println!("{} {}", prefix.paint(color), content.dim().italic());
    }

    fn print_event(&self, name: &str, event: Event, indent: Option<usize>, message: &str) {
        let color = color_for_name(name);
        let prefix = self.prefix(name, event.symbol(), indent);
        let dim = event.is_dim();
        if message.is_empty() {
            let text = format!("{event:?}").to_lowercase();
            println!(
                "{} {}",
                prefix.paint(color).dim_if(dim),
                text.paint(event.color()).dim_if(dim)
            );
        } else {
            let mut lines = message.lines();
            if let Some(first) = lines.next() {
                println!(
                    "{} {}",
                    prefix.paint(color).dim_if(dim),
                    first.paint(event.color()).dim_if(dim)
                );
            }
            let cont_prefix = self.prefix(name, "┆", indent);
            for line in lines {
                println!(
                    "{} {}",
                    cont_prefix.paint(color).dim_if(dim),
                    line.paint(event.color()).dim_if(dim)
                );
            }
        }
    }

    /// Like `print_event` but the message is pre-formatted and not re-painted.
    fn print_event_raw(&self, name: &str, event: Event, indent: Option<usize>, message: &str) {
        let color = color_for_name(name);
        let dim = event.is_dim();
        let prefix = self.prefix(name, event.symbol(), indent);
        let mut lines = message.lines();
        if let Some(first) = lines.next() {
            println!("{} {first}", prefix.paint(color).dim_if(dim));
        }
        let cont_prefix = self.prefix(name, "┆", indent);
        for line in lines {
            println!("{} {line}", cont_prefix.paint(color).dim_if(dim));
        }
    }
}

/// A writer bound to a specific block, used by providers to emit output.
pub struct BlockWriter {
    output: Output,
    name: String,
    color: Color,
    /// `None` = right-aligned (apply mode), `Some(n)` = left-aligned with indent (plan mode).
    indent: Option<usize>,
}

impl BlockWriter {
    pub fn event(&self, event: Event, message: &str) {
        self.output.print_event(&self.name, event, self.indent, message);
    }

    /// Whether debug output is enabled for the underlying Output.
    /// Prefer the `debug!` macro over calling this directly.
    pub fn debug_enabled(&self) -> bool {
        self.output.debug
    }

    /// Emit a block-scoped debug message. Each line is prefixed with the
    /// elapsed time since the `Output` was constructed. No-op when debug is
    /// disabled. Prefer the `debug!` macro to avoid formatting when disabled.
    pub fn debug(&self, message: &str) {
        if !self.output.debug {
            return;
        }
        let stamped = format!("{} {message}", format_elapsed(self.output.start.elapsed()));
        self.output.print_event(&self.name, Event::Debug, self.indent, &stamped);
    }

    /// Like `event`, but the message is pre-formatted with ANSI codes
    /// and will not be re-painted by the output layer.
    pub fn event_raw(&self, event: Event, message: &str) {
        self.output.print_event_raw(&self.name, event, self.indent, message);
    }

    pub fn line(&self, content: &str) {
        self.output.print_line(&self.name, self.color, self.indent, content);
    }

    pub fn stderr_line(&self, content: &str) {
        self.output
            .print_stderr_line(&self.name, self.color, self.indent, content);
    }

    /// Write all lines from a reader, prefixed with the block name.
    pub fn pipe_stdout(&self, reader: impl BufRead) {
        for line in reader.lines() {
            match line {
                Ok(l) => self.line(&l),
                Err(_) => break,
            }
        }
    }

    /// Emit a passing test suite summary.
    pub fn test_suite_passed(&self, suite: &str, duration: std::time::Duration, passed: usize, skipped: usize) {
        let ms = duration.as_millis();
        let detail = if skipped > 0 {
            format!("({ms}ms, {passed} passed, {skipped} skipped)")
        } else {
            format!("({ms}ms, {passed} passed)")
        };
        let msg = format!(
            "{} {} {}",
            "PASS".paint(Color::Green).bold(),
            suite.bold(),
            detail.dim()
        );
        self.output.print_event_raw(&self.name, Event::Ok, self.indent, &msg);
    }

    /// Emit a failing test suite summary with individual failures.
    pub fn test_suite_failed(
        &self,
        suite: &str,
        duration: std::time::Duration,
        passed: usize,
        failed: usize,
        failures: &[(String, std::time::Duration, String)],
    ) {
        let ms = duration.as_millis();
        let mut msg = format!(
            "{} {} {}",
            "FAIL".paint(Color::Red).bold(),
            suite.bold(),
            format!("({ms}ms, {passed} passed, {failed} failed)").dim(),
        );
        for (name, dur, output) in failures {
            let fms = dur.as_millis();
            msg.push_str(&format!(
                "\n  {} {}",
                name.paint(Color::Red),
                format!("({fms}ms)").dim()
            ));
            for line in output.lines() {
                msg.push_str(&format!("\n    {}", line.dim()));
            }
        }
        self.output
            .print_event_raw(&self.name, Event::Failed, self.indent, &msg);
    }

    /// Emit a skipped test suite.
    pub fn test_suite_skipped(&self, suite: &str) {
        let msg = format!("{} {}", "SKIP".paint(Color::Yellow).bold(), suite.dim());
        self.output
            .print_event_raw(&self.name, Event::Skipped, self.indent, &msg);
    }

    /// Emit a single passing test (verbose mode).
    pub fn test_passed(&self, suite: &str, name: &str, duration: std::time::Duration) {
        let ms = duration.as_millis();
        let msg = format!(
            "{} {} {}",
            "PASS".paint(Color::Green).bold(),
            format!("{suite}/{name}").bold(),
            format!("({ms}ms)").dim(),
        );
        self.output.print_event_raw(&self.name, Event::Ok, self.indent, &msg);
    }

    /// Emit a single failing test with output (verbose mode).
    pub fn test_failed(&self, suite: &str, name: &str, duration: std::time::Duration, output: &str) {
        let ms = duration.as_millis();
        let mut msg = format!(
            "{} {} {}",
            "FAIL".paint(Color::Red).bold(),
            format!("{suite}/{name}").bold(),
            format!("({ms}ms)").dim(),
        );
        for line in output.lines() {
            msg.push_str(&format!("\n  {}", line.dim()));
        }
        self.output
            .print_event_raw(&self.name, Event::Failed, self.indent, &msg);
    }

    /// Emit a single skipped test (verbose mode).
    pub fn test_skipped(&self, suite: &str, name: &str) {
        let msg = format!(
            "{} {}",
            "SKIP".paint(Color::Yellow).bold(),
            format!("{suite}/{name}").dim(),
        );
        self.output
            .print_event_raw(&self.name, Event::Skipped, self.indent, &msg);
    }

    /// Write all lines from a reader as stderr output.
    pub fn pipe_stderr(&self, reader: impl BufRead) {
        for line in reader.lines() {
            match line {
                Ok(l) => self.stderr_line(&l),
                Err(_) => break,
            }
        }
    }
}

/// An `io::Write` adapter that line-buffers and prefixes output.
pub struct BlockWriteAdapter {
    writer: BlockWriter,
    stderr: bool,
    buf: Vec<u8>,
}

impl BlockWriteAdapter {
    pub fn stdout(writer: BlockWriter) -> Self {
        Self {
            writer,
            stderr: false,
            buf: Vec::new(),
        }
    }

    pub fn stderr(writer: BlockWriter) -> Self {
        Self {
            writer,
            stderr: true,
            buf: Vec::new(),
        }
    }

    fn flush_lines(&mut self) {
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line = String::from_utf8_lossy(&self.buf[..pos]).into_owned();
            if self.stderr {
                self.writer.stderr_line(&line);
            } else {
                self.writer.line(&line);
            }
            self.buf.drain(..=pos);
        }
    }
}

impl Write for BlockWriteAdapter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(buf);
        self.flush_lines();
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        if !self.buf.is_empty() {
            let line = String::from_utf8_lossy(&self.buf).into_owned();
            if self.stderr {
                self.writer.stderr_line(&line);
            } else {
                self.writer.line(&line);
            }
            Vec::clear(&mut self.buf);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_deterministic() {
        let c1 = color_for_name("api");
        let c2 = color_for_name("api");
        assert_eq!(c1, c2);
    }

    #[test]
    fn color_different_names() {
        let c1 = color_for_name("api");
        let c2 = color_for_name("worker");
        assert_ne!(c1, c2);
    }
}
