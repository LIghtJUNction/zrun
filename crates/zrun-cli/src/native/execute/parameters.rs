use super::{State, error, syntax};
use std::{
    collections::BTreeMap,
    ffi::OsStr,
    fs::File,
    io::{self, Write},
};

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

impl State {
    pub(super) fn set_parameters(&mut self, args: &[String], out: &mut File) -> io::Result<u8> {
        if args.is_empty() {
            let mut vars: BTreeMap<_, _> = self
                .env
                .iter()
                .filter_map(|(k, v)| Some((k.to_str()?, v.to_str()?)))
                .collect();
            vars.extend(self.locals.iter().map(|(k, v)| (k.as_str(), v.as_str())));
            for (key, value) in vars {
                writeln!(out, "{key}={}", quote(value))?;
            }
            return Ok(0);
        }
        let mut rest = args;
        while let Some((word, tail)) = rest.split_first() {
            if word == "--" {
                rest = tail;
                break;
            }
            if !(word.starts_with('-') || word.starts_with('+')) || word.len() == 1 {
                break;
            }
            let enabled = word.starts_with('-');
            let flags = word[1..].chars();
            rest = tail;
            for flag in flags {
                match flag {
                    'a' => self.all_export = enabled,
                    'e' => self.errexit = enabled,
                    'u' => self.nounset = enabled,
                    'o' => {
                        let Some((name, remaining)) = rest.split_first() else {
                            return Err(error("set: expected an option name"));
                        };
                        match name.as_str() {
                            "allexport" => self.all_export = enabled,
                            "errexit" => self.errexit = enabled,
                            "pipefail" => self.pipefail = enabled,
                            "nounset" => self.nounset = enabled,
                            _ => return Err(error(format!("set: unsupported option {name}"))),
                        }
                        rest = remaining;
                    }
                    _ => return Err(error(format!("set: unsupported option {flag}"))),
                }
            }
            if rest.is_empty() {
                return Ok(0);
            }
        }
        self.positional.truncate(1);
        self.positional.extend(rest.iter().map(Into::into));
        Ok(0)
    }

    pub(super) fn shift_parameters(&mut self, args: &[String]) -> io::Result<u8> {
        let how_many = match args {
            [] => 1,
            [count] => count
                .parse::<usize>()
                .map_err(|_| error("shift: expected a nonnegative count"))?,
            _ => return Err(error("usage: shift [COUNT]")),
        };
        if how_many > self.positional.len().saturating_sub(1) {
            return Err(error("shift: not enough positional parameters"));
        }
        self.positional.drain(1..1 + how_many);
        Ok(0)
    }

    pub(super) fn declare_variables(
        &mut self,
        kind: &str,
        args: &[String],
        out: &mut File,
    ) -> io::Result<u8> {
        let mut export = kind == "export";
        let mut readonly = kind == "readonly";
        let mut rest = args;
        let mut print = false;
        while let Some((word, tail)) = rest.split_first() {
            if word == "--" {
                rest = tail;
                break;
            }
            if !word.starts_with('-') || word.len() == 1 {
                break;
            }
            for flag in word[1..].chars() {
                match flag {
                    'p' => print = true,
                    'r' => readonly = true,
                    'x' => export = true,
                    _ => return Err(error(format!("{kind}: unsupported option {flag}"))),
                }
            }
            rest = tail;
        }
        if rest.is_empty() || print {
            let mut names: std::collections::BTreeSet<_> =
                self.env.keys().filter_map(|k| k.to_str()).collect();
            names.extend(self.locals.keys().map(String::as_str));
            if rest.is_empty() {
                for key in names {
                    if (!export || self.env.contains_key(OsStr::new(key)))
                        && (!readonly || self.readonly.contains(key))
                    {
                        writeln!(out, "{kind} {key}={}", quote(&self.lookup(key)))?;
                    }
                }
            } else {
                for key in rest {
                    if !names.contains(key.as_str()) {
                        return Err(error(format!("{kind}: unknown parameter {key}")));
                    }
                    writeln!(out, "{kind} {key}={}", quote(&self.lookup(key)))?;
                }
            }
            return Ok(0);
        }
        for entry in rest {
            let (key, value) = entry
                .split_once('=')
                .map_or((entry.as_str(), None), |(k, v)| (k, Some(v)));
            if !syntax::identifier(key) {
                return Err(error(format!("{kind}: invalid variable name")));
            }
            if let Some(value) = value {
                self.assign(key, value.to_owned(), export)?;
            } else {
                let value = self.lookup(key);
                self.locals
                    .entry(key.to_owned())
                    .or_insert_with(|| value.clone());
                if export || self.all_export {
                    self.env.insert(key.into(), value.into());
                }
            }
            if readonly {
                self.readonly.insert(key.to_owned());
            }
        }
        Ok(0)
    }
}
