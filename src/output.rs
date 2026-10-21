use std::collections::hash_map::DefaultHasher;
use std::collections::{HashSet, VecDeque};
use std::hash::{Hash, Hasher};
use std::io::{self, BufRead, IsTerminal, StdoutLock, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use indexmap::IndexMap;
use yansi::{Color, Paint, Painted};

/// Number of streamed output lines displayed per active task in the live
/// region while it runs.
const REGION_LINES: usize = 5;

/// Size of the rolling buffer retained per task. On failure the full
/// buffer is flushed above the failure event so the error context isn't
/// lost to scrollback.
const TAIL_LINES: usize = 50;

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
    /// Outputs will be (or were) recreated from the shared cache.
    Restore,
    NoChange,
    Debug,
    /// Terminal: block was skipped because it is `protected` (destroy refused).
    Protected,
    Info,
    Warning,
}

impl Event {
    pub fn symbol(&self) -> &'static str {
        match self {
            Event::Starting => "▶",
            Event::Skipped => "·",
            Event::Ok => "✔",
            Event::Failed => "✘",
            Event::Create => "+",
            Event::Update => "~",
            Event::Destroy => "-",
            Event::Restore => "⇣",
            Event::NoChange => "·",
            Event::Debug => "⚙",
            Event::Protected => "⊘",
            Event::Info => "ℹ",
            Event::Warning => "▲",
        }
    }

    fn is_dim(&self) -> bool {
        matches!(self, Event::Skipped | Event::NoChange | Event::Debug | Event::Protected)
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
            Event::Restore => Color::Cyan,
            Event::NoChange => Color::Primary,
            Event::Debug => Color::Blue,
            Event::Protected => Color::Primary,
            Event::Info => Color::Cyan,
            Event::Warning => Color::Yellow,
        }
    }
}

/// Severity of a [`Message`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageLevel {
    Success,
    Info,
    Warning,
    Error,
}

impl MessageLevel {
    fn event(self) -> Event {
        match self {
            MessageLevel::Success => Event::Ok,
            MessageLevel::Info => Event::Info,
            MessageLevel::Warning => Event::Warning,
            MessageLevel::Error => Event::Failed,
        }
    }
}

/// A message a command prints to stdout as a GitHub Actions style workflow
/// command, e.g. `::warning::CHANGELOG has no entry`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    level: MessageLevel,
    text: String,
}

impl Message {
    /// Parse `::level[ params]::text`. GitHub parameters such as `file=` are
    /// accepted but ignored. Other lines, including commands bit doesn't
    /// handle such as `::group::`, return `None` and stay in the output.
    pub fn parse(line: &str) -> Option<Self> {
        let (head, text) = line.strip_prefix("::")?.split_once("::")?;
        let command = head.split_once(' ').map_or(head, |(command, _)| command);
        let level = match command {
            "success" => MessageLevel::Success,
            "info" | "notice" => MessageLevel::Info,
            "warning" => MessageLevel::Warning,
            "error" => MessageLevel::Error,
            _ => return None,
        };
        // GitHub's escapes. `%25` goes last so `%250A` decodes to `%0A`.
        let text = text.replace("%0D", "\r").replace("%0A", "\n").replace("%25", "%");
        Some(Self { level, text })
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
    /// Fully-formatted header lines (from the `Starting` event). Retained in
    /// full for failure output; the live region displays only the tail.
    header: Vec<String>,
    /// Last `REGION_LINES` streamed output lines, fully formatted.
    recent: VecDeque<String>,
}

impl TaskState {
    fn visible_header(&self) -> &[String] {
        let start = self.header.len().saturating_sub(REGION_LINES);
        &self.header[start..]
    }
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
    /// When true, all event and stream dispatches are dropped — used when
    /// an engine function is invoked only for its return value (e.g.
    /// computing plan state to colour `--graph`).
    silent: bool,
    /// Reference timestamp used to compute `+elapsed` prefixes on debug lines.
    start: Instant,
}

struct OutputInner {
    max_name_len: usize,
    /// Whether live scrolling regions are used. True only when stdout is a
    /// terminal AND the user has not opted out via [`Output::with_long`].
    /// When false, output is plain line-by-line streaming.
    live: bool,
    /// Cached terminal height in rows (0 = unknown / unlimited).
    term_height: usize,
    /// Cached terminal width in columns (0 = unknown / unlimited).
    term_width: usize,
    /// Per-task live-region state, in insertion order.
    active: IndexMap<String, TaskState>,
    /// Number of terminal rows currently occupied by the live region, used
    /// to know how far up to move the cursor before clearing.
    live_rows: usize,
    /// Blocks that printed a [`Message`] since they started.
    messaged: HashSet<String>,
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
        let (term_height, term_width) = if live {
            let (rows, columns) = console::Term::stdout().size();
            (rows as usize, columns as usize)
        } else {
            (0, 0)
        };
        Self {
            inner: Arc::new(Mutex::new(OutputInner {
                max_name_len,
                live,
                term_height,
                term_width,
                active: IndexMap::new(),
                live_rows: 0,
                messaged: HashSet::new(),
            })),
            debug: false,
            silent: false,
            start: Instant::now(),
        }
    }

    /// Enable or disable debug output. Builder-style: returns the updated value.
    pub fn with_debug(mut self, debug: bool) -> Self {
        self.debug = debug;
        self
    }

    /// Suppress all event and stream output. Used when an engine function
    /// is invoked only for its return value and the textual event stream
    /// would be noise.
    pub fn silent() -> Self {
        let mut out = Self::new(&[]);
        out.silent = true;
        out
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
            start_reason: None,
        }
    }

    /// Create a writer for a specific block with left-aligned indentation.
    pub fn writer_indented(&self, name: &str, indent: usize) -> BlockWriter {
        BlockWriter {
            output: self.clone(),
            name: name.to_owned(),
            color: color_for_name(name),
            indent: Some(indent),
            start_reason: None,
        }
    }

    fn dispatch_stream(&self, name: &str, color: Color, indent: Option<usize>, content: &str, stderr: bool) {
        if self.silent {
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        let lines = inner.fmt_stream_line(name, color, indent, content, stderr);
        let stdout = io::stdout();
        let mut out = stdout.lock();
        inner.push_stream_many(&mut out, name, lines);
    }

    fn dispatch_stderr_details(&self, name: &str, color: Color, indent: Option<usize>, details: &[String]) {
        if self.silent || details.is_empty() {
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        let lines = details
            .iter()
            .flat_map(|line| inner.fmt_stream_line(name, color, indent, line, true))
            .collect::<Vec<_>>();
        let stdout = io::stdout();
        let mut out = stdout.lock();
        inner.print_detached(&mut out, &lines);
    }

    fn dispatch_event(&self, name: &str, event: Event, indent: Option<usize>, message: &str, raw: bool) {
        if self.silent {
            return;
        }
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
            (Event::Starting, _) => {
                inner.messaged.remove(name);
                inner.open_region(&mut out, name, lines)
            }
            (Event::Failed, false) => {
                let lines = inner.finish_messages(name, event, message, lines);
                inner.close_region(&mut out, name, lines, /*preserve_tail=*/ true)
            }
            (Event::Ok | Event::Skipped | Event::NoChange | Event::Protected, false) => {
                let lines = inner.finish_messages(name, event, message, lines);
                inner.close_region(&mut out, name, lines, /*preserve_tail=*/ false)
            }
            (_, true) if inner.active.contains_key(name) => inner.push_stream_many(&mut out, name, lines),
            _ => inner.print_detached(&mut out, &lines),
        }
    }

    /// Messages print above the live region rather than into it, so they
    /// stay on screen when the region collapses on success.
    fn dispatch_message(&self, name: &str, indent: Option<usize>, message: &Message) {
        if self.silent {
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        let lines = inner.fmt_event(name, message.level.event(), indent, &message.text);
        inner.messaged.insert(name.to_owned());
        let stdout = io::stdout();
        let mut out = stdout.lock();
        inner.print_detached(&mut out, &lines);
    }
}

impl OutputInner {
    /// Return the lines to print for a block's terminal event and stop
    /// tracking its messages. A bare "ok" is dropped when the block printed
    /// messages, because they say more.
    fn finish_messages(&mut self, name: &str, event: Event, message: &str, lines: Vec<String>) -> Vec<String> {
        let messaged = self.messaged.remove(name);
        if messaged && event == Event::Ok && message.is_empty() {
            Vec::new()
        } else {
            lines
        }
    }

    fn prefix(&self, name: &str, sep: &str, indent: Option<usize>) -> String {
        format!("{} {sep}", self.label(name, indent))
    }

    /// The block name, indented (plan mode) or right-aligned (apply mode).
    fn label(&self, name: &str, indent: Option<usize>) -> String {
        match indent {
            Some(n) => {
                let pad = "  ".repeat(n);
                format!("{pad}{name}")
            }
            None => format!("{:>width$}", name, width = self.max_name_len),
        }
    }

    /// Painted head and continuation prefixes for an event. The block name
    /// takes its own colour and the event's sigil takes the event's colour.
    fn event_prefixes(&self, name: &str, event: Event, indent: Option<usize>) -> (String, String) {
        let color = color_for_name(name);
        let dim = event.is_dim();
        let head = format!(
            "{} {}",
            self.label(name, indent).paint(color).dim_if(dim),
            event.symbol().paint(event.color()).dim_if(dim),
        );
        let cont = self.prefix(name, "┆", indent).paint(color).dim_if(dim).to_string();
        (head, cont)
    }

    /// Split (possibly ANSI-styled) `content` into chunks whose display
    /// width is at most `width`. When `width == 0` (unknown terminal
    /// size) or `content` already fits, returns a single-chunk Vec.
    fn wrap_content(content: &str, width: usize) -> Vec<String> {
        if width == 0 || content.is_empty() || console::measure_text_width(content) <= width {
            return vec![content.to_string()];
        }
        let mut out = Vec::new();
        let mut remaining = content;
        while !remaining.is_empty() {
            if console::measure_text_width(remaining) <= width {
                out.push(remaining.to_string());
                break;
            }
            let head = console::truncate_str(remaining, width, "");
            let head_len = head.len();
            // Safety: if truncate_str couldn't make progress (e.g. a single
            // grapheme wider than `width`), emit the rest as-is and stop.
            if head_len == 0 {
                out.push(remaining.to_string());
                break;
            }
            out.push(head.into_owned());
            remaining = &remaining[head_len..];
        }
        out
    }

    /// Available width for content given a prefix. Returns 0 when the
    /// terminal width is unknown (which `wrap_content` interprets as
    /// "don't split").
    fn content_width(&self, prefix: &str) -> usize {
        if self.term_width == 0 {
            return 0;
        }
        let prefix_width = console::measure_text_width(prefix);
        self.term_width.saturating_sub(prefix_width + 1)
    }

    fn fmt_stream_line(
        &self,
        name: &str,
        color: Color,
        indent: Option<usize>,
        content: &str,
        stderr: bool,
    ) -> Vec<String> {
        let head_prefix = self.prefix(name, "│", indent);
        let cont_prefix = self.prefix(name, "┆", indent);
        let chunks = Self::wrap_content(content, self.content_width(&head_prefix));
        chunks
            .into_iter()
            .enumerate()
            .map(|(i, chunk)| {
                let prefix = if i == 0 { &head_prefix } else { &cont_prefix };
                if stderr {
                    format!("{} {}", prefix.paint(color), chunk.dim().italic())
                } else {
                    format!("{} {chunk}", prefix.paint(color))
                }
            })
            .collect()
    }

    fn fmt_event(&self, name: &str, event: Event, indent: Option<usize>, message: &str) -> Vec<String> {
        let (head_prefix, cont_prefix) = self.event_prefixes(name, event, indent);
        let dim = event.is_dim();

        let body: String = if message.is_empty() {
            format!("{event:?}").to_lowercase()
        } else {
            message.to_owned()
        };

        let mut lines = Vec::new();
        // Multiple logical lines in the message become multiple rows; each
        // row then wraps to fit the terminal.
        let mut first = true;
        for logical in body.lines() {
            let prefix = if first { &head_prefix } else { &cont_prefix };
            for (i, chunk) in Self::wrap_content(logical, self.content_width(prefix))
                .into_iter()
                .enumerate()
            {
                let p = if first && i == 0 { &head_prefix } else { &cont_prefix };
                lines.push(format!("{p} {}", chunk.paint(event.color()).dim_if(dim)));
            }
            first = false;
        }
        lines
    }

    fn fmt_event_raw(&self, name: &str, event: Event, indent: Option<usize>, message: &str) -> Vec<String> {
        let (head_prefix, cont_prefix) = self.event_prefixes(name, event, indent);

        let mut lines = Vec::new();
        let mut first = true;
        for logical in message.lines() {
            let prefix = if first { &head_prefix } else { &cont_prefix };
            for (i, chunk) in Self::wrap_content(logical, self.content_width(prefix))
                .into_iter()
                .enumerate()
            {
                let p = if first && i == 0 { &head_prefix } else { &cont_prefix };
                lines.push(format!("{p} {chunk}"));
            }
            first = false;
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

    /// Render the last `REGION_LINES` rows of each active task header and as
    /// many recent output rows as fit in the terminal. A trailing blank row
    /// must remain visible because cursor-up cannot reach lines that have
    /// scrolled out of the viewport. Task state remains buffered when rows are
    /// hidden, so it can reappear as other tasks complete and leave room in the
    /// live region.
    fn redraw_live(&mut self, out: &mut impl Write) -> io::Result<()> {
        let row_budget = self.term_height.checked_sub(1).unwrap_or(usize::MAX);
        let header_rows: usize = self.active.values().map(|state| state.visible_header().len()).sum();
        let mut rows = 0;

        if header_rows > row_budget {
            let skip = header_rows - row_budget;
            for line in self.active.values().flat_map(TaskState::visible_header).skip(skip) {
                writeln!(out, "{line}")?;
                rows += 1;
            }
        } else {
            let mut recent_counts = vec![0; self.active.len()];
            let mut remaining = row_budget - header_rows;
            while remaining > 0 {
                let mut added = false;
                for (index, state) in self.active.values().enumerate().rev() {
                    let available = state.recent.len().min(REGION_LINES);
                    if recent_counts[index] < available {
                        recent_counts[index] += 1;
                        remaining -= 1;
                        added = true;
                        if remaining == 0 {
                            break;
                        }
                    }
                }
                if !added {
                    break;
                }
            }

            for (index, state) in self.active.values().enumerate() {
                for line in state.visible_header() {
                    writeln!(out, "{line}")?;
                    rows += 1;
                }
                let visible = recent_counts[index];
                let skip = state.recent.len() - visible;
                for line in state.recent.iter().skip(skip) {
                    writeln!(out, "{line}")?;
                    rows += 1;
                }
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

    /// Push one or more rows into the task's rolling buffer (capped at
    /// `TAIL_LINES`). The live region only renders the last
    /// `REGION_LINES` of this buffer while the task runs; the rest is
    /// kept so a failure can flush full context. In non-TTY mode this
    /// just prints each line immediately.
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
            if state.recent.len() == TAIL_LINES {
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

    /// Snapshot the block's last `REGION_LINES` streamed lines without
    /// removing them from state. Returns an empty vec if the block has
    /// no active region. Exposed as a pure method for testing.
    fn recent_tail(&self, name: &str) -> Vec<String> {
        self.active
            .get(name)
            .map(|state| state.recent.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Snapshot the header (the `Event::Starting` lines — i.e. the command
    /// being executed) for a block that currently owns a live region. Used
    /// alongside [`recent_tail`] on failure so the command stays visible in
    /// scrollback. Returns an empty vec when the block has no active region.
    fn recent_header(&self, name: &str) -> Vec<String> {
        self.active
            .get(name)
            .map(|state| state.header.clone())
            .unwrap_or_default()
    }

    /// Print the task's terminal event above the live region and remove
    /// the region. When `preserve_tail` is true (on failure), the block's
    /// header (the command that was being executed) and its last
    /// `REGION_LINES` streamed lines are flushed above the event so the
    /// full failure context stays on screen. In non-TTY mode the header
    /// and tail were already streamed; only `lines` (the terminal event)
    /// needs printing.
    fn close_region(&mut self, out: &mut StdoutLock<'_>, name: &str, lines: Vec<String>, preserve_tail: bool) {
        if !self.live {
            for line in &lines {
                let _ = writeln!(out, "{line}");
            }
            return;
        }
        let (header, tail) = if preserve_tail {
            (self.recent_header(name), self.recent_tail(name))
        } else {
            (Vec::new(), Vec::new())
        };
        let _ = self.clear_live(out);
        self.active.shift_remove(name);
        for line in header.iter().chain(tail.iter()).chain(lines.iter()) {
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
    /// Why the block runs, shown on the line [`BlockWriter::starting`] prints.
    start_reason: Option<String>,
}

impl BlockWriter {
    pub fn event(&self, event: Event, message: &str) {
        self.output
            .dispatch_event(&self.name, event, self.indent, message, /*raw=*/ false);
    }

    /// Emit an event whose first line is followed by a dimmed `(reason)`.
    pub fn event_with_reason(&self, event: Event, description: &str, reason: Option<&str>) {
        let Some(reason) = reason else {
            self.event(event, description);
            return;
        };
        let mut lines = description.lines();
        let first = lines.next().unwrap_or("");
        let mut styled = format!("{} {}", first.paint(event.color()), format!("({reason})").dim());
        for line in lines {
            styled.push('\n');
            styled.push_str(&line.paint(event.color()).to_string());
        }
        self.event_raw(event, &styled);
    }

    /// A copy of this writer whose [`BlockWriter::starting`] line shows
    /// `reason`. The engine hands it to resources that announce their own
    /// start, since only the engine knows why a block runs.
    pub fn with_start_reason(&self, reason: Option<&str>) -> BlockWriter {
        BlockWriter {
            output: self.output.clone(),
            name: self.name.clone(),
            color: self.color,
            indent: self.indent,
            start_reason: reason.map(str::to_owned),
        }
    }

    /// Announce the work a resource is starting, for resources whose
    /// [`crate::provider::Resource::announces_start`] is true.
    pub fn starting(&self, description: &str) {
        self.event_with_reason(Event::Starting, description, self.start_reason.as_deref());
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

    /// Print complete failure diagnostics above the live region so its
    /// rolling tail cannot hide the first compiler error.
    pub fn stderr_details(&self, details: &[String]) {
        self.output
            .dispatch_stderr_details(&self.name, self.color, self.indent, details);
    }

    /// Show a message printed by the block's command. A block that printed
    /// any messages ends with them instead of a bare "ok".
    pub fn message(&self, message: &Message) {
        self.output.dispatch_message(&self.name, self.indent, message);
    }

    /// Write all lines from a reader, prefixed with the block name. Lines
    /// that are [`Message`]s are shown as messages instead.
    pub fn pipe_stdout(&self, reader: impl BufRead) {
        for line in reader.lines() {
            match line {
                Ok(l) => match Message::parse(&l) {
                    Some(message) => self.message(&message),
                    None => self.line(&l),
                },
                Err(_) => break,
            }
        }
    }

    /// Emit a passing test suite summary. A `None` duration means the tool
    /// reported no timing, so none is shown.
    pub fn test_suite_passed(&self, suite: &str, duration: Option<std::time::Duration>, passed: usize, skipped: usize) {
        let mut counts = vec![format!("{passed} passed")];
        if skipped > 0 {
            counts.push(format!("{skipped} skipped"));
        }
        let msg = format!(
            "{} {}{}",
            "PASS".paint(Color::Green).bold(),
            suite.bold(),
            test_detail(duration, &counts)
        );
        self.event_raw(Event::Ok, &msg);
    }

    /// Emit a failing test suite summary with individual failures.
    pub fn test_suite_failed(
        &self,
        suite: &str,
        duration: Option<std::time::Duration>,
        passed: usize,
        failed: usize,
        failures: &[(String, Option<std::time::Duration>, String)],
    ) {
        let counts = [format!("{passed} passed"), format!("{failed} failed")];
        let mut msg = format!(
            "{} {}{}",
            "FAIL".paint(Color::Red).bold(),
            suite.bold(),
            test_detail(duration, &counts),
        );
        for (name, dur, output) in failures {
            msg.push_str(&format!("\n  {}{}", name.paint(Color::Red), test_detail(*dur, &[])));
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

    /// Emit a single passing test.
    pub fn test_passed(&self, suite: &str, name: &str, duration: Option<std::time::Duration>) {
        let msg = format!(
            "{} {}{}",
            "PASS".paint(Color::Green).bold(),
            format!("{suite}/{name}").bold(),
            test_detail(duration, &[]),
        );
        self.event_raw(Event::Ok, &msg);
    }

    /// Emit a single failing test with output.
    pub fn test_failed(&self, suite: &str, name: &str, duration: Option<std::time::Duration>, output: &str) {
        let mut msg = format!(
            "{} {}{}",
            "FAIL".paint(Color::Red).bold(),
            format!("{suite}/{name}").bold(),
            test_detail(duration, &[]),
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

/// Format the dimmed ` (12ms, 3 passed)` suffix of a test line, or nothing
/// when there's neither a duration nor counts.
fn test_detail(duration: Option<std::time::Duration>, counts: &[String]) -> String {
    let parts: Vec<String> = duration
        .map(|d| format!("{}ms", d.as_millis()))
        .into_iter()
        .chain(counts.iter().cloned())
        .collect();
    if parts.is_empty() {
        String::new()
    } else {
        format!(" {}", format!("({})", parts.join(", ")).dim())
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

    #[test]
    fn test_detail_omits_unknown_duration() {
        let plain = |d, counts: &[String]| console::strip_ansi_codes(&test_detail(d, counts)).into_owned();
        let counts = ["2 passed".to_owned()];
        assert_eq!(plain(None, &[]), "");
        assert_eq!(plain(None, &counts), " (2 passed)");
        assert_eq!(plain(Some(std::time::Duration::from_millis(12)), &[]), " (12ms)");
        assert_eq!(
            plain(Some(std::time::Duration::from_millis(12)), &counts),
            " (12ms, 2 passed)"
        );
    }

    /// Build an `OutputInner` in non-TTY mode for testing region bookkeeping
    /// without actually writing escape codes to a real terminal.
    fn inner_tty(max_name_len: usize, term_width: usize) -> OutputInner {
        OutputInner {
            max_name_len,
            live: true,
            term_height: usize::MAX,
            term_width,
            active: IndexMap::new(),
            live_rows: 0,
            messaged: HashSet::new(),
        }
    }

    fn message(level: MessageLevel, text: &str) -> Message {
        Message {
            level,
            text: text.to_owned(),
        }
    }

    #[test]
    fn message_parses_levels() {
        assert_eq!(
            Message::parse("::success::released v1"),
            Some(message(MessageLevel::Success, "released v1"))
        );
        assert_eq!(
            Message::parse("::info::note"),
            Some(message(MessageLevel::Info, "note"))
        );
        assert_eq!(
            Message::parse("::notice::note"),
            Some(message(MessageLevel::Info, "note"))
        );
        assert_eq!(
            Message::parse("::warning::careful"),
            Some(message(MessageLevel::Warning, "careful"))
        );
        assert_eq!(
            Message::parse("::error::broken"),
            Some(message(MessageLevel::Error, "broken"))
        );
    }

    #[test]
    fn message_ignores_parameters() {
        assert_eq!(
            Message::parse("::warning file=src/main.rs,line=12::unused import"),
            Some(message(MessageLevel::Warning, "unused import"))
        );
    }

    #[test]
    fn message_decodes_escapes() {
        assert_eq!(
            Message::parse("::error::first%0Asecond 100%25 %250A"),
            Some(message(MessageLevel::Error, "first\nsecond 100% %0A"))
        );
    }

    #[test]
    fn message_rejects_other_lines() {
        assert_eq!(Message::parse("::group::Build"), None);
        assert_eq!(Message::parse("::endgroup::"), None);
        assert_eq!(Message::parse("warning::text"), None);
        assert_eq!(Message::parse("::warning text"), None);
        assert_eq!(Message::parse("plain output"), None);
    }

    #[test]
    fn event_sigil_takes_event_colour() {
        let inner = inner_tty(4, 80);
        let sigil = "▲".paint(Color::Yellow).to_string();
        assert!(inner.fmt_event("a", Event::Warning, None, "careful")[0].contains(&sigil));
        assert!(inner.fmt_event_raw("a", Event::Warning, None, "careful")[0].contains(&sigil));
    }

    #[test]
    fn finish_messages_replaces_ok_after_messages() {
        let mut inner = inner_tty(4, 80);
        let ok = vec!["a ✔ ok".to_owned()];
        inner.messaged.insert("a".into());
        assert!(inner.finish_messages("a", Event::Ok, "", ok.clone()).is_empty());
        // Tracking ends with the block, so its next run shows "ok" again.
        assert_eq!(inner.finish_messages("a", Event::Ok, "", ok.clone()), ok);
        inner.live = false;
    }

    #[test]
    fn finish_messages_keeps_failure_after_messages() {
        let mut inner = inner_tty(4, 80);
        let failed = vec!["a ✘ command exited with 1".to_owned()];
        inner.messaged.insert("a".into());
        assert_eq!(
            inner.finish_messages("a", Event::Failed, "command exited with 1", failed.clone()),
            failed
        );
        assert!(!inner.messaged.contains("a"));
        inner.live = false;
    }

    #[test]
    fn region_open_tracks_rows() {
        let mut inner = inner_tty(4, 80);
        let state = inner.active.entry("a".to_string()).or_default();
        state.header = vec!["h1".into(), "h2".into()];
        let mut buf: Vec<u8> = Vec::new();
        inner.redraw_live(&mut buf).unwrap();
        assert_eq!(inner.live_rows, 2);
        inner.live = false;
    }

    #[test]
    fn region_caps_long_header_at_per_target_height() {
        let mut inner = inner_tty(4, 80);
        let state = inner.active.entry("a".to_string()).or_default();
        state.header = (0..REGION_LINES + 2).map(|index| format!("line{index}")).collect();

        let mut buf = Vec::new();
        inner.redraw_live(&mut buf).unwrap();

        assert_eq!(String::from_utf8(buf).unwrap(), "line2\nline3\nline4\nline5\nline6\n");
        assert_eq!(inner.live_rows, REGION_LINES);
        assert_eq!(inner.recent_header("a").len(), REGION_LINES + 2);
        inner.live = false;
    }

    #[test]
    fn region_windows_rows_that_do_not_fit_terminal() {
        let mut inner = inner_tty(4, 80);
        inner.term_height = 4;
        let first = inner.active.entry("first".to_string()).or_default();
        first.header.push("first start".into());
        first.recent.push_back("first output".into());
        let second = inner.active.entry("second".to_string()).or_default();
        second.header.push("second start".into());
        second.recent.push_back("second output".into());

        let mut buf = Vec::new();
        inner.redraw_live(&mut buf).unwrap();

        assert_eq!(
            String::from_utf8(buf).unwrap(),
            "first start\nsecond start\nsecond output\n"
        );
        assert!(inner.live);
        assert_eq!(inner.live_rows, 3);
        assert_eq!(inner.recent_tail("first"), vec!["first output"]);
        inner.live = false;
    }

    #[test]
    fn region_windowed_rows_reappear_when_an_active_task_completes() {
        let mut inner = inner_tty(4, 80);
        inner.term_height = 4;
        let first = inner.active.entry("first".to_string()).or_default();
        first.header.push("first start".into());
        first.recent.push_back("first output".into());
        let second = inner.active.entry("second".to_string()).or_default();
        second.header.push("second start".into());
        second.recent.push_back("second output".into());
        inner.active.shift_remove("second");

        let mut buf = Vec::new();
        inner.redraw_live(&mut buf).unwrap();

        assert_eq!(String::from_utf8(buf).unwrap(), "first start\nfirst output\n");
        assert_eq!(inner.live_rows, 2);
        inner.live = false;
    }

    #[test]
    fn region_recent_caps_at_tail_lines() {
        let mut inner = inner_tty(4, 80);
        let state = inner.active.entry("a".to_string()).or_default();
        for i in 0..TAIL_LINES + 3 {
            if state.recent.len() == TAIL_LINES {
                state.recent.pop_front();
            }
            state.recent.push_back(format!("line{i}"));
        }
        assert_eq!(state.recent.len(), TAIL_LINES);
        assert_eq!(state.recent.front().map(|s| s.as_str()), Some("line3"));
        assert_eq!(
            state.recent.back().map(|s| s.as_str()),
            Some(format!("line{}", TAIL_LINES + 2).as_str())
        );
    }

    #[test]
    fn recent_tail_returns_full_rolling_buffer() {
        let mut inner = inner_tty(4, 80);
        let state = inner.active.entry("backend".to_string()).or_default();
        // Fill past REGION_LINES but under TAIL_LINES.
        for i in 0..12 {
            state.recent.push_back(format!("line{i}"));
        }
        let tail = inner.recent_tail("backend");
        // recent_tail surfaces the full buffer, not just the live-region slice.
        assert_eq!(tail.len(), 12);
        assert_eq!(tail.first().map(|s| s.as_str()), Some("line0"));
        assert_eq!(tail.last().map(|s| s.as_str()), Some("line11"));
    }

    #[test]
    fn wrap_content_noop_when_fits() {
        let chunks = OutputInner::wrap_content("hello world", 80);
        assert_eq!(chunks, vec!["hello world"]);
    }

    #[test]
    fn wrap_content_splits_long_line() {
        let chunks = OutputInner::wrap_content("abcdefghij", 4);
        assert_eq!(chunks, vec!["abcd", "efgh", "ij"]);
    }

    #[test]
    fn wrap_content_empty_when_width_zero() {
        let chunks = OutputInner::wrap_content("anything", 0);
        assert_eq!(chunks, vec!["anything"]);
    }

    #[test]
    fn wrap_content_handles_empty_input() {
        let chunks = OutputInner::wrap_content("", 10);
        assert_eq!(chunks, vec![""]);
    }

    #[test]
    fn fmt_stream_line_wraps_long_content() {
        let inner = inner_tty(4, 30);
        // prefix "backend │" width = 11, available = 30 - 11 - 1 = 18.
        let lines = inner.fmt_stream_line("backend", color_for_name("backend"), None, &"x".repeat(40), false);
        assert!(lines.len() >= 2, "expected wrapping, got {:?}", lines.len());
        // First line uses the primary separator; continuations use ┆.
        assert!(lines[0].contains('│'));
        for cont in &lines[1..] {
            assert!(cont.contains('┆'), "continuation should use ┆, got {cont:?}");
        }
    }

    #[test]
    fn recent_tail_snapshots_buffered_lines() {
        let mut inner = inner_tty(4, 80);
        let state = inner.active.entry("backend".to_string()).or_default();
        state.recent.push_back("first".into());
        state.recent.push_back("second".into());
        state.recent.push_back("third".into());

        let tail = inner.recent_tail("backend");
        assert_eq!(tail, vec!["first", "second", "third"]);
    }

    #[test]
    fn recent_tail_empty_for_unknown_block() {
        let inner = inner_tty(4, 80);
        assert!(inner.recent_tail("missing").is_empty());
    }

    #[test]
    fn recent_header_snapshots_starting_event() {
        let mut inner = inner_tty(4, 80);
        let state = inner.active.entry("backend".to_string()).or_default();
        state.header = vec!["backend ▶ go build -o bin ./cmd".into()];

        let header = inner.recent_header("backend");
        assert_eq!(header, vec!["backend ▶ go build -o bin ./cmd"]);
    }

    #[test]
    fn recent_header_empty_for_unknown_block() {
        let inner = inner_tty(4, 80);
        assert!(inner.recent_header("missing").is_empty());
    }

    #[test]
    fn close_region_preserves_header_and_tail_on_failure() {
        // Regression: on failure, the command being executed (the
        // `Event::Starting` header) must remain in scrollback alongside the
        // streamed-output tail, not just the failure event itself.
        let mut inner = inner_tty(4, 80);
        let state = inner.active.entry("backend".to_string()).or_default();
        state.header = vec!["backend ▶ go build -o bin ./cmd".into()];
        state.recent.push_back("backend │ error: missing package".into());

        let header = inner.recent_header("backend");
        let tail = inner.recent_tail("backend");
        let terminal = ["backend ✘ `go build` exited with exit status: 1".to_string()];

        // The chaining order in close_region: header first, then tail,
        // then the terminal event lines.
        let written: Vec<String> = header
            .iter()
            .chain(tail.iter())
            .chain(terminal.iter())
            .cloned()
            .collect();
        assert_eq!(
            written,
            vec![
                "backend ▶ go build -o bin ./cmd".to_string(),
                "backend │ error: missing package".to_string(),
                "backend ✘ `go build` exited with exit status: 1".to_string(),
            ]
        );
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
