use super::{
    error,
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
    pub env: BTreeMap<OsString, OsString>,
    locals: BTreeMap<String, String>,
    pub status: u8,
    pub exit: Option<u8>,
    pub positional: Vec<OsString>,
}
impl State {
    pub fn new() -> io::Result<Self> {
        let cwd = std::env::current_dir()?;
        let mut env: BTreeMap<_, _> = std::env::vars_os().collect();
        env.insert("SHELL".into(), std::env::current_exe()?.into_os_string());
        env.insert("PWD".into(), cwd.as_os_str().into());
        Ok(Self {
            cwd,
            env,
            locals: BTreeMap::new(),
            status: 0,
            exit: None,
            positional: vec!["zrun".into()],
        })
    }
    fn lookup(&self, k: &str) -> String {
        match k {
            "?" => self.status.to_string(),
            "#" => self.positional.len().saturating_sub(1).to_string(),
            "$" => std::process::id().to_string(),
            k if k.parse::<usize>().is_ok() => self
                .positional
                .get(k.parse::<usize>().unwrap())
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default(),
            _ => self
                .locals
                .get(k)
                .cloned()
                .or_else(|| {
                    self.env
                        .get(OsStr::new(k))
                        .map(|v| v.to_string_lossy().into_owned())
                })
                .unwrap_or_default(),
        }
    }
    fn word(&self, w: &Word) -> String {
        let mut s = String::new();
        for p in w {
            match p {
                Part::Text(t) => s.push_str(t),
                Part::Var(k) => s.push_str(&self.lookup(k)),
            }
        }
        s
    }
    fn assign(&mut self, k: &str, v: String, export: bool) {
        if export || self.env.contains_key(OsStr::new(k)) {
            self.env.insert(k.into(), v.clone().into());
        }
        self.locals.insert(k.into(), v);
    }
    fn builtin(&mut self, argv: &[String], out: &mut File) -> io::Result<Option<u8>> {
        let Some((name, args)) = argv.split_first() else {
            return Ok(Some(0));
        };
        let code = match name.as_str() {
            ":" | "true" => 0,
            "false" => 1,
            "exit" => {
                if args.len() > 1 {
                    return Err(error("exit [STATUS]"));
                }
                let code = match args.first() {
                    None => self.status,
                    Some(n) => n
                        .parse::<u8>()
                        .map_err(|_| error("exit needs a status in 0..255"))?,
                };
                self.exit = Some(code);
                code
            }
            "cd" => {
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
                0
            }
            "pwd" => {
                writeln!(out, "{}", self.cwd.display())?;
                0
            }
            "export" | "unset" => {
                for entry in args {
                    let (k, v) = entry
                        .split_once('=')
                        .map_or((entry.as_str(), None), |(k, v)| (k, Some(v)));
                    if !syntax::identifier(k) || name == "unset" && v.is_some() {
                        return Err(error(format!("{name}: invalid variable name")));
                    }
                    if name == "unset" {
                        self.locals.remove(k);
                        self.env.remove(OsStr::new(k));
                    } else {
                        self.assign(
                            k,
                            v.map(str::to_owned).unwrap_or_else(|| self.lookup(k)),
                            true,
                        );
                    }
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
    children: Vec<Child>,
    group: i32,
    restore: Option<i32>,
    control: &'a Control,
}
impl Drop for Pipeline<'_> {
    fn drop(&mut self) {
        if !self.children.is_empty() {
            signal_group(self.group, libc::SIGKILL);
            for c in &mut self.children {
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
    control: &Control,
    tty: bool,
) -> io::Result<u8> {
    control.active(0)?;
    let mut pipeline = Pipeline {
        children: Vec::new(),
        group: 0,
        restore: None,
        control,
    };
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
        for (op, path) in &ast.redirects {
            streams.redirect(*op, &state.cwd.join(state.word(path)))?;
        }
        let mut words = Vec::new();
        for w in &ast.words {
            if matches!(w.as_slice(),[Part::Var(name)] if name=="@") {
                words.extend(
                    state
                        .positional
                        .iter()
                        .skip(1)
                        .map(|v| v.to_string_lossy().into_owned()),
                );
            } else {
                words.push(state.word(w));
            }
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
        let assignments = words[..n]
            .iter()
            .map(|w| w.split_once('=').expect("validated assignment"));
        let mut args = &words[n..];
        let replaces_shell = args.first().is_some_and(|s| s == "exec");
        if args.first().is_some_and(|s| s == "exec" || s == "command") {
            args = &args[1..];
        }
        if cmds.len() == 1 && replaces_shell && !args.is_empty() {
            exits = true;
        }
        if args.is_empty() && cmds.len() == 1 {
            for (k, v) in assignments {
                state.assign(k, v.into(), false);
            }
            return Ok(0);
        }
        if cmds.len() == 1
            && n == 0
            && let Some(code) = state.builtin(args, &mut streams.out)?
        {
            return Ok(code);
        }
        let Some((name, rest)) = args.split_first() else {
            return Err(error("expected an executable in pipeline"));
        };
        // PATH lookup follows the session's exported environment, even after cd and export.
        let mut cmd = Command::new(name);
        cmd.args(rest)
            .current_dir(&state.cwd)
            .env_clear()
            .envs(&state.env)
            .envs(assignments)
            .stdin(Stdio::from(streams.input))
            .stdout(Stdio::from(streams.out))
            .stderr(Stdio::from(streams.err.try_clone()?));
        if owns_group {
            cmd.process_group(pipeline.group);
        }
        let child = match cmd.spawn() {
            Ok(child) => child,
            Err(e) => {
                writeln!(streams.err, "zrun: {name}: {e}")?;
                return Ok(missing(&e));
            }
        };
        let first = pipeline.children.is_empty();
        if first && owns_group {
            pipeline.group = child.id() as i32;
        }
        pipeline.children.push(child);
        if first {
            control.active(pipeline.group)?;
            if tty {
                // SAFETY: descriptor 0 is the terminal used for this foreground pipeline.
                let owner = unsafe { libc::tcgetpgrp(0) };
                if owner > 0 {
                    pipeline.restore = Some(owner);
                    foreground(pipeline.group);
                    signal_group(pipeline.group, libc::SIGCONT);
                }
            }
        }
    }
    let mut code = 0;
    for c in &mut pipeline.children {
        code = status(c.wait()?);
    }
    pipeline.children.clear();
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
) -> u8 {
    for (link, cmds) in program {
        if matches!(link, Link::Success) && state.status != 0
            || matches!(link, Link::Failure) && state.status == 0
        {
            continue;
        }
        state.status = match pipeline(state, cmds, streams, control, tty) {
            Ok(s) => s,
            Err(e) => {
                let _ = writeln!(&streams.err, "zrun: {e}");
                1
            }
        };
        if state.exit.is_some() {
            break;
        }
    }
    state.status
}
