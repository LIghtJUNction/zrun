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

fn integer(arg: &str) -> Option<i64> {
    if arg.is_empty() {
        return Some(0);
    }
    if let Some(s) = arg.strip_prefix(['\'', '"']) {
        return Some(s.chars().next().unwrap_or('\0') as i64);
    }
    let s = arg.trim();
    let negative = s.starts_with('-');
    let s = s.strip_prefix(['+', '-']).unwrap_or(s);
    let (base, digits) = if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        (16, hex)
    } else if s.len() > 1 && s.starts_with('0') {
        (8, s)
    } else {
        (10, s)
    };
    let magnitude = i128::from_str_radix(digits, base).ok()?;
    i64::try_from(if negative { -magnitude } else { magnitude }).ok()
}

fn spaces(out: &mut impl Write, mut n: usize) -> io::Result<()> {
    let block = [b' '; 256];
    while n > 0 {
        let count = n.min(block.len());
        out.write_all(&block[..count])?;
        n -= count;
    }
    Ok(())
}

pub(super) fn printf(
    args: &[String],
    out: &mut std::fs::File,
    err: &mut std::fs::File,
) -> io::Result<u8> {
    use std::{ffi::CString, os::fd::AsRawFd};
    unsafe extern "C" {
        fn dprintf(fd: libc::c_int, fmt: *const libc::c_char, ...) -> libc::c_int;
    }
    let args = if args.first().is_some_and(|a| a == "--") {
        &args[1..]
    } else {
        args
    };
    let Some((fmt, values)) = args.split_first() else {
        writeln!(err, "printf: expected a format")?;
        return Ok(1);
    };
    let mut remaining = values.iter().map(String::as_str).peekable();
    let mut status = 0;
    loop {
        let before = remaining.len();
        let mut chars = fmt.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\\' {
                let mut bytes = Vec::with_capacity(4);
                escaped(&mut chars, &mut bytes, false);
                out.write_all(&bytes)?;
                continue;
            }
            if c != '%' {
                out.write_all(c.encode_utf8(&mut [0; 4]).as_bytes())?;
                continue;
            }
            if chars.peek() == Some(&'%') {
                chars.next();
                out.write_all(b"%")?;
                continue;
            }
            let mut flags = String::new();
            while let Some(c) = chars.next_if(|c| "-+ #0".contains(*c)) {
                flags.push(c);
            }
            let mut field = || -> io::Result<Option<i32>> {
                if chars.next_if_eq(&'*').is_some() {
                    let v = remaining.next().unwrap_or("0");
                    return v
                        .parse::<i32>()
                        .map(Some)
                        .map_err(|_| error(format!("printf: invalid field width {v}")));
                }
                let n: String =
                    std::iter::from_fn(|| chars.next_if(char::is_ascii_digit)).collect();
                if n.is_empty() {
                    Ok(None)
                } else {
                    n.parse()
                        .map(Some)
                        .map_err(|_| error("printf: field width too large"))
                }
            };
            let width = field()?;
            // Recreate the small field parser for precision after consuming the dot.
            let precision = if chars.next_if_eq(&'.').is_some() {
                if chars.next_if_eq(&'*').is_some() {
                    let s = remaining.next().unwrap_or("0");
                    let value = s
                        .parse::<i32>()
                        .map_err(|_| error("printf: invalid precision"))?;
                    (value >= 0).then_some(value)
                } else {
                    let n: String =
                        std::iter::from_fn(|| chars.next_if(char::is_ascii_digit)).collect();
                    Some(if n.is_empty() {
                        0
                    } else {
                        n.parse()
                            .map_err(|_| error("printf: precision too large"))?
                    })
                }
            } else {
                None
            };
            let Some(conv) = chars.next() else {
                writeln!(err, "printf: incomplete format")?;
                return Ok(1);
            };
            let val = remaining.next().unwrap_or("");
            if matches!(conv, 's' | 'b' | 'c' | 'q') {
                let mut bytes = Vec::new();
                let more = match conv {
                    'b' => text(val, true, &mut bytes),
                    'c' => {
                        if let Some(c) = val.chars().next() {
                            bytes.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
                        }
                        true
                    }
                    'q' => {
                        if val.is_empty() {
                            bytes.extend_from_slice(b"''");
                        }
                        for c in val.chars() {
                            if !c.is_ascii_alphanumeric() && !"_/.:-".contains(c) {
                                bytes.push(b'\\');
                            }
                            bytes.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes());
                        }
                        true
                    }
                    _ => {
                        bytes.extend_from_slice(val.as_bytes());
                        true
                    }
                };
                if let Some(p) = precision {
                    bytes.truncate(p as usize);
                }
                let width = width.unwrap_or(0);
                let left = width < 0 || flags.contains('-');
                let pad = (width.unsigned_abs() as usize).saturating_sub(bytes.len());
                if !left {
                    spaces(out, pad)?;
                }
                out.write_all(&bytes)?;
                if left {
                    spaces(out, pad)?;
                }
                if !more {
                    return Ok(status);
                }
                continue;
            }
            if !"diouxXfFeEgGaA".contains(conv) {
                writeln!(err, "printf: unsupported conversion %{conv}")?;
                return Ok(1);
            }
            let mut spec = format!("%{flags}");
            if let Some(w) = width {
                spec.push_str(&w.to_string());
            }
            if let Some(p) = precision {
                spec.push('.');
                spec.push_str(&p.to_string());
            }
            let integral = "diouxX".contains(conv);
            if integral {
                spec.push_str("ll");
            }
            spec.push(conv);
            let spec = CString::new(spec).expect("validated ASCII format");
            let fd = out.as_raw_fd();
            let ret = if integral {
                let n = integer(val).unwrap_or_else(|| {
                    status = 1;
                    0
                });
                // SAFETY: spec is validated to contain exactly one matching ll integer conversion.
                unsafe {
                    if matches!(conv, 'd' | 'i') {
                        dprintf(fd, spec.as_ptr(), n as libc::c_longlong)
                    } else {
                        dprintf(fd, spec.as_ptr(), n as libc::c_ulonglong)
                    }
                }
            } else {
                let n = if val.is_empty() {
                    0.0
                } else {
                    val.parse::<f64>().unwrap_or_else(|_| {
                        status = 1;
                        0.0
                    })
                };
                // SAFETY: spec contains exactly one validated floating-point conversion, passed as double.
                unsafe { dprintf(fd, spec.as_ptr(), n as libc::c_double) }
            };
            if ret < 0 {
                return Err(io::Error::last_os_error());
            }
        }
        if before == remaining.len() || remaining.peek().is_none() {
            break;
        }
    }
    if status != 0 {
        writeln!(err, "printf: invalid numeric argument")?;
    }
    Ok(status)
}
