//! A completion menu previews choices without replacing the user's editable filter.
use super::{Buffer, Key, complete, display_char};
use std::io::{self, Write};

pub(super) enum Action {
    Cycle(bool),
    Cancel,
    Accept,
    Commit,
    Filter,
    Dismiss,
}

pub(super) fn action(key: &Key) -> Action {
    match key {
        Key::Byte(b'\t' | 14) => Action::Cycle(true),
        Key::BackTab | Key::Byte(16) => Action::Cycle(false),
        Key::Sequence(s) if s == b"B" => Action::Cycle(true),
        Key::Sequence(s) if s == b"A" => Action::Cycle(false),
        Key::Escape => Action::Cancel,
        Key::Byte(b' ') => Action::Accept,
        Key::Sequence(s) if s == b"C" => Action::Accept,
        Key::Byte(b'\n' | b'\r') => Action::Commit,
        Key::Byte(8 | 127 | 4 | 23 | b'!'..=b'~') | Key::Text(_) => Action::Filter,
        _ => Action::Dismiss,
    }
}

pub(super) struct Menu {
    original: String,
    original_cursor: usize,
    original_history_index: Option<usize>,
    options: Vec<String>,
    selected: usize,
}

impl Menu {
    pub fn new(edit: &Buffer, options: Vec<String>) -> Self {
        Self {
            original: edit.line.clone(),
            original_cursor: edit.cursor,
            original_history_index: edit.history_index,
            options,
            selected: 0,
        }
    }
    pub fn filtered(&mut self, options: Vec<String>) {
        self.options = options;
        self.selected = 0;
    }
    pub fn step(&mut self, forward: bool) {
        let n = self.options.len();
        if n > 0 {
            self.selected = if forward {
                (self.selected + 1) % n
            } else {
                (self.selected + n - 1) % n
            };
        }
    }
    pub fn has_choice(&self) -> bool {
        !self.options.is_empty()
    }

    pub fn accept(&self, edit: &mut Buffer) -> bool {
        let Some(name) = self.options.get(self.selected) else {
            return false;
        };
        let Some((range, text)) = complete::select(&edit.line, edit.cursor, name) else {
            return false;
        };
        if edit.line.len() - range.len() + text.len() > crate::execution::Batch::MAX_INPUT {
            return false;
        }
        edit.replace(range, &text);
        true
    }
    pub fn cancel(self, edit: &mut Buffer) {
        edit.line = self.original;
        edit.cursor = self.original_cursor;
        edit.history_index = self.original_history_index;
    }
    fn rows(&self, limit: usize) -> usize {
        self.options.len().max(1).min(limit)
    }
    fn row(&self, i: usize, height: usize, width: usize, color: bool) -> String {
        if self.options.is_empty() {
            return clip("  No matches", width);
        }
        // Keep the selected row inside a stable page when cycling through many matches.
        let first =
            (self.selected / height * height).min(self.options.len().saturating_sub(height));
        let index = first + i;
        let selected = index == self.selected;
        let marker = match (selected, color) {
            (true, true) => "› ",
            (true, false) => "> ",
            _ => "  ",
        };
        let text = if selected && width >= 24 && self.options.len() > height {
            let position = format!("  {}/{}", index + 1, self.options.len());
            format!(
                "{}{}",
                clip(
                    &format!("{marker}{}", self.options[index]),
                    width.saturating_sub(position.len())
                ),
                position
            )
        } else {
            clip(&format!("{marker}{}", self.options[index]), width)
        };
        if selected && color {
            format!("\x1b[1;36m{text}\x1b[0m")
        } else {
            text
        }
    }
}

// Most non-ASCII narrow glyphs can safely use less space than this bound. Han
// characters and emoji use two cells. Never split a scalar or paint a soft wrap.
fn cells(c: char) -> usize {
    if c.is_ascii() { 1 } else { 2 }
}

fn fit(chars: impl Iterator<Item = char>, width: usize) -> String {
    let mut used = 0;
    chars
        .take_while(|&c| {
            used += cells(c);
            used <= width
        })
        .collect()
}

fn clip(s: &str, width: usize) -> String {
    let clean: String = s.chars().map(display_char).collect();
    if clean.chars().map(cells).sum::<usize>() <= width {
        return clean;
    }
    if width < 6 {
        return fit(clean.chars(), width);
    }
    let first = fit(clean.chars(), (width - 2) / 2);
    let remaining = width - 2 - first.chars().map(cells).sum::<usize>();
    let last = fit(clean.chars().rev(), remaining);
    format!("{first}…{}", last.chars().rev().collect::<String>())
}

#[derive(Default)]
pub(super) struct Panel {
    lines: Vec<String>,
}
impl Panel {
    pub fn draw(
        &mut self,
        out: &mut impl Write,
        menu: Option<&Menu>,
        columns: usize,
        rows: usize,
        ansi: bool,
        color: bool,
    ) -> io::Result<()> {
        let width = columns.saturating_sub(1);
        if !ansi || rows < 2 {
            // A one-row or dumb terminal can show the current choice without CSI or scrolling.
            if let Some(m) = menu {
                write!(
                    out,
                    "\r{}\r{}",
                    " ".repeat(width),
                    m.row(0, 1, width, false)
                )?;
            }
            self.lines.clear();
            return Ok(());
        }
        let capacity = if columns < 24 { 1 } else { 6 };
        let height = menu.map_or(0, |m| m.rows(capacity.min(rows - 1)));
        let old = self.lines.len().min(rows - 1);
        if old == 0 && height == 0 {
            return Ok(());
        }
        if height > old {
            // Make space only on expansion. At the screen bottom, this scrolls
            // the input up together with the viewport, preserving their anchor.
            if old > 0 {
                write!(out, "\x1b[{old}B")?;
            }
            for _ in old..height {
                out.write_all(b"\r\n")?;
            }
            write!(out, "\x1b[{height}A")?;
        }
        let next: Vec<_> = menu
            .into_iter()
            .flat_map(|m| (0..height).map(move |i| m.row(i, height, width, color)))
            .collect();
        let mut at = 0;
        for i in 0..old.max(height) {
            if self.lines.get(i) == next.get(i) {
                continue;
            }
            write!(out, "\x1b[{}B\r\x1b[2K", i + 1 - at)?;
            if let Some(row) = next.get(i) {
                out.write_all(row.as_bytes())?;
            }
            at = i + 1;
        }
        if at > 0 {
            write!(out, "\x1b[{at}A\r")?;
        }
        self.lines = next;
        Ok(())
    }
}
