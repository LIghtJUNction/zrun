//! Parse the interactive shell's supported grammar without evaluating external shell code.
use super::error;
use std::io;

#[derive(Debug, Clone)]
pub(super) enum Part {
    Text(String),
    Var(String),
}
pub(super) type Word = Vec<Part>;
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Redirect {
    Input,
    Output,
    Append,
    Error,
    AppendError,
    ErrorToOutput,
    OutputToError,
}
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) enum Link {
    Always,
    Success,
    Failure,
}
#[derive(Debug, Clone)]
enum Token {
    Word(Word),
    Pipe,
    Link(Link),
    Redirect(Redirect),
}
#[derive(Debug, Clone, Default)]
pub(super) struct Command {
    pub words: Vec<Word>,
    pub redirects: Vec<(Redirect, Word)>,
}
pub(super) type Program = Vec<(Link, Vec<Command>)>;

fn literal(word: &mut Word, c: char) {
    if let Some(Part::Text(s)) = word.last_mut() {
        s.push(c);
    } else {
        word.push(Part::Text(c.to_string()));
    }
}
pub(super) fn identifier(s: &str) -> bool {
    let mut ch = s.bytes();
    ch.next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
        && ch.all(|c| c.is_ascii_alphanumeric() || c == b'_')
}
fn variable(chars: &[char], at: &mut usize, word: &mut Word) -> io::Result<()> {
    let start = *at;
    let name = match chars.get(*at) {
        Some(c @ ('?' | '0'..='9' | '$' | '#' | '@')) => {
            *at += 1;
            c.to_string()
        }
        Some('{') => {
            *at += 1;
            let first = *at;
            while *at < chars.len() && chars[*at] != '}' {
                *at += 1;
            }
            if *at == chars.len() {
                return Err(error("unclosed parameter expansion"));
            }
            let s: String = chars[first..*at].iter().collect();
            *at += 1;
            if !identifier(&s)
                && s.parse::<usize>().is_err()
                && !matches!(s.as_str(), "?" | "$" | "@" | "#")
            {
                return Err(error("unsupported parameter expansion"));
            }
            s
        }
        Some('(') => {
            return Err(error(
                "command substitution requires the explicit shell command",
            ));
        }
        Some(c) if c.is_ascii_alphabetic() || *c == '_' => {
            while chars
                .get(*at)
                .is_some_and(|c| c.is_ascii_alphanumeric() || *c == '_')
            {
                *at += 1
            }
            chars[start..*at].iter().collect()
        }
        _ => {
            literal(word, '$');
            return Ok(());
        }
    };
    word.push(Part::Var(name));
    Ok(())
}
fn lex(line: &str) -> io::Result<Vec<Token>> {
    let chars: Vec<_> = line.chars().collect();
    let mut i = 0;
    let mut tokens = Vec::new();
    let mut word: Word = Vec::new();
    let mut inside = false;
    let mut quote = None;
    while i < chars.len() {
        let c = chars[i];
        i += 1;
        if quote == Some(c) {
            quote = None;
            continue;
        }
        if quote != Some('\'') && c == '\\' {
            let escaped = *chars.get(i).ok_or_else(|| error("unfinished escape"))?;
            if quote == Some('"') && !matches!(escaped, '$' | '"' | '\\' | '\n' | '`') {
                literal(&mut word, '\\');
            } else if escaped == '\n' {
                i += 1;
                continue;
            }
            literal(&mut word, escaped);
            i += 1;
            inside = true;
            continue;
        }
        if quote != Some('\'') && c == '$' {
            variable(&chars, &mut i, &mut word)?;
            inside = true;
            continue;
        }
        if quote.is_some() {
            literal(&mut word, c);
            continue;
        }
        if matches!(c, '\'' | '"') {
            quote = Some(c);
            inside = true;
            continue;
        }
        if !inside && c == '#' {
            break;
        }
        if c == '`' {
            return Err(error(
                "backtick substitution requires the explicit shell command",
            ));
        }
        let c = if !inside
            && ((c == '1' && chars.get(i) == Some(&'>'))
                || (c == '0' && chars.get(i) == Some(&'<')))
        {
            i += 1;
            chars[i - 1]
        } else {
            c
        };
        let stderr = c == '2' && !inside && chars.get(i) == Some(&'>');
        let op = if stderr {
            i += 1;
            if chars[i..].starts_with(&['&', '1']) {
                i += 2;
                Some(Token::Redirect(Redirect::ErrorToOutput))
            } else if chars.get(i) == Some(&'>') {
                i += 1;
                Some(Token::Redirect(Redirect::AppendError))
            } else {
                Some(Token::Redirect(Redirect::Error))
            }
        } else {
            match c {
                '|' if chars.get(i) == Some(&'|') => {
                    i += 1;
                    Some(Token::Link(Link::Failure))
                }
                '|' => Some(Token::Pipe),
                ';' | '\n' => Some(Token::Link(Link::Always)),
                '&' if chars.get(i) == Some(&'&') => {
                    i += 1;
                    Some(Token::Link(Link::Success))
                }
                '&' => return Err(error("use async NAME -- COMMAND for background work")),
                '>' if chars[i..].starts_with(&['&', '2']) => {
                    i += 2;
                    Some(Token::Redirect(Redirect::OutputToError))
                }
                '>' if chars.get(i) == Some(&'>') => {
                    i += 1;
                    Some(Token::Redirect(Redirect::Append))
                }
                '>' => Some(Token::Redirect(Redirect::Output)),
                '<' => Some(Token::Redirect(Redirect::Input)),
                _ => None,
            }
        };
        if op.is_some() || c.is_whitespace() {
            if inside {
                tokens.push(Token::Word(std::mem::take(&mut word)));
                inside = false;
            }
            if let Some(op) = op {
                tokens.push(op);
            }
        } else {
            literal(&mut word, c);
            inside = true;
        }
    }
    if quote.is_some() {
        return Err(error("unclosed quote"));
    }
    if inside {
        tokens.push(Token::Word(word));
    }
    Ok(tokens)
}
pub(super) fn parse(line: &str) -> io::Result<Program> {
    let mut tokens = lex(line)?.into_iter().peekable();
    let mut result = Vec::new();
    let mut link = Link::Always;
    while tokens.peek().is_some() {
        let mut pipeline = Vec::new();
        loop {
            let mut command = Command::default();
            loop {
                match tokens.peek() {
                    Some(Token::Word(_)) => {
                        let Some(Token::Word(w)) = tokens.next() else {
                            unreachable!()
                        };
                        command.words.push(w);
                    }
                    Some(Token::Redirect(_)) => {
                        let Some(Token::Redirect(r)) = tokens.next() else {
                            unreachable!()
                        };
                        let path = if matches!(r, Redirect::ErrorToOutput | Redirect::OutputToError)
                        {
                            Vec::new()
                        } else {
                            let Some(Token::Word(w)) = tokens.next() else {
                                return Err(error("redirection needs a path"));
                            };
                            w
                        };
                        command.redirects.push((r, path));
                    }
                    _ => break,
                }
            }
            if command.words.is_empty() && command.redirects.is_empty() {
                return Err(error("expected a command"));
            }
            pipeline.push(command);
            if !matches!(tokens.peek(), Some(Token::Pipe)) {
                break;
            }
            tokens.next();
        }
        result.push((link, pipeline));
        match tokens.next() {
            None => break,
            Some(Token::Link(next)) => {
                link = next;
                if link != Link::Always && tokens.peek().is_none() {
                    return Err(error("missing command after conditional operator"));
                }
            }
            _ => return Err(error("unexpected token")),
        }
    }
    Ok(result)
}

// Native control words occupy one statement. Quoted semicolons stay in their word;
// an explicit shell instruction owns the rest of its line, including backend syntax.
pub(super) fn statements(line: &str) -> Vec<&str> {
    let mut start = 0;
    let mut result = Vec::new();
    let mut quote = None;
    let mut escaped = false;
    for (i, c) in line.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if c == '\\' && quote != Some('\'') {
            escaped = true;
            continue;
        }
        if let Some(q) = quote {
            if c == q {
                quote = None;
            }
            continue;
        }
        if matches!(c, '\'' | '"') {
            quote = Some(c);
        } else if c == ';' {
            if line[start..].split_whitespace().next() == Some("shell") {
                break;
            }
            result.push(&line[start..i]);
            start = i + 1;
        } else if c == '#'
            && (i == start
                || line[..i]
                    .chars()
                    .next_back()
                    .is_some_and(char::is_whitespace))
        {
            result.push(&line[start..i]);
            return result;
        }
    }
    result.push(&line[start..]);
    result
}
