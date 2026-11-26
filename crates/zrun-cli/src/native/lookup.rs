//! Describe commands using the same session directory, aliases and PATH as dispatch.
use super::{
    BUILTINS, complete,
    execute::{State, Streams},
};
use std::{
    ffi::OsStr,
    io::{self, Write},
    path::{Path, PathBuf},
};

const STANDARD_PATH: &str = "/bin:/usr/bin";

pub(super) fn paths<'a>(
    state: &'a State,
    name: &'a str,
    standard: bool,
) -> impl Iterator<Item = PathBuf> + 'a {
    let path = if standard {
        OsStr::new(STANDARD_PATH)
    } else {
        state
            .env
            .get(OsStr::new("PATH"))
            .map_or(OsStr::new(STANDARD_PATH), |s| s.as_os_str())
    };
    let explicit = name.contains('/');
    let direct = explicit.then(|| PathBuf::from(name));
    direct
        .into_iter()
        .chain(
            std::env::split_paths(path)
                .take_while(move |_| !explicit)
                .map(move |dir| {
                    if dir.as_os_str().is_empty() {
                        Path::new(".").join(name)
                    } else {
                        dir.join(name)
                    }
                }),
        )
        .filter(|candidate| {
            let file = state.cwd.join(candidate);
            file.is_file() && complete::executable(&file)
        })
}

#[derive(Clone, Copy, PartialEq)]
enum Format {
    Plain,
    Verbose,
    Csh,
    Kind,
    Command,
}

pub(super) fn is_query(name: &str) -> bool {
    matches!(name, "type" | "whence" | "where" | "which" | "command")
}

pub(super) fn describe(state: &State, argv: &[String], streams: &mut Streams) -> io::Result<u8> {
    let (cmd, mut names) = argv.split_first().expect("command query has a name");
    let command = cmd == "command";
    let mut format = match cmd.as_str() {
        "type" | "command" => Format::Verbose,
        "which" | "where" => Format::Csh,
        _ => Format::Plain,
    };
    let mut all = cmd == "where";
    let mut external = false;
    let mut standard = false;
    while let Some((option, rest)) = names.split_first() {
        if option == "--" {
            names = rest;
            break;
        }
        if !option.starts_with('-') || option == "-" {
            break;
        }
        for flag in option[1..].chars() {
            match (command, flag) {
                (true, 'p') => standard = true,
                (true, 'v') => format = Format::Command,
                (true, 'V') => format = Format::Verbose,
                (false, 'a') => all = true,
                (false, 'p') => external = true,
                (false, 'v') if !matches!(format, Format::Kind | Format::Csh) => {
                    format = Format::Verbose
                }
                (false, 'c') if format != Format::Kind => format = Format::Csh,
                (false, 'w') => format = Format::Kind,
                (false, 'v' | 'c' | 'f') => {}
                _ => {
                    writeln!(streams.err, "{cmd}: invalid option -{flag}")?;
                    return Ok(2);
                }
            }
        }
        names = rest;
    }
    let mut status = 0;
    for name in names {
        let mut found = false;
        if !external && !name.contains('/') {
            if let Some((body, _)) = state.aliases.get(name) {
                found = true;
                match format {
                    Format::Verbose => writeln!(streams.out, "{name} is an alias for {body}")?,
                    Format::Csh => writeln!(streams.out, "{name}: aliased to {body}")?,
                    Format::Kind => writeln!(streams.out, "{name}: alias")?,
                    Format::Command => writeln!(
                        streams.out,
                        "alias {name}='{}'",
                        body.replace('\'', "'\\''")
                    )?,
                    Format::Plain => writeln!(streams.out, "{body}")?,
                }
                if !all {
                    continue;
                }
            }
            if BUILTINS.contains(&name.as_str()) {
                found = true;
                match format {
                    Format::Verbose => writeln!(streams.out, "{name} is a shell builtin")?,
                    Format::Csh => writeln!(streams.out, "{name}: shell built-in command")?,
                    Format::Kind => writeln!(streams.out, "{name}: builtin")?,
                    Format::Command | Format::Plain => writeln!(streams.out, "{name}")?,
                }
                if !all {
                    continue;
                }
            }
        }
        for path in paths(state, name, standard) {
            found = true;
            match format {
                Format::Verbose => writeln!(streams.out, "{name} is {}", path.display())?,
                Format::Kind => writeln!(streams.out, "{name}: command")?,
                _ => writeln!(streams.out, "{}", path.display())?,
            }
            if !all {
                break;
            }
        }
        if !found {
            status = 1;
            match format {
                Format::Verbose | Format::Csh => writeln!(streams.out, "{name} not found")?,
                Format::Kind => writeln!(streams.out, "{name}: none")?,
                _ => {}
            }
        }
    }
    Ok(status)
}
