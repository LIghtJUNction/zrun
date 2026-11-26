//! POSIX test and bracket commands. File operands are relative to the session cwd.
use std::{
    ffi::CString,
    io::{self, Write},
    os::unix::{
        ffi::OsStrExt,
        fs::{FileTypeExt, MetadataExt},
    },
    path::Path,
};

fn access(path: &Path, mode: i32) -> bool {
    let Ok(name) = CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    // SAFETY: name is a live C string, and AT_EACCESS tests this process's effective credentials.
    unsafe { libc::faccessat(libc::AT_FDCWD, name.as_ptr(), mode, libc::AT_EACCESS) == 0 }
}

fn unary(cwd: &Path, op: &str, value: &str) -> Option<bool> {
    let path = cwd.join(value);
    let meta = || path.metadata().ok().filter(|_| !value.is_empty());
    Some(match op {
        "-n" => !value.is_empty(),
        "-z" => value.is_empty(),
        "-t" => value.parse::<i32>().is_ok_and(|fd| {
            // SAFETY: isatty checks an integer descriptor without taking ownership.
            unsafe { libc::isatty(fd) == 1 }
        }),
        "-a" | "-e" => meta().is_some(),
        "-f" => meta().is_some_and(|m| m.is_file()),
        "-d" => meta().is_some_and(|m| m.is_dir()),
        "-L" | "-h" => !value.is_empty() && path.is_symlink(),
        "-s" => meta().is_some_and(|m| m.len() > 0),
        "-r" | "-w" | "-x" => {
            !value.is_empty()
                && access(
                    &path,
                    match op {
                        "-r" => libc::R_OK,
                        "-w" => libc::W_OK,
                        _ => libc::X_OK,
                    },
                )
        }
        "-b" => meta().is_some_and(|m| m.file_type().is_block_device()),
        "-c" => meta().is_some_and(|m| m.file_type().is_char_device()),
        "-p" => meta().is_some_and(|m| m.file_type().is_fifo()),
        "-S" => meta().is_some_and(|m| m.file_type().is_socket()),
        "-u" | "-g" | "-k" => meta().is_some_and(|m| {
            m.mode()
                & match op {
                    "-u" => 0o4000,
                    "-g" => 0o2000,
                    _ => 0o1000,
                }
                != 0
        }),
        // SAFETY: geteuid and getegid have no preconditions or side effects.
        "-O" => meta().is_some_and(|m| m.uid() == unsafe { libc::geteuid() }),
        "-G" => meta().is_some_and(|m| m.gid() == unsafe { libc::getegid() }),
        _ => return None,
    })
}

fn binary(cwd: &Path, a: &str, op: &str, b: &str) -> Result<Option<bool>, String> {
    Ok(Some(match op {
        "=" | "==" => a == b,
        "!=" => a != b,
        "<" => a < b,
        ">" => a > b,
        "-eq" | "-ne" | "-lt" | "-le" | "-gt" | "-ge" => {
            let integer = |s: &str| {
                s.trim()
                    .parse::<i64>()
                    .map_err(|_| format!("invalid integer: {s}"))
            };
            let (x, y) = (integer(a)?, integer(b)?);
            match op {
                "-eq" => x == y,
                "-ne" => x != y,
                "-lt" => x < y,
                "-le" => x <= y,
                "-gt" => x > y,
                _ => x >= y,
            }
        }
        "-ef" | "-nt" | "-ot" => {
            let left = (!a.is_empty())
                .then(|| cwd.join(a).metadata().ok())
                .flatten();
            let right = (!b.is_empty())
                .then(|| cwd.join(b).metadata().ok())
                .flatten();
            match (left, right) {
                (Some(l), Some(r)) if op == "-ef" => l.dev() == r.dev() && l.ino() == r.ino(),
                (Some(l), Some(r)) => {
                    let x = (l.mtime(), l.mtime_nsec());
                    let y = (r.mtime(), r.mtime_nsec());
                    if op == "-nt" { x > y } else { x < y }
                }
                (Some(_), None) => op == "-nt",
                (None, Some(_)) => op == "-ot",
                _ => false,
            }
        }
        _ => return Ok(None),
    }))
}

fn expression(cwd: &Path, args: &[String], depth: usize) -> Result<bool, String> {
    match args {
        [] => return Ok(false),
        [word] => return Ok(!word.is_empty()),
        [op, value] if op == "!" => return Ok(value.is_empty()),
        [op, value] => {
            return unary(cwd, op, value).ok_or_else(|| format!("unknown operator: {op}"));
        }
        [a, op, b] => {
            if let Some(result) = binary(cwd, a, op, b)? {
                return Ok(result);
            }
        }
        _ => {}
    }
    if depth > 128 {
        return Err("expression nesting limit exceeded".into());
    }
    for op in ["-o", "-a"] {
        let mut parens = 0;
        for (i, token) in args.iter().enumerate() {
            if token == "(" {
                parens += 1;
            } else if token == ")" {
                parens -= 1;
            } else if token == op && parens == 0 && i > 0 && i + 1 < args.len() {
                let l = expression(cwd, &args[..i], depth + 1)?;
                let r = expression(cwd, &args[i + 1..], depth + 1)?;
                return Ok(if op == "-o" { l || r } else { l && r });
            }
        }
    }
    if args.first().is_some_and(|s| s == "!") {
        return Ok(!expression(cwd, &args[1..], depth + 1)?);
    }
    if args.first().is_some_and(|s| s == "(") && args.last().is_some_and(|s| s == ")") {
        return expression(cwd, &args[1..args.len() - 1], depth + 1);
    }
    Err("invalid test expression".into())
}

pub(super) fn test(
    name: &str,
    args: &[String],
    cwd: &Path,
    err: &mut impl Write,
) -> io::Result<u8> {
    let args = if name == "[" {
        let Some((end, rest)) = args.split_last().filter(|(end, _)| *end == "]") else {
            writeln!(err, "[: missing ]")?;
            return Ok(2);
        };
        let _ = end;
        rest
    } else {
        args
    };
    match expression(cwd, args, 0) {
        Ok(true) => Ok(0),
        Ok(false) => Ok(1),
        Err(e) => {
            writeln!(err, "{name}: {e}")?;
            Ok(2)
        }
    }
}
