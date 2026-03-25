use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::io::{self, BufRead, Write};
use std::sync::{Arc, Mutex};

use yansi::{Color, Paint};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    Starting,
    Skipped,
    Done,
    Failed,
}

impl Event {
    fn symbol(&self) -> &'static str {
        match self {
            Event::Starting => "▶",
            Event::Skipped => "·",
            Event::Done => "✔",
            Event::Failed => "✘",
        }
    }

    fn color(&self) -> Color {
        match self {
            Event::Starting => Color::Cyan,
            Event::Skipped => Color::Primary,
            Event::Done => Color::Green,
            Event::Failed => Color::Red,
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
}

struct OutputInner {
    max_name_len: usize,
}

impl Output {
    pub fn new(block_names: &[&str]) -> Self {
        let max_name_len = block_names.iter().map(|n| n.len()).max().unwrap_or(0);
        Self {
            inner: Arc::new(Mutex::new(OutputInner { max_name_len })),
        }
    }

    /// Create a writer for a specific block.
    pub fn writer(&self, name: &str) -> BlockWriter {
        BlockWriter {
            output: self.clone(),
            name: name.to_owned(),
            color: color_for_name(name),
        }
    }

    fn print_line(&self, name: &str, color: Color, content: &str) {
        let inner = self.inner.lock().unwrap();
        let prefix = format!("{:>width$} │", name, width = inner.max_name_len);
        println!("{} {content}", prefix.paint(color));
    }

    fn print_stderr_line(&self, name: &str, color: Color, content: &str) {
        let inner = self.inner.lock().unwrap();
        let prefix = format!("{:>width$} │", name, width = inner.max_name_len);
        println!("{} {}", prefix.paint(color), content.dim().italic());
    }

    fn print_event(&self, name: &str, event: Event, message: &str) {
        let inner = self.inner.lock().unwrap();
        let color = color_for_name(name);
        let prefix = format!("{:>width$} {}", name, event.symbol(), width = inner.max_name_len);
        if message.is_empty() {
            println!(
                "{} {}",
                prefix.paint(color),
                format!("{event:?}").to_lowercase().paint(event.color())
            );
        } else {
            println!("{} {}", prefix.paint(color), message.paint(event.color()));
        }
    }
}

/// A writer bound to a specific block, used by providers to emit output.
pub struct BlockWriter {
    output: Output,
    name: String,
    color: Color,
}

impl BlockWriter {
    pub fn event(&self, event: Event, message: &str) {
        self.output.print_event(&self.name, event, message);
    }

    pub fn line(&self, content: &str) {
        self.output.print_line(&self.name, self.color, content);
    }

    pub fn stderr_line(&self, content: &str) {
        self.output.print_stderr_line(&self.name, self.color, content);
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
