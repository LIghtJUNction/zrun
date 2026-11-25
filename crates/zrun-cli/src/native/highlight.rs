//! Color the editable input without interpreting it or touching the filesystem.
use super::editor::display_char;

#[derive(Clone, Copy, PartialEq)]
enum Style {
    Plain,
    Program,
    Quoted,
    Variable,
    Operator,
    Comment,
}

impl Style {
    fn sgr(self) -> &'static str {
        match self {
            Self::Plain => "\x1b[0m",
            Self::Program => "\x1b[36m",
            Self::Quoted => "\x1b[32m",
            Self::Variable => "\x1b[33m",
            Self::Operator => "\x1b[34m",
            Self::Comment => "\x1b[2m",
        }
    }
}

/// Color only the visible UTF-8 byte range. The prefix still determines quoting
/// and command position when the viewport begins in the middle of a token.
pub(super) fn paint(line: &str, start: usize, end: usize) -> String {
    debug_assert!(start <= end && line.is_char_boundary(start) && line.is_char_boundary(end));
    let mut out = String::new();
    let mut previous = Style::Plain;
    let mut quote = None;
    let mut escaped = false;
    let mut comment = false;
    let mut command_expected = true;
    let mut word = None;
    let mut variable = false;
    for (i, c) in line[..end].char_indices() {
        let style = if comment && c != '\n' {
            Style::Comment
        } else if escaped {
            escaped = false;
            if quote.is_some() {
                Style::Quoted
            } else {
                word.unwrap_or(Style::Plain)
            }
        } else if let Some(q) = quote {
            if c == q {
                quote = None;
            } else if c == '\\' && q == '"' {
                escaped = true;
            }
            Style::Quoted
        } else if c.is_whitespace() {
            word = None;
            variable = false;
            if c == '\n' {
                comment = false;
                command_expected = true;
            }
            Style::Plain
        } else if c == '#' && word.is_none() {
            comment = true;
            Style::Comment
        } else if matches!(c, '|' | '&' | ';' | '<' | '>') {
            word = None;
            variable = false;
            if matches!(c, '|' | '&' | ';') {
                command_expected = true;
            }
            Style::Operator
        } else {
            let normal = *word.get_or_insert_with(|| {
                if command_expected {
                    command_expected = false;
                    Style::Program
                } else {
                    Style::Plain
                }
            });
            if c == '\'' || c == '"' {
                quote = Some(c);
                variable = false;
                Style::Quoted
            } else if c == '\\' {
                escaped = true;
                variable = false;
                normal
            } else {
                variable = c == '$' || (variable && (c.is_ascii_alphanumeric() || c == '_'));
                if variable { Style::Variable } else { normal }
            }
        };
        if i >= start {
            if style != previous {
                out.push_str(style.sgr());
                previous = style;
            }
            out.push(display_char(c));
        }
    }
    if previous != Style::Plain {
        out.push_str(Style::Plain.sgr());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_position_quotes_variables_and_visible_range() {
        let line = "echo 'one two' | cat $path # comment";
        assert!(paint(line, 0, line.len()).contains("\x1b[36mcat"));
        assert_eq!(paint(line, 7, 10), "\x1b[32mne \x1b[0m");
        assert!(paint(line, 0, line.len()).contains("\x1b[33m$path"));
        assert!(paint(line, 0, line.len()).ends_with("\x1b[2m# comment\x1b[0m"));
    }
}
