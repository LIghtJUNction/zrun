//! Linux native interactive shell. The session owns its parser, terminal, state and jobs.
mod execute;
mod syntax;
mod tasks;
use execute::{Control, State, Streams};
use std::{
    ffi::{OsStr, OsString},
    io::{self, BufRead, IsTerminal, Read, Write},
    os::fd::{AsRawFd, RawFd},
    os::unix::net::UnixStream,
    sync::Arc,
};
use tasks::Tasks;

fn error(s: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, s.into())
}
struct Interrupt {
    stream: UnixStream,
    id: signal_hook::SigId,
}
impl Interrupt {
    fn new() -> io::Result<Self> {
        let (r, w) = UnixStream::pair()?;
        r.set_nonblocking(true)?;
        let id = signal_hook::low_level::pipe::register(libc::SIGINT, w)?;
        Ok(Self { stream: r, id })
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
        signal_hook::low_level::unregister(self.id);
    }
}
enum Line {
    Text(String),
    Cancel,
    Eof,
}
fn read_line(signal: &Interrupt, prompt: &str) -> io::Result<Line> {
    eprint!("{prompt}");
    io::stderr().flush()?;
    let mut line = Vec::new();
    loop {
        let mut fds = [
            libc::pollfd {
                fd: 0,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: signal.stream.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: fds contains exactly two initialized pollfd values.
        let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if signal.take()? {
            eprintln!();
            return Ok(Line::Cancel);
        }
        if fds[0].revents == 0 {
            continue;
        }
        let mut byte = 0u8;
        // SAFETY: byte is one writable byte and fd 0 is only borrowed.
        let n = unsafe { libc::read(0 as RawFd, (&raw mut byte).cast(), 1) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if n == 0 && line.is_empty() {
            return Ok(Line::Eof);
        }
        if n == 0 || byte == b'\n' {
            return String::from_utf8(line)
                .map(Line::Text)
                .map_err(|_| error("command is not UTF-8"));
        }
        if line.len() == crate::execution::Batch::MAX_INPUT {
            return Err(error("command exceeded the input limit"));
        }
        line.push(byte);
    }
}

struct Session {
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
                )
            }
            _ => execute::execute(
                &mut self.state,
                &syntax::parse(line)?,
                &self.io,
                &self.control,
                tty,
            ),
        };
        self.state.status = code;
        Ok(())
    }
}
pub(super) fn interactive() -> io::Result<u8> {
    let mut session = Session::new(vec![])?;
    let signal = Interrupt::new()?;
    loop {
        match read_line(&signal, "zrun> ")? {
            Line::Text(s) => {
                if let Err(e) = session.line(&s, true, Some(&signal)) {
                    eprintln!("zrun: {e}");
                    session.state.status = 2;
                }
            }
            Line::Cancel => session.state.status = 130,
            Line::Eof => return Ok(session.state.status),
        }
        if let Some(exit) = session.state.exit {
            return Ok(exit);
        }
    }
}
pub(super) fn command(script: &OsStr, args: Vec<OsString>) -> io::Result<u8> {
    let script = script
        .to_str()
        .ok_or_else(|| error("native command is not UTF-8"))?;
    let mut session = Session::new(args)?;
    for line in script.lines() {
        session.line(line, io::stdin().is_terminal(), None)?;
        if let Some(exit) = session.state.exit {
            return Ok(exit);
        }
    }
    Ok(session.state.status)
}

pub(super) fn source(input: impl BufRead, args: Vec<OsString>) -> io::Result<u8> {
    let mut session = Session::new(args)?;
    for line in input.lines() {
        let line = line?;
        if line.len() > crate::execution::Batch::MAX_INPUT {
            return Err(error("command exceeded the input limit"));
        }
        session.line(&line, io::stdin().is_terminal(), None)?;
        if let Some(code) = session.state.exit {
            return Ok(code);
        }
    }
    Ok(session.state.status)
}
