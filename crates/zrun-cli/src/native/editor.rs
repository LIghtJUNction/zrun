//! An interactive edit owns the terminal only while collecting one command.
mod input;
use super::{Interrupt, Line, State, complete, highlight, history::History, prompt};
use input::{Key, Mode, Reader};
use std::{
    ffi::OsStr,
    io::{self, IsTerminal, Write},
};

pub(super) fn display_char(c: char) -> char {
    match c {
        '\n' => '↵',
        '\t' => '⇥',
        c if c.is_control() => '?',
        '\u{061c}'
        | '\u{200b}'..='\u{200f}'
        | '\u{2028}'..='\u{202e}'
        | '\u{2060}'..='\u{206f}'
        | '\u{feff}' => '?',
        c => c,
    }
}
fn display(s: &str) -> String {
    s.chars().map(display_char).collect()
}

#[derive(Default)]
struct Buffer {
    line: String,
    cursor: usize,
    history_index: Option<usize>,
    draft: String,
}
impl Buffer {
    fn previous(&self) -> usize {
        self.line.floor_char_boundary(self.cursor.saturating_sub(1))
    }
    fn next(&self) -> usize {
        self.line
            .ceil_char_boundary((self.cursor + 1).min(self.line.len()))
    }
    fn replace(&mut self, range: std::ops::Range<usize>, text: &str) {
        if range.start > range.end
            || !self.line.is_char_boundary(range.start)
            || !self.line.is_char_boundary(range.end)
            || self.line.len() - range.len() + text.len() > crate::execution::Batch::MAX_INPUT
        {
            return;
        }
        self.cursor = range.start + text.len();
        self.line.replace_range(range, text);
        self.history_index = None;
    }
    fn recall(&mut self, history: &History, older: bool) {
        let i = self.history_index.unwrap_or(history.len());
        let next = if older {
            i.saturating_sub(1)
        } else {
            (i + 1).min(history.len())
        };
        if i == next {
            return;
        }
        if self.history_index.is_none() {
            self.draft.clone_from(&self.line);
        }
        self.history_index = (next < history.len()).then_some(next);
        self.line = history.get(next).unwrap_or(&self.draft).to_owned();
        self.cursor = self.line.len();
    }
}

fn visible_prompt_width(prompt: &str) -> usize {
    let mut sgr = false;
    let mut prompt_width = 0;
    for c in prompt.chars() {
        if c == '\x1b' {
            sgr = true;
        }
        if !sgr {
            prompt_width += c.len_utf8();
        }
        if sgr && c == 'm' {
            sgr = false;
        }
    }
    prompt_width
}

struct Screen {
    prompt: String,
    ansi: bool,
    color: bool,
    echo: bool,
    columns: usize,
    prompt_width: usize,
    paste_enabled: bool,
}
impl Screen {
    fn new(state: &State, echo: bool) -> Self {
        let term = state.env.get(OsStr::new("TERM")).and_then(|v| v.to_str());
        let ansi = !matches!(term, None | Some("" | "dumb"));
        let color = ansi
            && state
                .env
                .get(OsStr::new("NO_COLOR"))
                .is_none_or(|s| s.is_empty());
        let prompt = prompt::current(state);
        let prompt_width = visible_prompt_width(&prompt);
        Self {
            prompt,
            ansi,
            color,
            echo,
            columns: 80,
            prompt_width,
            paste_enabled: false,
        }
    }
    fn begin(&mut self) -> io::Result<()> {
        let mut out = io::stderr().lock();
        if self.ansi && self.echo {
            self.paste_enabled = true;
            out.write_all(b"\x1b[?2004h")?;
        }
        write!(out, "{}", self.prompt)?;
        out.flush()
    }
    fn finish(&mut self) -> io::Result<()> {
        if self.paste_enabled {
            io::stderr().write_all(b"\x1b[?2004l")?;
            self.paste_enabled = false;
        }
        Ok(())
    }
    fn suggestion<'a>(&self, edit: &Buffer, history: &'a History) -> Option<&'a str> {
        if self.echo && self.color && edit.cursor == edit.line.len() && edit.line.len() <= 4096 {
            history.suggestion(&edit.line)
        } else {
            None
        }
    }

    fn redraw(
        &mut self,
        state: &State,
        edit: &Buffer,
        history: Option<&History>,
    ) -> io::Result<()> {
        if !self.echo {
            return Ok(());
        }
        let mut size = libc::winsize::default();
        // SAFETY: TIOCGWINSZ writes one winsize into the borrowed storage.
        if unsafe { libc::ioctl(2, libc::TIOCGWINSZ, &mut size) } == 0
            && size.ws_col > 0
            && self.columns != usize::from(size.ws_col)
        {
            self.columns = usize::from(size.ws_col);
            self.prompt = prompt::current(state);
            self.prompt_width = visible_prompt_width(&self.prompt);
        }
        // A one-row scrolling viewport avoids cursor drift at wraps and wide glyphs.
        // A UTF-8 byte budget is a conservative cell bound for printable Unicode.
        let room = self.columns.saturating_sub(self.prompt_width + 1);
        let start = edit
            .line
            .ceil_char_boundary(edit.cursor.saturating_sub(room));
        let end = edit
            .line
            .floor_char_boundary((start + room).min(edit.line.len()));
        let paint = |a, b| {
            if self.color && edit.line.len() <= 4096 {
                highlight::paint(&edit.line, a, b)
            } else {
                display(&edit.line[a..b])
            }
        };
        let left = paint(start, edit.cursor);
        let right = paint(edit.cursor, end);
        let mut out = io::stderr().lock();
        if self.ansi {
            write!(out, "\r{}{left}", self.prompt)?;
            // Save and restore the terminal's actual cursor; it knows the width of its font.
            write!(out, "\x1b7{right}")?;
            if let Some(full) = history.and_then(|h| self.suggestion(edit, h)) {
                let suffix = &full[edit.line.len()..];
                let room = room.saturating_sub(end - start).min(suffix.len());
                let ghost = display(&suffix[..suffix.floor_char_boundary(room)]);
                if !ghost.is_empty() {
                    write!(out, "\x1b[2m{ghost}\x1b[0m")?;
                }
            }
            out.write_all(b"\x1b[K\x1b8")?;
        } else {
            // Keep dumb terminals free of CSI. Erase the previous row with spaces.
            write!(
                out,
                "\r{}\r{}{left}{right}\r{}{left}",
                " ".repeat(self.columns.saturating_sub(1)),
                self.prompt,
                self.prompt
            )?;
        }
        out.flush()
    }
    fn choices(&self, candidates: &[String]) -> io::Result<()> {
        if self.echo && candidates.len() > 1 {
            let mut out = io::stderr().lock();
            writeln!(out)?;
            for name in candidates.iter().take(12) {
                writeln!(out, "{}", display(name))?;
            }
            if candidates.len() > 12 {
                writeln!(out, "… {} more", candidates.len() - 12)?;
            }
        }
        Ok(())
    }
}
impl Drop for Screen {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

pub(super) fn read_line(
    signal: &Interrupt,
    state: &State,
    history: &mut History,
) -> io::Result<Line> {
    let mut keys = Reader {
        signal,
        pending: None,
    };
    if !io::stderr().is_terminal() {
        return keys.canonical();
    }
    let terminal = Mode::enter()?;
    let mut screen = Screen::new(state, terminal.echo());
    screen.begin()?;
    let mut edit = Buffer::default();
    let mut too_long = false;
    let result = loop {
        let room = crate::execution::Batch::MAX_INPUT - edit.line.len();
        let key = keys.key(room)?;
        if !too_long
            && (matches!(key, Key::TooLong)
                || matches!(&key, Key::Text(t) if t.len() > room)
                || (room == 0 && matches!(key, Key::Byte(b' '..=b'~'))))
        {
            writeln!(
                io::stderr(),
                "\nzrun: input limit exceeded; discard this command with Enter or Ctrl-C"
            )?;
            too_long = true;
        }
        if too_long {
            match key {
                Key::Eof | Key::Byte(b'\n' | b'\r' | 4) => break Line::Rejected,
                Key::Cancel | Key::Byte(3) => break Line::Cancel,
                _ => continue,
            }
        }
        match key {
            Key::Cancel | Key::Byte(3) => {
                writeln!(io::stderr())?;
                break Line::Cancel;
            }
            Key::Eof | Key::Byte(4) if edit.line.is_empty() => break Line::Eof,
            Key::Eof | Key::Byte(b'\n' | b'\r') => {
                if screen.echo {
                    if edit.cursor != edit.line.len() || screen.suggestion(&edit, history).is_some()
                    {
                        edit.cursor = edit.line.len();
                        screen.redraw(state, &edit, None)?;
                    }
                    writeln!(io::stderr())?;
                }
                break Line::Text(edit.line);
            }
            Key::Byte(b'\t') => {
                let result = complete::complete(state, &edit.line, edit.cursor);
                if let Some((range, text)) = result.replace {
                    edit.replace(range, &text);
                }
                screen.choices(&result.candidates)?;
            }
            Key::Byte(8 | 127) => edit.replace(edit.previous()..edit.cursor, ""),
            Key::Byte(4) => edit.replace(edit.cursor..edit.next(), ""),
            Key::Byte(1) => edit.cursor = 0,
            Key::Byte(5) => end(&mut edit, &screen, history),
            Key::Byte(2) => edit.cursor = edit.previous(),
            Key::Byte(6) => right(&mut edit, &screen, history),
            Key::Byte(11) => edit.replace(edit.cursor..edit.line.len(), ""),
            Key::Byte(12) => {
                if screen.ansi && screen.echo {
                    io::stderr().write_all(b"\x1b[2J\x1b[H")?;
                }
            }
            Key::Byte(21) => edit.replace(0..edit.cursor, ""),
            Key::Byte(23) => {
                let prefix = &edit.line[..edit.cursor];
                let start = prefix
                    .trim_end()
                    .rfind(char::is_whitespace)
                    .map_or(0, |i| i + prefix[i..].chars().next().unwrap().len_utf8());
                edit.replace(start..edit.cursor, "");
            }
            Key::Byte(16) => edit.recall(history, true),
            Key::Byte(14) => edit.recall(history, false),
            Key::Sequence(seq) => match seq.as_slice() {
                b"A" => edit.recall(history, true),
                b"B" => edit.recall(history, false),
                b"C" => right(&mut edit, &screen, history),
                b"D" => edit.cursor = edit.previous(),
                b"H" | b"1~" | b"7~" => edit.cursor = 0,
                b"F" | b"4~" | b"8~" => end(&mut edit, &screen, history),
                b"3~" => edit.replace(edit.cursor..edit.next(), ""),
                _ => continue,
            },
            Key::Text(text) => edit.replace(edit.cursor..edit.cursor, &text),
            Key::Byte(b @ b' '..=b'~') => {
                edit.replace(edit.cursor..edit.cursor, &(b as char).to_string())
            }
            _ => continue,
        }
        screen.redraw(state, &edit, Some(history))?;
    };
    screen.finish()?;
    terminal.restore()?;
    if screen.echo
        && let Line::Text(text) = &result
    {
        history.push(text);
    }
    Ok(result)
}
fn end(edit: &mut Buffer, screen: &Screen, history: &History) {
    if let Some(full) = screen.suggestion(edit, history) {
        edit.replace(0..edit.line.len(), full);
    } else {
        edit.cursor = edit.line.len();
    }
}
fn right(edit: &mut Buffer, screen: &Screen, history: &History) {
    if edit.cursor == edit.line.len() {
        end(edit, screen, history);
    } else {
        edit.cursor = edit.next();
    }
}
