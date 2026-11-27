//! Linux native interactive shell. The session owns its parser, terminal, state and jobs.
mod complete;
mod condition;
mod editor;
mod execute;
mod highlight;
mod history;
mod lookup;
mod output;
mod prompt;
pub(crate) mod rc;
mod read;
mod syntax;
mod tasks;
mod terminal_resume;
use execute::{Control, State, Streams};
use std::{
    ffi::{OsStr, OsString},
    io::{self, BufRead, IsTerminal, Read},
    os::{fd::AsRawFd, unix::net::UnixStream},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicI32, Ordering},
    },
};
use tasks::Tasks;

fn error(s: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, s.into())
}
struct Interrupt {
    stream: UnixStream,
    ids: Vec<signal_hook::SigId>,
    editing: Arc<AtomicBool>,
    terminate: Arc<AtomicI32>,
}
impl Interrupt {
    fn new() -> io::Result<Self> {
        let (r, w) = UnixStream::pair()?;
        r.set_nonblocking(true)?;
        w.set_nonblocking(true)?;
        let id = signal_hook::low_level::pipe::register(libc::SIGINT, w.try_clone()?)?;
        let mut signal = Self {
            stream: r,
            ids: vec![id],
            editing: Arc::default(),
            terminate: Arc::default(),
        };
        let wake = Arc::new(w);
        for sig in [libc::SIGHUP, libc::SIGTERM, libc::SIGQUIT] {
            let (editing, terminate, wake) = (
                signal.editing.clone(),
                signal.terminate.clone(),
                wake.clone(),
            );
            // SAFETY: the handler performs lock-free atomic operations and an async-signal-safe
            // nonblocking write on a permanently borrowed fd, or restores the signal's default.
            let id = unsafe {
                signal_hook::low_level::register(sig, move || {
                    if editing.load(Ordering::SeqCst) {
                        terminate.store(sig, Ordering::SeqCst);
                        libc::write(wake.as_raw_fd(), b"t".as_ptr().cast(), 1);
                    } else {
                        let _ = signal_hook::low_level::emulate_default_handler(sig);
                    }
                })
            }?;
            signal.ids.push(id);
        }
        Ok(signal)
    }
    fn take(&self) -> io::Result<bool> {
        let mut seen = false;
        let mut b = [0; 64];
        loop {
            match (&self.stream).read(&mut b) {
                Ok(0) => break,
                Ok(_) => seen = true,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(seen)
    }
}
impl Drop for Interrupt {
    fn drop(&mut self) {
        for id in self.ids.drain(..) {
            signal_hook::low_level::unregister(id);
        }
    }
}
enum Line {
    Text(String),
    Cancel,
    Rejected,
    Eof,
}
const BUILTINS: &[&str] = &[
    ".", "eval", "return", "source", ":", "[", "test", "ai", "alias", "async", "await", "builtin",
    "bye", "cd", "chdir", "dirs", "popd", "pushd", "command", "exec", "exit", "export", "false",
    "declare", "echo", "print", "printf", "pwd", "read", "readonly", "set", "shift", "shell",
    "typeset", "true", "type", "unalias", "unset", "whence", "where", "which", "zsh-call",
];

struct Session {
    prompt_navigation: bool,
    state: State,
    tasks: Tasks,
    io: Streams,
    control: Arc<Control>,
}
impl Session {
    fn new(args: Vec<OsString>) -> io::Result<Self> {
        let mut state = State::new()?;
        if !args.is_empty() {
            state.positional = args;
        }
        Ok(Self {
            prompt_navigation: false,
            state,
            tasks: Tasks::new(),
            io: Streams::terminal()?,
            control: Arc::new(Control::default()),
        })
    }
    fn line(&mut self, line: &str, tty: bool, signal: Option<&Interrupt>) -> io::Result<()> {
        for stmt in syntax::statements(line) {
            self.statement(stmt, tty, signal)?;
            if self.state.exit.is_some() {
                break;
            }
        }
        Ok(())
    }
    fn statement(&mut self, line: &str, tty: bool, signal: Option<&Interrupt>) -> io::Result<()> {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return Ok(());
        }
        let (word, rest) = line.split_once(char::is_whitespace).unwrap_or((line, ""));
        let rest = rest.trim_start();
        let code = match word {
            "async" => {
                let (name, body) = rest
                    .split_once(char::is_whitespace)
                    .ok_or_else(|| error("usage: async NAME -- COMMAND"))?;
                let body = body
                    .trim_start()
                    .strip_prefix("--")
                    .and_then(|s| s.strip_prefix(char::is_whitespace))
                    .ok_or_else(|| error("usage: async NAME -- COMMAND"))?;
                let program = syntax::parse(body)?;
                if program.is_empty() {
                    return Err(error("usage: async NAME -- COMMAND"));
                }
                self.tasks.start(name, &self.state, program)?;
                0
            }
            "await" => self
                .tasks
                .wait(rest, || signal.map_or(Ok(false), Interrupt::take))?,
            "shell" => {
                let body = rest
                    .strip_prefix("--")
                    .and_then(|s| s.strip_prefix(char::is_whitespace))
                    .ok_or_else(|| error("usage: shell -- COMMAND"))?;
                let cmd = syntax::Command {
                    negated: false,
                    words: [
                        crate::shell()?.to_string_lossy().to_string(),
                        "-c".into(),
                        body.into(),
                    ]
                    .map(|s| vec![syntax::Part::Text(s)])
                    .into(),
                    redirects: Vec::new(),
                };
                execute::execute(
                    &mut self.state,
                    &vec![(syntax::Link::Always, vec![cmd])],
                    &self.io,
                    &self.control,
                    tty,
                    signal,
                )
            }
            _ => {
                let program = syntax::parse(line)?;
                if self.prompt_navigation && self.state.auto_cd(line, &program)? {
                    0
                } else {
                    execute::execute(
                        &mut self.state,
                        &program,
                        &self.io,
                        &self.control,
                        tty,
                        signal,
                    )
                }
            }
        };
        self.state.status = code;
        Ok(())
    }
}
pub(super) fn interactive() -> io::Result<u8> {
    let mut session = Session::new(vec![])?;
    session.state.expansion_exit = None;
    session.startup();
    if let Some(code) = session.state.exit {
        return Ok(code);
    }
    session.prompt_navigation = true;
    let signal = Interrupt::new()?;
    let mut history = history::History::load();
    loop {
        signal.editing.store(true, Ordering::SeqCst);
        let line = editor::read_line(&signal, &session.state, &mut history);
        signal.editing.store(false, Ordering::SeqCst);
        let terminate = signal.terminate.load(Ordering::SeqCst);
        if terminate != 0 {
            signal_hook::low_level::emulate_default_handler(terminate)?;
            return Ok(128 + terminate as u8);
        }
        match line? {
            Line::Text(s) => {
                if let Err(e) = session.line(&s, true, Some(&signal)) {
                    eprintln!("zrun: {e}");
                    session.state.status = 2;
                }
            }
            Line::Cancel => session.state.status = 130,
            Line::Rejected => session.state.status = 2,
            Line::Eof => return Ok(session.state.status),
        }
        let terminate = signal.terminate.load(Ordering::SeqCst);
        if terminate != 0 {
            signal_hook::low_level::emulate_default_handler(terminate)?;
            return Ok(128 + terminate as u8);
        }
        if let Some(exit) = session.state.exit {
            return Ok(exit);
        }
        if let Some(handoff) = session.state.terminal_handoff.take() {
            handoff.before_prompt(&session.state)?;
        }
    }
}
pub(super) fn command(script: &OsStr, args: Vec<OsString>, interactive_rc: bool) -> io::Result<u8> {
    let script = script
        .to_str()
        .ok_or_else(|| error("native command is not UTF-8"))?;
    let mut session = Session::new(args)?;
    session.state.expansion_exit = Some(127);
    if interactive_rc {
        session.startup();
        if let Some(code) = session.state.exit {
            return Ok(code);
        }
    }
    for (index, line) in script.lines().enumerate() {
        session.state.line_number = index + 1;
        session.line(line, io::stdin().is_terminal(), None)?;
        if let Some(exit) = session.state.exit {
            return Ok(exit);
        }
    }
    Ok(session.state.status)
}

pub(super) fn source(input: impl BufRead, args: Vec<OsString>) -> io::Result<u8> {
    let mut session = Session::new(args)?;
    stream_lines(input, |line| {
        session.line(line, io::stdin().is_terminal(), None)?;
        session.state.line_number += 1;
        Ok(session.state.exit.is_none())
    })?;
    Ok(session.state.exit.unwrap_or(session.state.status))
}

fn stream_lines(
    mut input: impl BufRead,
    mut line_ready: impl FnMut(&str) -> io::Result<bool>,
) -> io::Result<()> {
    let mut bytes = Vec::new();
    loop {
        bytes.clear();
        // Allow both bytes of CRLF in addition to the command's byte budget.
        if (&mut input)
            .take(crate::execution::Batch::MAX_INPUT as u64 + 2)
            .read_until(b'\n', &mut bytes)?
            == 0
        {
            break;
        }
        if bytes.last() == Some(&b'\n') {
            bytes.pop();
            if bytes.last() == Some(&b'\r') {
                bytes.pop();
            }
        }
        if bytes.len() > crate::execution::Batch::MAX_INPUT {
            return Err(error("command exceeded the input limit"));
        }
        let line = std::str::from_utf8(&bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        if !line_ready(line)? {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod source_tests {
    use super::source;
    use crate::execution::Batch;
    use std::io::Cursor;

    #[test]
    fn streamed_command_limit_preserves_line_endings_and_bounds_reads() {
        let mut comment = vec![b'#'; Batch::MAX_INPUT];
        for ending in [b"\n".as_slice(), b"\r\n", b""] {
            let mut bytes = comment.clone();
            bytes.extend_from_slice(ending);
            if !ending.is_empty() {
                bytes.extend_from_slice(b"exit 7");
            }
            assert_eq!(
                source(Cursor::new(bytes), vec![]).unwrap(),
                if ending.is_empty() { 0 } else { 7 }
            );
        }
        comment.extend_from_slice(b"padding\n");
        let mut input = Cursor::new(comment);
        assert_eq!(
            source(&mut input, vec![]).unwrap_err().to_string(),
            "command exceeded the input limit"
        );
        assert_eq!(input.position(), Batch::MAX_INPUT as u64 + 2);
    }
}
