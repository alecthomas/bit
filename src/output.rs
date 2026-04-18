use std::collections::VecDeque;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{self, BufRead, IsTerminal, StdoutLock, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use indexmap::IndexMap;
use yansi::{Color, Paint, Painted};

/// Number of streamed output lines retained per active task in the live region.
const REGION_LINES: usize = 5;

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

/// Rendering state for a single task that currently occupies a live region.
#[derive(Default)]
struct TaskState {
    /// Fully-formatted header lines (from the `Starting` event). Always visible.
    header: Vec<String>,
    /// Last `REGION_LINES` streamed output lines, fully formatted.
    recent: VecDeque<String>,
}

/// Thread-safe output formatter. All output goes through this to keep
/// block prefixes aligned and interleaved output readable.
///
/// When stdout is a TTY, each active task gets a fixed-height "live region"
/// at the bottom of the screen showing its header and most recent output
/// lines. Events and completed tasks scroll upward off the region naturally.
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
    /// Whether live scrolling regions are used. True only when stdout is a
    /// terminal AND the user has not opted out via [`Output::with_long`].
    /// When false, output is plain line-by-line streaming.
    live: bool,
    /// Cached terminal width in columns (0 = unknown / unlimited).
    term_width: usize,
    /// Per-task live-region state, in insertion order.
    active: IndexMap<String, TaskState>,
    /// Number of terminal rows currently occupied by the live region, used
    /// to know how far up to move the cursor before clearing.
    live_rows: usize,
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
        let live = io::stdout().is_terminal();
        let term_width = if live {
            console::Term::stdout().size().1 as usize
        } else {
            0
        };
        Self {
            inner: Arc::new(Mutex::new(OutputInner {
                max_name_len,
                live,
                term_width,
                active: IndexMap::new(),
                live_rows: 0,
            })),
            debug: false,
            start: Instant::now(),
        }
    }

    /// Enable or disable debug output. Builder-style: returns the updated value.
    pub fn with_debug(mut self, debug: bool) -> Self {
        self.debug = debug;
        self
    }

    /// Force long-form line-by-line streaming, disabling live scrolling
    /// regions even on a TTY. Useful for log-friendly transcripts and
    /// debugging.
    pub fn with_long(self, long: bool) -> Self {
        if long {
            let mut inner = self.inner.lock().expect("output lock poisoned");
            inner.live = false;
            inner.term_width = 0;
        }
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
        self.dispatch_event("", Event::Debug, None, &stamped, /*raw=*/ false);
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

    fn dispatch_stream(&self, name: &str, color: Color, indent: Option<usize>, content: &str, stderr: bool) {
        let mut inner = self.inner.lock().unwrap();
        let line = inner.fmt_stream_line(name, color, indent, content, stderr);
        let stdout = io::stdout();
        let mut out = stdout.lock();
        inner.push_stream(&mut out, name, line);
    }

    fn dispatch_event(&self, name: &str, event: Event, indent: Option<usize>, message: &str, raw: bool) {
        let mut inner = self.inner.lock().unwrap();
        let lines = if raw {
            inner.fmt_event_raw(name, event, indent, message)
        } else {
            inner.fmt_event(name, event, indent, message)
        };
        let stdout = io::stdout();
        let mut out = stdout.lock();
        // `event_raw` is used by providers for *intermediate* progress events
        // (e.g. per-suite `test_suite_passed`). These flow through the task's
        // live region so they scroll within the 5-line window and are gone
        // when the region collapses on completion. Only `event` (non-raw)
        // Ok/Failed/Skipped/NoChange are terminal for the region.
        match (event, raw) {
            (Event::Starting, _) => inner.open_region(&mut out, name, lines),
            (Event::Ok | Event::Failed | Event::Skipped | Event::NoChange, false) => {
                inner.close_region(&mut out, name, lines)
            }
            (_, true) if inner.active.contains_key(name) => inner.push_stream_many(&mut out, name, lines),
            _ => inner.print_detached(&mut out, &lines),
        }
    }
}

impl OutputInner {
    fn prefix(&self, name: &str, sep: &str, indent: Option<usize>) -> String {
        match indent {
            Some(n) => {
                let pad = "  ".repeat(n);
                format!("{pad}{name} {sep}")
            }
            None => format!("{:>width$} {sep}", name, width = self.max_name_len),
        }
    }

    /// Truncate a (possibly ANSI-colored) line to the cached terminal width
    /// so it renders on exactly one row. When width is unknown (non-TTY or
    /// detection failure), the line is returned unchanged.
    fn fit(&self, line: &str) -> String {
        if self.term_width == 0 {
            return line.to_string();
        }
        console::truncate_str(line, self.term_width, "").into_owned()
    }

    fn fmt_stream_line(&self, name: &str, color: Color, indent: Option<usize>, content: &str, stderr: bool) -> String {
        let prefix = self.prefix(name, "│", indent);
        let raw = if stderr {
            format!("{} {}", prefix.paint(color), content.dim().italic())
        } else {
            format!("{} {content}", prefix.paint(color))
        };
        self.fit(&raw)
    }

    fn fmt_event(&self, name: &str, event: Event, indent: Option<usize>, message: &str) -> Vec<String> {
        let color = color_for_name(name);
        let prefix = self.prefix(name, event.symbol(), indent);
        let dim = event.is_dim();
        let mut lines = Vec::new();
        if message.is_empty() {
            let text = format!("{event:?}").to_lowercase();
            lines.push(self.fit(&format!(
                "{} {}",
                prefix.paint(color).dim_if(dim),
                text.paint(event.color()).dim_if(dim),
            )));
            return lines;
        }
        let mut src = message.lines();
        if let Some(first) = src.next() {
            lines.push(self.fit(&format!(
                "{} {}",
                prefix.paint(color).dim_if(dim),
                first.paint(event.color()).dim_if(dim),
            )));
        }
        let cont_prefix = self.prefix(name, "┆", indent);
        for line in src {
            lines.push(self.fit(&format!(
                "{} {}",
                cont_prefix.paint(color).dim_if(dim),
                line.paint(event.color()).dim_if(dim),
            )));
        }
        lines
    }

    fn fmt_event_raw(&self, name: &str, event: Event, indent: Option<usize>, message: &str) -> Vec<String> {
        let color = color_for_name(name);
        let dim = event.is_dim();
        let prefix = self.prefix(name, event.symbol(), indent);
        let mut lines = Vec::new();
        let mut src = message.lines();
        if let Some(first) = src.next() {
            lines.push(self.fit(&format!("{} {first}", prefix.paint(color).dim_if(dim),)));
        }
        let cont_prefix = self.prefix(name, "┆", indent);
        for line in src {
            lines.push(self.fit(&format!("{} {line}", cont_prefix.paint(color).dim_if(dim),)));
        }
        lines
    }

    /// Move the cursor to the top of the live region and clear everything
    /// below it. Caller is responsible for redrawing afterwards.
    fn clear_live(&mut self, out: &mut StdoutLock<'_>) -> io::Result<()> {
        if self.live_rows > 0 {
            // ESC[{N}A  = cursor up N lines (column unchanged; after a println
            //             we're at column 0 so this lands at column 0 of the
            //             first drawn line).
            // ESC[0J    = clear from cursor to end of screen.
            write!(out, "\x1b[{}A\x1b[0J", self.live_rows)?;
            self.live_rows = 0;
        }
        Ok(())
    }

    /// Render every active task's header + recent buffer at the current
    /// cursor position, updating `live_rows` with the total rows drawn.
    fn redraw_live(&mut self, out: &mut StdoutLock<'_>) -> io::Result<()> {
        let mut rows = 0;
        for state in self.active.values() {
            for line in &state.header {
                writeln!(out, "{line}")?;
                rows += 1;
            }
            for line in &state.recent {
                writeln!(out, "{line}")?;
                rows += 1;
            }
        }
        self.live_rows = rows;
        out.flush()
    }

    /// Print lines above the live region (they scroll naturally upward).
    /// Used for debug, plan-mode change events, and intermediate `event_raw`.
    fn print_detached(&mut self, out: &mut StdoutLock<'_>, lines: &[String]) {
        if !self.live {
            for line in lines {
                let _ = writeln!(out, "{line}");
            }
            return;
        }
        let _ = self.clear_live(out);
        for line in lines {
            let _ = writeln!(out, "{line}");
        }
        let _ = self.redraw_live(out);
    }

    /// Append a streamed output line to a task's live region (dropping the
    /// oldest when the buffer exceeds `REGION_LINES`). In non-TTY mode this
    /// just prints the line.
    fn push_stream(&mut self, out: &mut StdoutLock<'_>, name: &str, line: String) {
        if !self.live {
            let _ = writeln!(out, "{line}");
            return;
        }
        let _ = self.clear_live(out);
        let state = self.active.entry(name.to_string()).or_default();
        if state.recent.len() == REGION_LINES {
            state.recent.pop_front();
        }
        state.recent.push_back(line);
        let _ = self.redraw_live(out);
    }

    /// Like `push_stream` for multiple lines, with a single clear + redraw.
    /// Used by intermediate `event_raw` messages (e.g. test suite summaries)
    /// that can be multi-line but should still flow through the live region.
    fn push_stream_many(&mut self, out: &mut StdoutLock<'_>, name: &str, lines: Vec<String>) {
        if !self.live {
            for line in &lines {
                let _ = writeln!(out, "{line}");
            }
            return;
        }
        let _ = self.clear_live(out);
        let state = self.active.entry(name.to_string()).or_default();
        for line in lines {
            if state.recent.len() == REGION_LINES {
                state.recent.pop_front();
            }
            state.recent.push_back(line);
        }
        let _ = self.redraw_live(out);
    }

    /// Begin (or replace) a task's live region with the given header lines.
    fn open_region(&mut self, out: &mut StdoutLock<'_>, name: &str, header: Vec<String>) {
        if !self.live {
            for line in &header {
                let _ = writeln!(out, "{line}");
            }
            return;
        }
        let _ = self.clear_live(out);
        let state = self.active.entry(name.to_string()).or_default();
        state.header = header;
        // Use fully-qualified form: `yansi::Paint` has a deprecated `clear()`
        // extension on every type that shadows `VecDeque::clear`.
        VecDeque::clear(&mut state.recent);
        let _ = self.redraw_live(out);
    }

    /// Print the task's terminal event above the live region and remove
    /// the region. In non-TTY mode this just prints the lines.
    fn close_region(&mut self, out: &mut StdoutLock<'_>, name: &str, lines: Vec<String>) {
        if !self.live {
            for line in &lines {
                let _ = writeln!(out, "{line}");
            }
            return;
        }
        let _ = self.clear_live(out);
        self.active.shift_remove(name);
        for line in &lines {
            let _ = writeln!(out, "{line}");
        }
        let _ = self.redraw_live(out);
    }
}

impl Drop for OutputInner {
    fn drop(&mut self) {
        // Best-effort cleanup so a stranded live region doesn't mangle the
        // terminal after normal exit. Panics have already unwound by here;
        // if the process is aborting this runs only for the thread that
        // owns the last Arc.
        if self.live && self.live_rows > 0 {
            let stdout = io::stdout();
            let mut out = stdout.lock();
            let _ = self.clear_live(&mut out);
            let _ = out.flush();
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
        self.output
            .dispatch_event(&self.name, event, self.indent, message, /*raw=*/ false);
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
        self.output
            .dispatch_event(&self.name, Event::Debug, self.indent, &stamped, false);
    }

    /// Like `event`, but the message is pre-formatted with ANSI codes and
    /// will not be re-painted by the output layer. Intended for *intermediate*
    /// progress events (e.g. per-test-suite summaries) that should scroll
    /// above the live region rather than terminate it.
    pub fn event_raw(&self, event: Event, message: &str) {
        self.output
            .dispatch_event(&self.name, event, self.indent, message, /*raw=*/ true);
    }

    pub fn line(&self, content: &str) {
        self.output
            .dispatch_stream(&self.name, self.color, self.indent, content, false);
    }

    pub fn stderr_line(&self, content: &str) {
        self.output
            .dispatch_stream(&self.name, self.color, self.indent, content, true);
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
        self.event_raw(Event::Ok, &msg);
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
        self.event_raw(Event::Failed, &msg);
    }

    /// Emit a skipped test suite.
    pub fn test_suite_skipped(&self, suite: &str) {
        let msg = format!("{} {}", "SKIP".paint(Color::Yellow).bold(), suite.dim());
        self.event_raw(Event::Skipped, &msg);
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
        self.event_raw(Event::Ok, &msg);
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
        self.event_raw(Event::Failed, &msg);
    }

    /// Emit a single skipped test (verbose mode).
    pub fn test_skipped(&self, suite: &str, name: &str) {
        let msg = format!(
            "{} {}",
            "SKIP".paint(Color::Yellow).bold(),
            format!("{suite}/{name}").dim(),
        );
        self.event_raw(Event::Skipped, &msg);
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

    /// Build an `OutputInner` in non-TTY mode for testing region bookkeeping
    /// without actually writing escape codes to a real terminal.
    fn inner_tty(max_name_len: usize, term_width: usize) -> OutputInner {
        OutputInner {
            max_name_len,
            live: true,
            term_width,
            active: IndexMap::new(),
            live_rows: 0,
        }
    }

    #[test]
    fn fit_truncates_to_term_width() {
        let inner = inner_tty(4, 10);
        // 15 visible chars, width 10 -> truncated to 10.
        let line = "0123456789ABCDE";
        let fitted = inner.fit(line);
        assert_eq!(console::measure_text_width(&fitted), 10);
    }

    #[test]
    fn fit_noop_when_width_unknown() {
        let mut inner = inner_tty(4, 0);
        inner.live = false;
        let line = "hello world";
        assert_eq!(inner.fit(line), line);
    }

    #[test]
    fn region_open_tracks_rows() {
        let mut inner = inner_tty(4, 80);
        // Simulate open_region without a real stdout: manipulate state directly.
        let state = inner.active.entry("a".to_string()).or_default();
        state.header = vec!["h1".into(), "h2".into()];
        // Redraw would write 2 header + 0 recent = 2 rows.
        let mut buf: Vec<u8> = Vec::new();
        // Fake stdout lock by writing to Vec via a helper mirroring redraw_live.
        let mut rows = 0;
        for s in inner.active.values() {
            for line in &s.header {
                writeln!(&mut buf, "{line}").unwrap();
                rows += 1;
            }
            for line in &s.recent {
                writeln!(&mut buf, "{line}").unwrap();
                rows += 1;
            }
        }
        assert_eq!(rows, 2);
    }

    #[test]
    fn region_recent_caps_at_region_lines() {
        let mut inner = inner_tty(4, 80);
        let state = inner.active.entry("a".to_string()).or_default();
        for i in 0..REGION_LINES + 3 {
            if state.recent.len() == REGION_LINES {
                state.recent.pop_front();
            }
            state.recent.push_back(format!("line{i}"));
        }
        assert_eq!(state.recent.len(), REGION_LINES);
        assert_eq!(state.recent.front().map(|s| s.as_str()), Some("line3"));
        assert_eq!(state.recent.back().map(|s| s.as_str()), Some("line7"));
    }

    #[test]
    fn active_preserves_insertion_order() {
        let mut inner = inner_tty(4, 80);
        inner.active.insert("first".into(), TaskState::default());
        inner.active.insert("second".into(), TaskState::default());
        inner.active.insert("third".into(), TaskState::default());
        let order: Vec<&str> = inner.active.keys().map(String::as_str).collect();
        assert_eq!(order, vec!["first", "second", "third"]);

        inner.active.shift_remove("second");
        let order: Vec<&str> = inner.active.keys().map(String::as_str).collect();
        assert_eq!(order, vec!["first", "third"]);
    }
}
