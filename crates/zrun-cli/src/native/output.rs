//! Output builtins write to the native shell's redirected descriptors.
use super::error;
use std::io::{self, Write};

fn escaped<I: Iterator<Item = char>>(
    chars: &mut std::iter::Peekable<I>,
    out: &mut Vec<u8>,
    percent_b: bool,
) -> bool {
    let Some(c) = chars.next() else {
        out.push(b'\\');
        return true;
    };
    match c {
        'a' => out.push(7),
        'b' => out.push(8),
        'e' | 'E' => out.push(27),
        'f' => out.push(12),
        'n' => out.push(10),
        'r' => out.push(13),
        't' => out.push(9),
        'v' => out.push(11),
        '\\' => out.push(b'\\'),
        'c' if percent_b => return false,
        '0'..='7' | 'x' => {
            let base = if c == 'x' { 16 } else { 8 };
            let leading = if c == 'x' || (c == '0' && percent_b) {
                None
            } else {
                c.to_digit(8)
            };
            let mut n = leading.unwrap_or(0);
            let mut used = 0;
            for _ in 0..if base == 16 || leading.is_some() {
                2
            } else {
                3
            } {
                let Some(digit) = chars.peek().and_then(|c| c.to_digit(base)) else {
                    break;
                };
                chars.next();
                n = n * base + digit;
                used += 1;
            }
            if c == 'x' && used == 0 {
                out.extend_from_slice(b"\\x");
            } else {
                out.push(n as u8);
            }
        }
        _ => {
            out.push(b'\\');
            out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
        }
    }
    true
}

fn text(value: &str, interpret: bool, output: &mut Vec<u8>) -> bool {
    if !interpret {
        output.extend_from_slice(value.as_bytes());
        return true;
    }
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if !escaped(&mut chars, output, true) {
                return false;
            }
        } else {
            output.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
        }
    }
    true
}

pub(super) fn print(name: &str, args: &[String], out: &mut impl Write) -> io::Result<u8> {
    let mut newline = true;
    let mut escapes = true;
    let mut separator = b' ';
    let mut end = b'\n';
    let mut rest = args;
    while let Some((word, tail)) = rest.split_first() {
        if name == "print" && matches!(word.as_str(), "--" | "-") {
            rest = tail;
            break;
        }
        if !word.starts_with('-') || word.len() < 2 {
            break;
        }
        let allowed = if name == "echo" { "neE" } else { "nerElN" };
        if !word[1..].chars().all(|c| allowed.contains(c)) {
            if name == "echo" {
                break;
            }
            return Err(error(format!("print: unsupported option {word}")));
        }
        for c in word[1..].chars() {
            match c {
                'n' => newline = false,
                'e' => escapes = true,
                'r' | 'E' => escapes = false,
                'l' => separator = b'\n',
                'N' => {
                    separator = 0;
                    end = 0;
                }
                _ => unreachable!(),
            }
        }
        rest = tail;
    }
    let mut buf = Vec::new();
    for (i, arg) in rest.iter().enumerate() {
        if i > 0 {
            out.write_all(&[separator])?;
        }
        let more = text(arg, escapes, &mut buf);
        out.write_all(&buf)?;
        buf.clear();
        if !more {
            return Ok(0);
        }
    }
    if newline {
        out.write_all(&[end])?;
    }
    Ok(0)
}
