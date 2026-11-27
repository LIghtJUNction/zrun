mod directories;
mod parameters;
mod script;
use super::{
    Interrupt, error, read,
    syntax::{self, Link, Part, Program, Redirect, Word},
};
use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    fs::{File, OpenOptions},
    io::{self, Write},
    os::{
        fd::{AsFd, OwnedFd},
        unix::process::{CommandExt, ExitStatusExt},
    },
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
};

#[derive(Clone)]
pub(super) struct State {
    pub cwd: PathBuf,
    dirs: Vec<PathBuf>,
    pub env: BTreeMap<OsString, OsString>,
    locals: BTreeMap<String, String>,
    readonly: std::collections::BTreeSet<String>,
    all_export: bool,
    errexit: bool,
    ignore_errexit: bool,
    pipefail: bool,
    nounset: bool,
    pub(super) expansion_exit: Option<u8>,
    pub(super) line_number: usize,
    pub(super) aliases: BTreeMap<String, (String, Vec<Word>)>,
    pub status: u8,
    pub exit: Option<u8>,
    returning: Option<u8>,
    source_depth: usize,
    eval_depth: usize,
    pub positional: Vec<OsString>,
    pub terminal_handoff: Option<super::terminal_resume::Handoff>,
}
#[derive(Debug)]
struct UnboundVariable(String);

impl std::fmt::Display for UnboundVariable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: unbound variable", self.0)
    }
}
impl std::error::Error for UnboundVariable {}

impl State {
    pub fn new() -> io::Result<Self> {
        let cwd = std::env::current_dir()?;
        let mut env: BTreeMap<_, _> = std::env::vars_os().collect();
        env.insert("SHELL".into(), std::env::current_exe()?.into_os_string());
        env.insert("PWD".into(), cwd.as_os_str().into());
        Ok(Self {
            cwd,
            dirs: Vec::new(),
            env,
            locals: BTreeMap::new(),
            aliases: BTreeMap::new(),
            readonly: Default::default(),
            all_export: false,
            errexit: false,
            ignore_errexit: false,
            pipefail: false,
            nounset: false,
            expansion_exit: Some(1),
            line_number: 1,
            status: 0,
            exit: None,
            returning: None,
            source_depth: 0,
            eval_depth: 0,
            positional: vec!["zrun".into()],
            terminal_handoff: None,
        })
    }
    fn lookup(&self, k: &str) -> String {
        self.value(k).unwrap_or_default()
    }
    fn value(&self, k: &str) -> Option<String> {
        match k {
            "?" => Some(self.status.to_string()),
            "#" => Some(self.positional.len().saturating_sub(1).to_string()),
            "$" => Some(std::process::id().to_string()),
            "@" => Some(
                self.positional
                    .iter()
                    .skip(1)
                    .map(|v| v.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
            k if k.bytes().all(|b| b.is_ascii_digit()) => k
                .parse::<usize>()
                .ok()
                .and_then(|index| self.positional.get(index))
                .map(|s| s.to_string_lossy().into_owned()),
            _ => self.locals.get(k).cloned().or_else(|| {
                self.env
                    .get(OsStr::new(k))
                    .map(|v| v.to_string_lossy().into_owned())
            }),
        }
    }
    fn expand(&self, name: &str) -> io::Result<String> {
        match self.value(name) {
            Some(value) => Ok(value),
            None if self.nounset => Err(io::Error::other(UnboundVariable(name.to_owned()))),
            None => Ok(String::new()),
        }
    }
    fn word(&self, word: &Word) -> io::Result<String> {
        let mut expanded = String::new();
        for part in word {
            match part {
                Part::Text(text) => expanded.push_str(text),
                Part::Var(name) => expanded.push_str(&self.expand(name)?),
                Part::Default {
                    name,
                    value,
                    if_empty,
                } => {
                    let found = self.value(name).filter(|v| !if_empty || !v.is_empty());
                    expanded.push_str(found.as_deref().unwrap_or(value));
                }
                Part::Trim { name, suffix } => {
                    let value = self.expand(name)?;
                    expanded.push_str(value.strip_suffix(suffix).unwrap_or(&value));
                }
            }
        }
        Ok(expanded)
    }
    fn report_unbound(&self, error: &io::Error, err: &File) -> io::Result<()> {
        writeln!(
            &*err,
            "{}: line {}: {error}",
            self.positional[0].to_string_lossy(),
            self.line_number
        )
    }
    fn assign(&mut self, k: &str, v: String, export: bool) -> io::Result<()> {
        if self.readonly.contains(k) {
            return Err(error(format!("{k}: readonly variable")));
        }
        if self.all_export || export || self.env.contains_key(OsStr::new(k)) {
            self.env.insert(k.into(), v.clone().into());
        }
        self.locals.insert(k.into(), v);
        Ok(())
    }
    fn change_directory(&mut self, arg: &Path) -> io::Result<()> {
        let next = self.cwd.join(arg).canonicalize()?;
        if !next.is_dir() {
            return Err(error("cd: not a directory"));
        }
        self.env
            .insert("OLDPWD".into(), self.cwd.as_os_str().into());
        self.env.insert("PWD".into(), next.as_os_str().into());
        self.locals.remove("PWD");
        self.locals.remove("OLDPWD");
        self.cwd = next;
        Ok(())
    }
    pub(super) fn auto_cd(&mut self, line: &str, program: &Program) -> io::Result<bool> {
        let [(Link::Always, pipeline)] = program.as_slice() else {
            return Ok(false);
        };
        let [command] = pipeline.as_slice() else {
            return Ok(false);
        };
        let Some(first) = command.words.first() else {
            return Ok(false);
        };
        if command.negated || !command.redirects.is_empty() {
            return Ok(false);
        }
        let Ok(name) = self.word(first) else {
            return Ok(false);
        };
        if name.is_empty()
            || super::BUILTINS.contains(&name.as_str())
            || self.aliases.contains_key(&name)
            || name
                .split_once('=')
                .is_some_and(|(key, _)| syntax::identifier(key))
        {
            return Ok(false);
        }
        // An unquoted pasted directory may contain spaces. Take the complete literal
        // line only when every parsed word is unchanged; never reinterpret shell syntax.
        let candidate = if command.words.len() == 1 {
            name.as_str()
        } else if line
            .split_whitespace()
            .map(Some)
            .eq(command.words.iter().map(|word| match word.as_slice() {
                [Part::Text(text)] => Some(text.as_str()),
                _ => None,
            }))
        {
            line
        } else {
            return Ok(false);
        };
        let directory = self.cwd.join(candidate);
        if !directory.is_dir() {
            return Ok(false);
        }
        if super::lookup::paths(self, &name, false).next().is_some() {
            return Ok(false);
        }
        self.change_directory(&directory)?;
        Ok(true)
    }
    fn builtin(
        &mut self,
        argv: &[String],
        streams: &mut Streams,
        control: &Arc<Control>,
        tty: bool,
        signal: Option<&Interrupt>,
    ) -> io::Result<Option<u8>> {
        let Some((name, args)) = argv.split_first() else {
            return Ok(Some(0));
        };
        let out = &mut streams.out;
        let code = match name.as_str() {
            query if super::lookup::is_query(query) => {
                super::lookup::describe(self, argv, streams)?
            }
            "printf" => super::output::printf(args, out, &mut streams.err)?,
            "echo" | "print" => super::output::print(name, args, out)?,
            "test" | "[" => super::condition::test(name, args, &self.cwd, &mut streams.err)?,
            ":" | "true" => 0,
            "false" => 1,
            "." | "source" | "eval" => self.evaluate(argv, streams, control, tty, signal)?,
            "exit" | "bye" | "return" => {
                if args.len() > 1 {
                    return Err(error("exit [STATUS]"));
                }
                let code = match args.first() {
                    None => self.status,
                    Some(n) => n
                        .parse::<u8>()
                        .map_err(|_| error("exit needs a status in 0..255"))?,
                };
                if name == "return" && self.source_depth > 0 {
                    self.returning = Some(code);
                } else {
                    self.exit = Some(code);
                }
                code
            }
            "dirs" | "pushd" | "popd" => self.directory_stack(name, args, out)?,
            "cd" | "chdir" => {
                if args.len() > 1 {
                    return Err(error("cd [DIRECTORY]"));
                }
                let arg = match args.first().map(String::as_str) {
                    None | Some("~") => self.lookup("HOME"),
                    Some("-") => self.lookup("OLDPWD"),
                    Some(s) => s.into(),
                };
                if arg.is_empty() {
                    return Err(error("cd: directory is unset"));
                }
                self.change_directory(Path::new(&arg))?;
                0
            }
            "pwd" => {
                writeln!(out, "{}", self.cwd.display())?;
                0
            }
            "alias" => {
                let display = |out: &mut File, name: &str, body: &str| {
                    writeln!(out, "alias {name}='{}'", body.replace('\'', "'\\''"))
                };
                if args.is_empty() {
                    for (name, (body, _)) in &self.aliases {
                        display(out, name, body)?;
                    }
                }
                for arg in args {
                    let Some((key, body)) = arg.split_once('=') else {
                        let (body, _) = self
                            .aliases
                            .get(arg)
                            .ok_or_else(|| error(format!("alias: unknown name {arg}")))?;
                        display(out, arg, body)?;
                        continue;
                    };
                    if key.is_empty()
                        || key.starts_with('-')
                        || !key
                            .chars()
                            .all(|c| c.is_alphanumeric() || "_+.-".contains(c))
                    {
                        return Err(error("invalid alias name"));
                    }
                    let program = syntax::parse(body)?;
                    let [(Link::Always, pipeline)] = program.as_slice() else {
                        return Err(error("alias needs one command"));
                    };
                    let [command] = pipeline.as_slice() else {
                        return Err(error("alias needs one command"));
                    };
                    if command.words.is_empty() || !command.redirects.is_empty() {
                        return Err(error("alias needs a simple command"));
                    }
                    self.aliases
                        .insert(key.into(), (body.into(), command.words.clone()));
                }
                0
            }
            "unalias" => {
                for name in args {
                    self.aliases.remove(name);
                }
                0
            }
            "read" => {
                let result = read::line(args, &mut streams.input, &mut streams.err, signal)?;
                if let Some((name, value)) = result.assignment {
                    self.assign(name, value, false)?;
                }
                result.status
            }
            "set" => self.set_parameters(args, out)?,
            "shift" => self.shift_parameters(args)?,
            "readonly" | "export" | "typeset" | "declare" => {
                self.declare_variables(name, args, out)?
            }
            "unset" => {
                for entry in args {
                    if !syntax::identifier(entry) || self.readonly.contains(entry) {
                        return Err(error(format!("unset: cannot unset {entry}")));
                    }
                    self.locals.remove(entry);
                    self.env.remove(OsStr::new(entry));
                }
                0
            }
            _ => return Ok(None),
        };
        Ok(Some(code))
    }
}
pub(super) struct Streams {
    pub input: File,
    pub out: File,
    pub err: File,
}
impl Streams {
    pub fn terminal() -> io::Result<Self> {
        Ok(Self {
            input: io::stdin().as_fd().try_clone_to_owned()?.into(),
            out: io::stdout().as_fd().try_clone_to_owned()?.into(),
            err: io::stderr().as_fd().try_clone_to_owned()?.into(),
        })
    }
    fn copy(&self) -> io::Result<Self> {
        Ok(Self {
            input: self.input.try_clone()?,
            out: self.out.try_clone()?,
            err: self.err.try_clone()?,
        })
    }
    fn redirect(&mut self, op: Redirect, path: &Path) -> io::Result<()> {
        match op {
            Redirect::Input => self.input = File::open(path)?,
            Redirect::ErrorToOutput => self.err = self.out.try_clone()?,
            Redirect::OutputToError => self.out = self.err.try_clone()?,
            _ => {
                let append = matches!(op, Redirect::Append | Redirect::AppendError);
                let file = OpenOptions::new()
                    .write(true)
                    .create(true)
                    .truncate(!append)
                    .append(append)
                    .open(path)?;
                if matches!(op, Redirect::Error | Redirect::AppendError) {
                    self.err = file;
                } else {
                    self.out = file;
                }
            }
        };
        Ok(())
    }
}

#[derive(Default)]
pub(super) struct Control {
    active: Mutex<(bool, i32)>,
    background: bool,
}
impl Control {
    pub fn background() -> Self {
        Self {
            background: true,
            ..Self::default()
        }
    }
    pub fn cancel(&self) {
        let mut c = self.active.lock().unwrap();
        c.0 = true;
        if c.1 > 0 {
            signal_group(c.1, libc::SIGKILL);
        }
    }
    fn active(&self, pid: i32) -> io::Result<()> {
        let mut c = self.active.lock().unwrap();
        c.1 = pid;
        if c.0 {
            if pid > 0 {
                signal_group(pid, libc::SIGKILL);
            }
            Err(io::Error::new(io::ErrorKind::Interrupted, "task cancelled"))
        } else {
            Ok(())
        }
    }
}
fn signal_group(id: i32, sig: i32) {
    if id > 0 {
        // SAFETY: only the process group owned by this pipeline is addressed.
        unsafe {
            libc::kill(-id, sig);
        }
    }
}
fn foreground(id: i32) {
    // SAFETY: temporarily blocking SIGTTOU lets the shell restore the foreground terminal from a background group.
    unsafe {
        let mut set = std::mem::zeroed();
        let mut old = std::mem::zeroed();
        libc::sigemptyset(&mut set);
        libc::sigaddset(&mut set, libc::SIGTTOU);
        if libc::pthread_sigmask(libc::SIG_BLOCK, &set, &mut old) == 0 {
            libc::tcsetpgrp(0, id);
            libc::pthread_sigmask(libc::SIG_SETMASK, &old, std::ptr::null_mut());
        }
    }
}
struct Pipeline<'a> {
    children: Vec<(usize, Child)>,
    group: i32,
    restore: Option<i32>,
    control: &'a Control,
}
impl Drop for Pipeline<'_> {
    fn drop(&mut self) {
        if !self.children.is_empty() {
            signal_group(self.group, libc::SIGKILL);
            for (_, c) in &mut self.children {
                if self.group == 0 {
                    let _ = c.kill();
                }
                let _ = c.wait();
            }
        }
        let _ = self.control.active(0);
        if let Some(group) = self.restore {
            foreground(group);
        }
    }
}
fn status(e: std::process::ExitStatus) -> u8 {
    e.code().unwrap_or_else(|| 128 + e.signal().unwrap_or(0)) as u8
}
fn missing(e: &io::Error) -> u8 {
    if e.kind() == io::ErrorKind::NotFound {
        127
    } else {
        126
    }
}

fn pipeline(
    state: &mut State,
    cmds: &[syntax::Command],
    base: &Streams,
    control: &Arc<Control>,
    tty: bool,
    signal: Option<&Interrupt>,
) -> io::Result<u8> {
    control.active(0)?;
    let mut pipeline = Pipeline {
        children: Vec::new(),
        group: 0,
        restore: None,
        control,
    };
    let mut writers = Vec::new();
    let mut statuses = vec![0; cmds.len()];
    let pipefail = state.pipefail;
    let mut input = None;
    let mut exits = false;
    // Noninteractive foreground commands inherit the caller's process group so
    // interrupts and hangups delivered to a script also reach its children.
    let owns_group = tty || control.background;
    for (i, ast) in cmds.iter().enumerate() {
        let last = i + 1 == cmds.len();
        let mut streams = base.copy()?;
        if let Some(fd) = input.take() {
            streams.input = fd;
        }
        if !last {
            let (r, w) = io::pipe()?;
            input = Some(File::from(OwnedFd::from(r)));
            streams.out = File::from(OwnedFd::from(w));
        }
        let n = ast
            .words
            .iter()
            .take_while(|word| {
                let Some(Part::Text(prefix)) = word.first() else {
                    return false;
                };
                prefix
                    .split_once('=')
                    .is_some_and(|(name, _)| syntax::identifier(name))
            })
            .count();
        let mut expand = || -> io::Result<Vec<String>> {
            let mut words = Vec::new();
            for w in &ast.words {
                if matches!(w.as_slice(), [Part::Var(name)] if name == "@") {
                    words.extend(
                        state
                            .positional
                            .iter()
                            .skip(1)
                            .map(|v| v.to_string_lossy().into_owned()),
                    );
                } else {
                    words.push(state.word(w)?);
                }
            }
            // Explicit command/exec bypass aliases. Expand alias arguments before
            // opening any redirection, just like the original command arguments.
            if let Some(name) = words.get(n)
                && !matches!(name.as_str(), "command" | "builtin" | "exec")
                && let Some((_, definition)) = state.aliases.get(name)
            {
                let expansion = definition
                    .iter()
                    .map(|word| state.word(word))
                    .collect::<io::Result<Vec<_>>>()?;
                words.splice(n..=n, expansion);
            }
            for (op, path) in &ast.redirects {
                streams.redirect(*op, &state.cwd.join(state.word(path)?))?;
            }
            Ok(words)
        };
        let words = match expand() {
            Ok(expanded) => expanded,
            Err(error)
                if cmds.len() > 1 && error.get_ref().is_some_and(|e| e.is::<UnboundVariable>()) =>
            {
                state.report_unbound(&error, &streams.err)?;
                statuses[i] = state.expansion_exit.unwrap_or(1);
                continue;
            }
            Err(error) => return Err(error),
        };
        let assignments = words[..n]
            .iter()
            .map(|word| word.split_once('=').expect("validated assignment"));
        let mut args = &words[n..];
        let replaces_shell = args.first().is_some_and(|s| s == "exec");
        let mut standard_path = false;
        let mut builtin_only = false;
        loop {
            match args.split_first() {
                Some((name, rest)) if name == "builtin" => {
                    builtin_only = true;
                    args = if rest.first().is_some_and(|v| v == "--") {
                        &rest[1..]
                    } else {
                        rest
                    };
                }
                Some((name, rest)) if name == "command" => {
                    let flags = rest
                        .iter()
                        .take_while(|v| v.starts_with('-') && v.as_str() != "--");
                    if flags.clone().any(|v| v.bytes().skip(1).any(|b| b != b'p')) {
                        break;
                    }
                    let nflags = flags.count();
                    standard_path |= nflags > 0;
                    args = &rest[nflags..];
                    if args.first().is_some_and(|v| v == "--") {
                        args = &args[1..];
                    }
                }
                Some((name, rest)) if name == "exec" => {
                    args = rest;
                    break;
                }
                _ => break,
            }
        }
        if cmds.len() == 1 && replaces_shell && !args.is_empty() {
            exits = true;
        }
        if args.is_empty() && cmds.len() == 1 {
            for (k, v) in assignments {
                state.assign(k, v.into(), false)?;
            }
            return Ok(0);
        }
        // A non-final builtin may fill a pipe before the following command starts.
        // Writers own a private session. The final builtin can update the parent.
        let native = args.first().is_some_and(|name| {
            super::BUILTINS.contains(&name.as_str())
                && !matches!(
                    name.as_str(),
                    "ai" | "zsh-call" | "async" | "await" | "shell" | "exec" | "builtin"
                )
        });
        if native && (cmds.len() > 1 || n > 0) {
            if last && n == 0 {
                let code = state
                    .builtin(args, &mut streams, control, tty, signal)?
                    .expect("native builtin");
                statuses[i] = code;
                continue;
            }
            let mut session = state.clone();
            for (key, value) in assignments {
                session.assign(key, value.to_owned(), true)?;
            }
            let argv = args.to_vec();
            let stage_control = if last {
                control.clone()
            } else {
                Arc::new(Control::background())
            };
            let mut run = move || -> io::Result<u8> {
                match session.builtin(&argv, &mut streams, &stage_control, false, None) {
                    Ok(Some(code)) => Ok(code),
                    Ok(None) => Err(error("invalid builtin dispatch")),
                    Err(e) => {
                        writeln!(streams.err, "zrun: {e}")?;
                        Ok(1)
                    }
                }
            };
            if last {
                statuses[i] = run()?;
            } else {
                writers.push((i, std::thread::Builder::new().spawn(run)?));
            }
            continue;
        }
        if cmds.len() == 1
            && n == 0
            && let Some(code) = state.builtin(args, &mut streams, control, tty, signal)?
        {
            return Ok(code);
        }
        let Some((name, rest)) = args.split_first() else {
            return Err(error("expected an executable in pipeline"));
        };
        if builtin_only {
            writeln!(streams.err, "builtin: {name}: not a shell builtin")?;
            return Ok(1);
        }
        // PATH lookup follows the session's exported environment, even after cd and export.
        let mut forwarded = rest;
        let mut cmd = if name == "zsh-call"
            && words
                .get(n)
                .is_some_and(|first| first != "command" && first != "exec")
        {
            let (target, args) = rest
                .split_first()
                .ok_or_else(|| error("zsh-call needs a command name"))?;
            if target.is_empty()
                || target.starts_with('-')
                || !target
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"_+.-".contains(&b))
            {
                return Err(error("invalid zsh command name"));
            }
            // Zsh expands aliases while parsing. Only the validated name is in
            // that source text. Each external argument travels separately in argv.
            let mut zsh = Command::new("zsh");
            zsh.args(["-ic", &format!("{target} \"$@\""), "zrun-zsh-call"]);
            forwarded = args;
            zsh
        } else if name == "ai"
            && words
                .get(n)
                .is_some_and(|first| first != "command" && first != "exec")
        {
            let mut this = Command::new(std::env::current_exe()?);
            this.arg("ai");
            this
        } else {
            Command::new(name)
        };
        cmd.args(forwarded)
            .current_dir(&state.cwd)
            .env_clear()
            .envs(&state.env)
            .envs(assignments)
            .stdin(Stdio::from(streams.input))
            .stdout(Stdio::from(streams.out))
            .stderr(Stdio::from(streams.err.try_clone()?));
        if standard_path {
            cmd.env("PATH", "/bin:/usr/bin");
        }
        if owns_group {
            cmd.process_group(pipeline.group);
        }
        let handoff = (tty && pipeline.children.is_empty() && state.terminal_handoff.is_none())
            .then(super::terminal_resume::Handoff::capture);
        let child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                writeln!(streams.err, "zrun: {name}: {e}")?;
                statuses[i] = missing(&e);
                continue;
            }
        };
        let first = pipeline.children.is_empty();
        if first && owns_group {
            pipeline.group = child.id() as i32;
        }
        pipeline.children.push((i, child));
        if first {
            control.active(pipeline.group)?;
            if tty {
                // SAFETY: descriptor 0 is the terminal used for this foreground pipeline.
                let owner = unsafe { libc::tcgetpgrp(0) };
                if owner > 0 {
                    if let Some(handoff) = handoff {
                        state.terminal_handoff = Some(handoff);
                    }
                    pipeline.restore = Some(owner);
                    foreground(pipeline.group);
                    signal_group(pipeline.group, libc::SIGCONT);
                }
            }
        }
    }
    for (index, c) in &mut pipeline.children {
        let exit = c.wait()?;
        if pipeline.restore.is_some()
            && exit.signal().is_some()
            && let Some(handoff) = &mut state.terminal_handoff
        {
            handoff.interrupted = true;
        }
        statuses[*index] = status(exit);
    }
    pipeline.children.clear();
    for (index, writer) in writers {
        statuses[index] = writer
            .join()
            .map_err(|_| error("builtin pipeline stage panicked"))??;
    }
    let code = if pipefail {
        statuses
            .into_iter()
            .rev()
            .find(|&code| code != 0)
            .unwrap_or(0)
    } else {
        statuses.last().copied().unwrap_or(0)
    };
    if exits {
        state.exit = Some(code);
    }
    Ok(code)
}
pub(super) fn execute(
    state: &mut State,
    program: &Program,
    streams: &Streams,
    control: &Arc<Control>,
    tty: bool,
    signal: Option<&Interrupt>,
) -> u8 {
    let mut lists = program.iter().peekable();
    while let Some((link, cmds)) = lists.next() {
        if matches!(link, Link::Success) && state.status != 0
            || matches!(link, Link::Failure) && state.status == 0
        {
            continue;
        }
        // A status inspected by && or || cannot trigger errexit, including
        // inside an eval or sourced file invoked as the inspected command.
        let previous = state.ignore_errexit;
        let negated = cmds[0].negated;
        state.ignore_errexit |=
            negated || lists.peek().is_some_and(|(link, _)| *link != Link::Always);
        state.status = match pipeline(state, cmds, streams, control, tty, signal) {
            Ok(s) => s,
            Err(e)
                if e.get_ref()
                    .is_some_and(|source| source.is::<UnboundVariable>()) =>
            {
                let _ = state.report_unbound(&e, &streams.err);
                let code = state.expansion_exit.unwrap_or(1);
                state.exit = state.expansion_exit;
                state.ignore_errexit = previous;
                state.status = code;
                return code;
            }
            Err(e) => {
                let _ = writeln!(&streams.err, "zrun: {e}");
                1
            }
        };
        if negated && state.exit.is_none() {
            state.status = u8::from(state.status == 0);
        }
        if state.errexit
            && !state.ignore_errexit
            && state.status != 0
            && state.exit.is_none()
            && state.returning.is_none()
        {
            state.exit = Some(state.status);
        }
        state.ignore_errexit = previous;
        if state.exit.is_some()
            || state.returning.is_some()
            || signal.is_some_and(|signal| {
                signal.terminate.load(std::sync::atomic::Ordering::SeqCst) != 0
            })
        {
            break;
        }
    }
    state.status
}
