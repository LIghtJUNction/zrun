use super::{State, error};
use std::{
    fs::File,
    io::{self, Write},
    path::{Path, PathBuf},
};

fn index(argument: &str, length: usize) -> io::Result<Option<usize>> {
    let Some(number) = argument.strip_prefix(['+', '-']) else {
        return Ok(None);
    };
    if number.is_empty() || !number.bytes().all(|byte| byte.is_ascii_digit()) {
        return Ok(None);
    }
    let n = number
        .parse::<usize>()
        .ok()
        .filter(|n| *n < length)
        .ok_or_else(|| error("directory stack index out of range"))?;
    Ok(Some(if argument.starts_with('-') {
        length - n - 1
    } else {
        n
    }))
}

impl State {
    pub(super) fn directory_stack(
        &mut self,
        cmd: &str,
        args: &[String],
        out: &mut File,
    ) -> io::Result<u8> {
        if cmd == "dirs" {
            if args == ["-c"] {
                self.dirs.clear();
                return Ok(0);
            }
            let mut full = false;
            let mut lines = false;
            let mut numbered = false;
            let mut rest = args;
            while let Some((opt, tail)) = rest.split_first() {
                if opt == "--" {
                    rest = tail;
                    break;
                }
                if !opt.starts_with('-') || opt == "-" {
                    break;
                }
                for flag in opt[1..].chars() {
                    match flag {
                        'l' => full = true,
                        'p' => lines = true,
                        'v' => {
                            numbered = true;
                            lines = true;
                        }
                        _ => return Err(error(format!("dirs: unknown flag -{flag}"))),
                    }
                }
                rest = tail;
            }
            if !rest.is_empty() {
                self.dirs = rest.iter().map(|name| self.cwd.join(name)).collect();
                return Ok(0);
            }
            let home = self.lookup("HOME");
            for (i, dir) in std::iter::once(&self.cwd).chain(&self.dirs).enumerate() {
                if i > 0 {
                    write!(out, "{}", if lines { "\n" } else { " " })?;
                }
                if numbered {
                    write!(out, "{i}\t")?;
                }
                let abbreviated = if full || home.is_empty() {
                    None
                } else {
                    dir.strip_prefix(&home).ok()
                };
                match abbreviated {
                    Some(path) if path.as_os_str().is_empty() => write!(out, "~")?,
                    Some(path) => write!(out, "~/{}", path.display())?,
                    None => write!(out, "{}", dir.display())?,
                }
            }
            writeln!(out)?;
            return Ok(0);
        }
        let rest = if args.first().is_some_and(|a| a == "--") {
            &args[1..]
        } else {
            args
        };
        if rest.len() > 1 {
            return Err(error(format!("{cmd}: expected at most one argument")));
        }
        let argument = rest.first();
        let idx = argument
            .map(|v| index(v, self.dirs.len() + 1))
            .transpose()?
            .flatten();
        if cmd == "pushd" {
            if let Some(i) = idx {
                if i != 0 {
                    let mut stack: Vec<PathBuf> = std::iter::once(self.cwd.clone())
                        .chain(self.dirs.iter().cloned())
                        .collect();
                    stack.rotate_left(i);
                    self.change_directory(&stack[0])?;
                    self.dirs = stack.split_off(1);
                }
            } else {
                let next = match argument.map(String::as_str) {
                    None | Some("~") => PathBuf::from(self.lookup("HOME")),
                    Some("-") => PathBuf::from(self.lookup("OLDPWD")),
                    Some(dir) => self.cwd.join(Path::new(dir)),
                };
                if next.as_os_str().is_empty() {
                    return Err(error("pushd: directory is unset"));
                }
                let old = self.cwd.clone();
                self.change_directory(&next)?;
                self.dirs.insert(0, old);
            }
        } else {
            let i = match (argument, idx) {
                (None, _) => 0,
                (_, Some(i)) => i,
                _ => return Err(error("popd: expected a stack index")),
            };
            if i == 0 {
                let next = self
                    .dirs
                    .first()
                    .ok_or_else(|| error("popd: directory stack empty"))?
                    .clone();
                self.change_directory(&next)?;
                self.dirs.remove(0);
            } else {
                self.dirs.remove(i - 1);
            }
        }
        Ok(0)
    }
}
